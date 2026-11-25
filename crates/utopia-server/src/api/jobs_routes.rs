//! Failed jobs go back onto the queue (#216).
//!
//! A failed job could previously only be re-run through the object it belonged to: documents could be re-extracted and sources re-synced, but `bootstrap_ontology`,
//! `adjudicate_entities` had no object to click. Running out of balance (#201 makes the first attempt fail) halted a whole batch of documents,
//! and each had to be clicked again after topping up. Two entry points here: within a library (Editor) and global (admin); the scope can be narrowed by
//! kind and failure time: the "run again" on an alert passes the time window of that very failure.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use utopia_store::jobs::RequeueScope;
use uuid::Uuid;

use super::graph_routes::require_kb;
use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::state::AppState;

// Both API scopes must apply the same retry policies and report the combined total.
async fn requeue_failed(
    pool: &sqlx::PgPool,
    scope: RequeueScope<'_>,
) -> utopia_core::AppResult<u64> {
    let generic = utopia_store::jobs::requeue_failed(pool, scope).await?;
    let rss = utopia_store::rss_full_content::requeue_failed(pool, scope).await?;
    Ok(generic + rss)
}

#[cfg(test)]
#[path = "jobs_routes_tests.rs"]
mod tests;

#[derive(Deserialize, Default)]
pub struct RequeueBody {
    #[serde(default)]
    pub kind: Option<String>,
    /// Only queue jobs that failed after this moment
    #[serde(default)]
    pub failed_since: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn failed_in_kb(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let failed = utopia_store::jobs::failed_count(&state.pool, Some(kb_id)).await?;
    Ok(Json(json!({ "failed": failed })))
}

/// Whether a job has finished (0051). When a person sets a short-phrase signature they get a job id rather than a result;
/// the result is either awaited from the `review` / `graph` events, or asked here. A Viewer can ask:
/// it answers only if the job belongs to this library; otherwise 404, the same convention as for things invisible in the library
pub async fn job_in_kb(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, job_id)): Path<(Uuid, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let job = utopia_store::jobs::status_in_kb(&state.pool, kb_id, job_id)
        .await?
        .ok_or(utopia_core::AppError::NotFound)?;
    Ok(Json(json!({ "job": job })))
}

pub async fn requeue_in_kb(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(body): Json<RequeueBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let requeued = requeue_failed(
        &state.pool,
        RequeueScope {
            kb_id: Some(kb_id),
            kind: body.kind.as_deref(),
            failed_since: body.failed_since,
        },
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "jobs.requeued",
        "kb",
        Some(kb_id),
        json!({ "requeued": requeued, "kind": body.kind, "failed_since": body.failed_since }),
    )
    .await;
    Ok(Json(json!({ "requeued": requeued })))
}

/// Global re-queue: system-level alerts (those with no library) go through here. Admin only: it touches jobs across all libraries
pub async fn requeue_all(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<RequeueBody>,
) -> ApiResult<Json<serde_json::Value>> {
    if !user.is_admin {
        return Err(utopia_core::AppError::Forbidden.into());
    }
    let requeued = requeue_failed(
        &state.pool,
        RequeueScope {
            kb_id: None,
            kind: body.kind.as_deref(),
            failed_since: body.failed_since,
        },
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "jobs.requeued",
        "system",
        None,
        json!({ "requeued": requeued, "kind": body.kind, "failed_since": body.failed_since }),
    )
    .await;
    Ok(Json(json!({ "requeued": requeued })))
}
