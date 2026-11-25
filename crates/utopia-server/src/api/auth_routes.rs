use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::User;
use utopia_core::AppError;

use crate::auth::{self, AuthUser};
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct RegisterReq {
    pub email: String,
    pub password: String,
    pub display_name: String,
    pub org_name: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginReq {
    pub email: String,
    pub password: String,
}

fn validate_register(req: &RegisterReq) -> Result<(), AppError> {
    if !req.email.contains('@') || req.email.len() > 254 {
        return Err(AppError::invalid("bad_email", "Invalid email address"));
    }
    if req.password.chars().count() < 8 {
        return Err(AppError::invalid(
            "password_too_short",
            "Password must be at least 8 characters",
        ));
    }
    if req.display_name.trim().is_empty() || req.display_name.chars().count() > 64 {
        return Err(AppError::invalid(
            "bad_display_name",
            "Display name must be 1-64 characters",
        ));
    }
    Ok(())
}

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<RegisterReq>,
) -> ApiResult<(CookieJar, Json<serde_json::Value>)> {
    password_login_enabled(&state)?;
    validate_register(&req)?;
    let hash = auth::hash_password(&req.password)?;
    let org_name = req
        .org_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Registration switch: the in-database setting (toggled at /admin) takes precedence; before the database exists (first-user bootstrap) it is always allowed
    let open = utopia_store::access::open_registration(&state.pool)
        .await
        .unwrap_or(state.open_registration);
    let utopia_store::accounts::Registered {
        user,
        workspace,
        general_kb: _,
    } = utopia_store::accounts::register(
        &state.pool,
        req.email.trim(),
        &hash,
        req.display_name.trim(),
        org_name,
        open,
    )
    .await?;

    // The first user's General database **starts empty** (#580). It used to install schema.org here (#322),
    // the reason being that without domain/range facts there is no direction; measured afterwards (the #580 comparison): the package supplies entity
    // types, not predicates -- even when installed, most facts still have no predicates; an empty database grows its own classes from documents via bootstrap
    // a dozen-odd classes and over a hundred relations, all in the corpus's own words, and each block's prompt drops from 18k tokens to 2k.
    // The package is still in the create-database dialog; anyone who wants it can install it with one click

    let token = auth::issue_token(&state, user.id)?;
    let secure = auth::behind_tls(&headers, state.cookie_secure);
    let jar = jar.add(auth::auth_cookie(token.clone(), secure));
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "auth.register",
        "user",
        Some(user.id),
        json!({ "email": user.email, "is_admin": user.is_admin }),
    )
    .await;
    Ok((
        jar,
        Json(json!({ "user": user, "workspace": workspace, "token": token })),
    ))
}

/// Failed logins are recorded: there is no account to attribute them to, so actor is NULL and the attempted email goes into detail.
/// Distinguishes "email does not exist" from "wrong password": many of the former from one IP is email enumeration,
/// while many of the latter against one account is credential stuffing; the two attacks look different. The ledger is readable by admins only,
/// so there is no way to use it to probe whether an account is registered.
async fn record_login_failure(state: &AppState, email: &str, reason: &str) {
    let _ = utopia_store::audit::record_opt(
        &state.pool,
        None,
        None,
        "auth.login_failed",
        "user",
        None,
        json!({ "email": email, "reason": reason }),
    )
    .await;
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(req): Json<LoginReq>,
) -> ApiResult<(CookieJar, Json<serde_json::Value>)> {
    password_login_enabled(&state)?;
    let email = req.email.trim();
    let user = utopia_store::accounts::find_user_by_email(&state.pool, email).await?;
    // Whether an email exists must take the same code path: without the argon2 branch, response-time differences would reveal which emails are registered
    // (after going through a password dump, whoever is left able to log in is a real user).
    // So the "email does not exist" branch runs one argon2 check and discards the result --
    // this branch is therefore just as slow as the "email exists, wrong password" branch.
    let password_valid = match &user {
        Some(u) => auth::verify_password(&req.password, &u.password_hash),
        None => {
            let _ = auth::verify_password(&req.password, auth::dummy_password_hash());
            false
        }
    };
    if !password_valid {
        let reason = if user.is_some() {
            "bad_password"
        } else {
            "unknown_email"
        };
        record_login_failure(&state, email, reason).await;
        return Err(AppError::Unauthorized.into());
    }
    let Some(user) = user else {
        unreachable!("password_valid is only true for an existing user")
    };
    let token = auth::issue_token(&state, user.id)?;
    let secure = auth::behind_tls(&headers, state.cookie_secure);
    let jar = jar.add(auth::auth_cookie(token.clone(), secure));
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "auth.login",
        "user",
        Some(user.id),
        json!({}),
    )
    .await;
    Ok((jar, Json(json!({ "user": user, "token": token }))))
}

/// Logout does not require a valid session: the cookie must be clearable even after it expires, otherwise the frontend gets stuck on a
/// dead session. So this parses the token itself to get identity, and if it cannot be parsed it only clears the cookie without leaving a record.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> (CookieJar, Json<serde_json::Value>) {
    if let Some(user_id) = jar
        .get(auth::COOKIE_NAME)
        .and_then(|c| auth::decode_user_id(&state, c.value()).ok())
    {
        let _ = utopia_store::audit::record(
            &state.pool,
            None,
            user_id,
            "auth.logout",
            "user",
            Some(user_id),
            json!({}),
        )
        .await;
    }
    let secure = auth::behind_tls(&headers, state.cookie_secure);
    let jar = jar.remove(auth::clear_auth_cookie(secure));
    (jar, Json(json!({ "ok": true })))
}

pub async fn me(AuthUser(user): AuthUser) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
pub struct UpdateMeReq {
    pub display_name: String,
}

/// Profile: change display name.
pub async fn update_me(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<UpdateMeReq>,
) -> ApiResult<Json<User>> {
    let name = req.display_name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(
            AppError::invalid("bad_display_name", "Display name must be 1-64 characters").into(),
        );
    }
    let updated = utopia_store::accounts::update_display_name(&state.pool, user.id, name).await?;
    Ok(Json(updated))
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub current_password: String,
    pub new_password: String,
}

/// Change password: verify the old password -> hash the new one. The wrong old password is distinguished from "not logged in", so the frontend can show an accurate message.
pub async fn change_password(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if !auth::verify_password(&req.current_password, &user.password_hash) {
        return Err(AppError::invalid("wrong_password", "Current password is incorrect").into());
    }
    if req.new_password.chars().count() < 8 {
        return Err(AppError::invalid(
            "password_too_short",
            "Password must be at least 8 characters",
        )
        .into());
    }
    let hash = auth::hash_password(&req.new_password)?;
    utopia_store::accounts::update_password(&state.pool, user.id, &hash).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "auth.password_changed",
        "user",
        Some(user.id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

fn password_login_enabled(state: &AppState) -> Result<(), AppError> {
    if super::oidc_routes::password_login(state) {
        Ok(())
    } else {
        Err(AppError::invalid(
            "password_login_disabled",
            "Password sign-in is turned off on this deployment; sign in with SSO",
        ))
    }
}
