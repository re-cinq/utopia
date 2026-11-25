//! System admin API (system admins only): deployment config plus creating accounts on behalf of users.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::{Role, User};
use utopia_core::AppError;

use crate::auth::{self, AuthUser};
use crate::error::ApiResult;
use crate::state::AppState;
use uuid::Uuid;

fn require_admin(user: &User) -> Result<(), AppError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

pub async fn get_deployment(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    let open = utopia_store::access::open_registration(&state.pool).await?;
    let workers = utopia_store::access::worker_concurrency(&state.pool).await?;
    let onto_lang = utopia_store::access::default_ontology_lang(&state.pool).await?;
    let (limits, dflt) = utopia_store::model_limits::list(&state.pool).await?;
    let in_use = utopia_store::model_limits::models_in_use(&state.pool).await?;
    Ok(Json(json!({
        "open_registration": open,
        "worker_concurrency": workers,
        "default_ontology_lang": onto_lang,
        "model_limits": limits,
        "default_model_concurrency": dflt,
        "models_in_use": in_use.into_iter().map(|(b, m, k)| json!({"base_url": b, "model": m, "kind": k})).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct DeploymentReq {
    pub open_registration: bool,
    /// Task worker concurrency (1-256): governs model tasks. A further `jobs::NO_CHAT_LANE` slots are reserved for ingestion tasks.
    /// It is an **outer safety net** that stops tasks from piling up without limit. The real throttling is the per-model limit, and this value should be clearly larger than the sum of the per-model limits
    #[serde(default)]
    pub worker_concurrency: Option<i32>,
    /// Concurrency default for models without their own setting
    #[serde(default)]
    pub default_model_concurrency: Option<i32>,
    /// Concurrency for a single model. A null `max_concurrent` deletes the dedicated setting and falls back to the default
    #[serde(default)]
    pub model_limit: Option<ModelLimitReq>,
    /// Language the ontology is seeded in when a new knowledge base is created. **This is not the UI language**; that one is set on the client
    #[serde(default)]
    pub default_ontology_lang: Option<String>,
}

#[derive(Deserialize)]
pub struct ModelLimitReq {
    pub base_url: String,
    pub model: String,
    pub max_concurrent: Option<i32>,
}

pub async fn put_deployment(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<DeploymentReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    utopia_store::access::set_open_registration(&state.pool, req.open_registration).await?;
    if let Some(n) = req.worker_concurrency {
        utopia_store::access::set_worker_concurrency(&state.pool, n).await?;
        // Takes effect immediately: the scheduling loop reads this value on every round, no restart needed
        state
            .worker_concurrency
            .store(n as usize, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(l) = &req.default_ontology_lang {
        utopia_store::access::set_default_ontology_lang(&state.pool, l).await?;
    }
    let limits_changed = req.default_model_concurrency.is_some() || req.model_limit.is_some();
    if let Some(n) = req.default_model_concurrency {
        utopia_store::model_limits::set_default(&state.pool, n).await?;
    }
    if let Some(m) = req.model_limit {
        utopia_store::model_limits::set(&state.pool, &m.base_url, &m.model, m.max_concurrent)
            .await?;
    }
    // The per-model limit takes effect in place here: a larger gate lets more through, a smaller one tightens it, without waiting for the next task to take a permit to read it
    // -- by then every worker may be queued at the gate, and the only thing in flight is one repeat (#1302). The limit is already stored, so
    // If adjusting the gate fails, only log it once: the next permit acquisition still reads the new limit
    if limits_changed {
        if let Err(e) = state.model_gates.refresh(&state.pool).await {
            tracing::warn!(error = %e, "限额已保存，闸门要等下一次取许可才按新限额调");
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct CreateUserReq {
    pub email: String,
    pub display_name: String,
    pub password: String,
    /// Deployment role: admin | editor | viewer
    #[serde(default)]
    pub role: Option<String>,
}

/// Admin creates an account on someone's behalf (the only entry point once registration is closed).
pub async fn create_user(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<CreateUserReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    if !req.email.contains('@') || req.email.len() > 254 {
        return Err(AppError::invalid("bad_email", "Invalid email address").into());
    }
    if req.password.chars().count() < 8 {
        return Err(AppError::invalid(
            "password_too_short",
            "Password must be at least 8 characters",
        )
        .into());
    }
    if req.display_name.trim().is_empty() || req.display_name.chars().count() > 64 {
        return Err(
            AppError::invalid("bad_display_name", "Display name must be 1-64 characters").into(),
        );
    }
    let role = match req.role.as_deref().unwrap_or("editor") {
        "admin" => Role::Admin,
        "editor" => Role::Editor,
        "viewer" => Role::Viewer,
        _ => {
            return Err(AppError::Validation("role must be admin, editor or viewer".into()).into())
        }
    };
    let hash = auth::hash_password(&req.password)?;
    let created = utopia_store::accounts::admin_create_user(
        &state.pool,
        req.email.trim(),
        &hash,
        req.display_name.trim(),
        role,
    )
    .await?;
    Ok(Json(json!({ "user": created })))
}

/// Deactivate an account (soft delete, see `users.deactivated_at`).
///
/// **Not DELETE.** Audit events, merge logs, ledger edits and fact confirmations all point at
/// this person through `actor_id`. Those are audit records: even after someone leaves, it must still be possible to answer "who did it at the time". Deactivation only cuts off access:
/// Login cannot find the person, and an issued token fails on its next request too (session checks use the same function),
/// they no longer appear in the member list, but all attribution stays as it is.
///
/// The two guardrails live in the store layer rather than here: you cannot deactivate yourself, and you cannot deactivate the last admin.
/// They sit here because the UI can stop an accidental click but not a direct API call.
pub async fn deactivate_user(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(target): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    utopia_store::accounts::deactivate_user(&state.pool, target, user.id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "user.deactivated",
        "user",
        Some(target),
        serde_json::json!({}),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Reactivate a deactivated account.
///
/// **May fail**: while deactivated, someone may have created a new account with the same email, and that partial unique index
/// (`users_email_active_idx`) blocks the restore. Showing the admin the conflict is better than silently letting two
/// active accounts share one email.
pub async fn reactivate_user(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(target): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    utopia_store::accounts::reactivate_user(&state.pool, target).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "user.reactivated",
        "user",
        Some(target),
        serde_json::json!({}),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct SetSystemAdminReq {
    pub is_admin: bool,
}

/// Grant or revoke system admin. Apart from the first person to register, this is the only way to obtain it (#1233).
pub async fn set_system_admin(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(target): Path<Uuid>,
    Json(req): Json<SetSystemAdminReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    utopia_store::accounts::set_system_admin(&state.pool, target, user.id, req.is_admin).await?;
    let action = if req.is_admin {
        "user.admin_granted"
    } else {
        "user.admin_revoked"
    };
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        action,
        "user",
        Some(target),
        serde_json::json!({}),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
