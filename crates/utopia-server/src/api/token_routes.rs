//! Issuing and revoking personal access tokens (0014).
//!
//! **Uses the account-level route, not the knowledge-base level**: a token belongs to a person, and a person can enter several KBs.
//! Which KBs a token is limited to is decided by its own `kb_ids`. That narrows access; it does not grant it.

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct IssueReq {
    pub name: String,
    /// read | write. Defaults to read-only: to let an agent write to the ledger you must tick it explicitly
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Default = every KB this person can enter
    #[serde(default)]
    pub kb_ids: Option<Vec<Uuid>>,
    /// How many days until it expires. Default 90 days; an explicit 0 means it never expires
    #[serde(default = "default_days")]
    pub expires_in_days: i64,
}
fn default_scope() -> String {
    "read".into()
}
/// 90 days. **Never expiring is available as an option, but it is not the default**: a key set up on someone else's laptop
/// is often forgotten about
fn default_days() -> i64 {
    90
}

fn token_expires_at(now: DateTime<Utc>, expires_in_days: i64) -> AppResult<Option<DateTime<Utc>>> {
    if expires_in_days < 0 {
        return Err(AppError::invalid(
            "bad_token_expiry",
            "expires_in_days must be non-negative",
        ));
    }
    if expires_in_days == 0 {
        return Ok(None);
    }

    let duration = Duration::try_days(expires_in_days).ok_or_else(|| {
        AppError::invalid(
            "bad_token_expiry",
            "Token expiration duration is out of range",
        )
    })?;
    let expires_at = now.checked_add_signed(duration).ok_or_else(|| {
        AppError::invalid("bad_token_expiry", "Token expiration date is out of range")
    })?;
    Ok(Some(expires_at))
}

/// Issue one. **The plaintext appears only in this one response**; afterwards the store holds only the hash.
pub async fn issue(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(req): Json<IssueReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let expires_at = token_expires_at(Utc::now(), req.expires_in_days)?;
    let (view, plain) = utopia_store::tokens::issue(
        &state.pool,
        user.id,
        &req.name,
        &req.scope,
        req.kb_ids.as_deref(),
        expires_at,
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "token.issued",
        "personal_token",
        Some(view.id),
        json!({ "name": view.name, "scope": view.scope }),
    )
    .await;
    // The `token` field appears only here. The list endpoint can never return it
    Ok(Json(json!({ "token": plain, "info": view })))
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let tokens = utopia_store::tokens::list(&state.pool, user.id).await?;
    Ok(Json(json!({ "tokens": tokens })))
}

pub async fn revoke(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(token_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::tokens::revoke(&state.pool, user.id, token_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "token.revoked",
        "personal_token",
        Some(token_id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
#[path = "token_routes_tests.rs"]
mod tests;
