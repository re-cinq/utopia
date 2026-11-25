//! Google Drive source connection and folder selection (0074).
//!
//! **The connection is made by the source's editor himself.** He clicks "Connect" in the source settings; we record one flow
//! (state, PKCE verifier, who, which source), and send him to Google's consent page; on callback the
//! browser session must still be his, and he must still be an Editor of that database. Without this check, someone could
//! start a flow themselves, then trick someone else into finishing consent -- that person's Drive would be hooked into the source.
//!
//! **Once connected, only the person who connected chooses the folders.** The reads come from his Drive, and anyone in the database can see what is synced in:
//! if other editors could browse or pick, they could pull into the database any folder he never meant to hand over. So the callback records
//! `connected_by`, and browsing and selecting honor only this person; if he is no longer an editor, sync stops and asks someone to reconnect.
//!
//! The refresh token goes in but never out: it is stored in the source config as `refresh_token` (sealed), and the folder browser's requests
//! exchange it on the server for an access token, so the browser never receives any Google token.

use axum::extract::{Path, Query, State};
use axum::response::Html;
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use utopia_core::models::{Role, Source, SourceKind};
use utopia_core::AppError;
use uuid::Uuid;

use super::graph_routes::require_kb;
use crate::auth::{self, AuthUser};
use crate::error::{ApiErr, ApiResult};
use crate::google_drive::{self, Drive, FolderEntry, OAuthConfig};
use crate::state::AppState;

/// How long a connection flow lives: if he has not finished on the consent page within ten minutes, it is abandoned
const FLOW_TTL_SECS: u64 = 600;
/// Cap on concurrent in-flight connection flows, same reasoning as SSO: every `connect` writes a row
const MAX_PENDING_FLOWS: i64 = 1000;
/// Cap on in-flight flows per person: without it, one editor clicking repeatedly could fill the total above, and nobody could connect for ten minutes
const MAX_PENDING_FLOWS_PER_USER: i64 = 5;
/// The connection is recorded in a few keys in the config. Written only by the connect and disconnect routes; disconnect deletes them together with the folders,
/// so the next person to connect starts selecting from scratch
const CONNECTION_KEYS: &[&str] = &[
    "refresh_token",
    "account_email",
    "connected_at",
    "connected_by",
    "connected_by_name",
    "folders",
];
/// Maximum number of folders one source can select. For more, select their common ancestor instead.
const MAX_FOLDERS: usize = 50;

fn configured() -> Result<OAuthConfig, AppError> {
    google_drive::oauth_config().ok_or_else(|| {
        AppError::invalid(
            "google_drive_not_configured",
            "Google Drive is not configured on this server",
        )
    })
}

/// Load a Google Drive source and confirm it belongs to the database on this path
async fn drive_source(state: &AppState, kb_id: Uuid, source_id: Uuid) -> Result<Source, AppError> {
    let source = utopia_store::sources::get(&state.pool, source_id).await?;
    if source.kb_id != kb_id || SourceKind::parse(&source.kind) != Some(SourceKind::GoogleDrive) {
        return Err(AppError::NotFound);
    }
    Ok(source)
}

/// Whether the Drive this source is connected to is this person's: browsing and folder selection honor only him (0074 decision 5)
fn require_connector(source: &Source, user_id: Uuid) -> Result<(), AppError> {
    if google_drive::connected_by(&source.config) == Some(user_id) {
        return Ok(());
    }
    Err(AppError::CodedForbidden {
        code: "google_drive_not_connector",
        message: "Only the person who connected this Google Drive can choose its folders".into(),
    })
}

/// A source connected by someone else cannot be reconnected directly: one click would swap out his Drive and drop the folders he selected.
/// To change the person, disconnect first (the disconnect asks, stating whose Drive it is), then the new person connects
fn refuse_someone_elses_connection(source: &Source, user_id: Uuid) -> Result<(), AppError> {
    match google_drive::connected_by(&source.config) {
        Some(connector) if connector != user_id => Err(AppError::CodedForbidden {
            code: "google_drive_connected_by_someone_else",
            message: "This source reads someone else's Google Drive; disconnect it first".into(),
        }),
        _ => Ok(()),
    }
}

/// Whether the same Google account is still connected to other sources. Google revokes a refresh token by revoking the person's whole grant to our
/// client, so his other Drive sources would fail together; revoke only if something else is still using it
pub(super) async fn grant_still_used(
    pool: &sqlx::PgPool,
    source_id: Uuid,
    account_email: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sources
          WHERE kind = 'google_drive' AND id <> $1
            AND lower(config->>'account_email') = lower($2) AND config ? 'refresh_token')",
    )
    .bind(source_id)
    .bind(account_email)
    .fetch_one(pool)
    .await
}

/// Whether the Google account is still the same one after reconnecting. Emails are case-insensitive; if either side is unknown, treat it as changed:
/// on a changed account, the folders selected under the previous account must not be read with the new account
pub(super) fn same_account(previous: Option<&str>, now: Option<&str>) -> bool {
    matches!((previous, now), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
}

/// Whether to revoke the previous grant after reconnecting: only when both accounts are known and differ.
/// If either side is unknown it may be the same account -- revoking would also invalidate the new token just obtained (0074 decision 5)
pub(super) fn revokes_previous(previous: Option<&str>, now: Option<&str>) -> bool {
    previous.is_some() && now.is_some() && !same_account(previous, now)
}

/// Ask Google to revoke a refresh token, unless the same account is still connected to another source ([`grant_still_used`]).
/// If the account is not known, do not revoke. A failure is logged once: the database no longer has it
async fn revoke_unless_still_used(
    pool: &sqlx::PgPool,
    source_id: Uuid,
    token: &str,
    account: Option<&str>,
) -> Result<(), sqlx::Error> {
    let Some(account) = account else {
        return Ok(());
    };
    if grant_still_used(pool, source_id, account).await? {
        return Ok(());
    }
    let revoked = reqwest::Client::new()
        .post("https://oauth2.googleapis.com/revoke")
        .form(&[("token", token)])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    match revoked {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::warn!(status = %resp.status(), "Google 拒绝作废刷新令牌，库里已删除"),
        Err(e) => tracing::warn!(error = %e, "Google 刷新令牌作废失败，库里已删除"),
    }
    Ok(())
}

/// Record one connection flow. The total and this person's in-flight count are both checked in the same insert; when full, nothing is inserted and `None` is returned
pub(super) async fn record_flow(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    source_id: Uuid,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query("DELETE FROM google_drive_flows WHERE expires_at < now()")
        .execute(pool)
        .await?;
    let flow_state = auth::generate_jwt_secret();
    let verifier = auth::generate_jwt_secret();
    let inserted = sqlx::query(
        "INSERT INTO google_drive_flows (state, verifier, user_id, source_id, expires_at)
         SELECT $1, $2, $3, $4, now() + make_interval(secs => $5)
         WHERE (SELECT count(*) FROM google_drive_flows WHERE user_id = $3) < $6
           AND (SELECT count(*) FROM google_drive_flows) < $7",
    )
    .bind(&flow_state)
    .bind(&verifier)
    .bind(user_id)
    .bind(source_id)
    .bind(FLOW_TTL_SECS as f64)
    .bind(MAX_PENDING_FLOWS_PER_USER)
    .bind(MAX_PENDING_FLOWS)
    .execute(pool)
    .await?;
    Ok((inserted.rows_affected() == 1).then_some((flow_state, verifier)))
}

/// The UI decides from this whether to show a connect button for this kind of source
pub async fn status(AuthUser(_): AuthUser) -> Json<Value> {
    Json(json!({ "enabled": google_drive::oauth_config().is_some() }))
}

/// Start a connection: return the UI a Google consent-page address, which the UI opens in a popup
pub async fn connect(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, source_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let source = drive_source(&state, kb_id, source_id).await?;
    refuse_someone_elses_connection(&source, user.id)?;
    let cfg = configured()?;
    let (flow_state, verifier) = record_flow(&state.pool, user.id, source_id)
        .await?
        .ok_or_else(|| {
            AppError::invalid(
                "google_drive_busy",
                "Too many Google Drive connections are in progress; try again in a few minutes",
            )
        })?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

    let mut url =
        reqwest::Url::parse(google_drive::AUTHORIZE_URL).map_err(|e| AppError::Other(e.into()))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", cfg.client_id.as_str()),
        ("redirect_uri", cfg.redirect.as_str()),
        ("scope", google_drive::SCOPES),
        ("state", flow_state.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        // The refresh token is guaranteed only with offline + consent requested every time: reconnecting a source whose grant was revoked behaves the same way
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("include_granted_scopes", "false"),
    ]);
    Ok(Json(json!({ "url": url.as_str() })))
}

#[derive(Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
    /// The person clicked cancel on the consent page
    error: Option<String>,
}

/// The callback lands in the popup: on success and failure alike it returns a small page without scripts. The UI side polls the source,
/// and closes the popup once `account_email` appears
pub async fn callback(
    State(state): State<AppState>,
    user: Result<AuthUser, ApiErr>,
    Query(q): Query<Callback>,
) -> Html<String> {
    match finish(&state, user.ok().map(|AuthUser(u)| u), q).await {
        Ok(()) => page(
            "Google Drive connected",
            "You can close this window and choose the folders to sync.",
        ),
        Err((code, reason)) => {
            tracing::warn!(code, reason = %reason, "Google Drive 连接被拒");
            page(
                "Google Drive was not connected",
                match code {
                    "cancelled" => "The consent screen was cancelled. Close this window and try again.",
                    "session" => "This window is not signed in to Utopia as the person who started the connection. Close it and start again from the source settings.",
                    "expired" => "This connection attempt has expired. Close this window and start again.",
                    "scope" => "Google did not grant read access to Drive. Close this window, connect again and allow it.",
                    "taken" => "Someone else connected this source while you were on Google's page. Close this window.",
                    _ => "Google could not be reached or refused the request. Close this window and try again.",
                },
            )
        }
    }
}

pub(super) async fn finish(
    state: &AppState,
    user: Option<utopia_core::models::User>,
    q: Callback,
) -> Result<(), (&'static str, String)> {
    let db = |e: sqlx::Error| ("unavailable", format!("database: {e}"));
    let (Some(code), Some(flow_state)) = (q.code, q.state) else {
        return Err(match q.error {
            Some(e) => ("cancelled", format!("google returned {e}")),
            None => ("invalid", "callback without code or state".into()),
        });
    };
    // One-shot: delete first, then use; a second arrival of the same state gets nothing back
    let flow: Option<(String, Uuid, Uuid)> = sqlx::query_as(
        "DELETE FROM google_drive_flows WHERE state = $1 AND expires_at > now()
         RETURNING verifier, user_id, source_id",
    )
    .bind(&flow_state)
    .fetch_optional(&state.pool)
    .await
    .map_err(db)?;
    let (verifier, flow_user, source_id) =
        flow.ok_or(("expired", "unknown or expired state".to_string()))?;
    if let Some(e) = q.error {
        return Err(("cancelled", format!("google returned {e}")));
    }
    let user = user.filter(|u| u.id == flow_user).ok_or((
        "session",
        "callback session is not the user who started it".to_string(),
    ))?;
    let source = utopia_store::sources::get(&state.pool, source_id)
        .await
        .map_err(|e| ("invalid", format!("source: {e}")))?;
    // In the minutes since initiation, the permission may have been revoked
    require_kb(state, &user, source.kb_id, Role::Editor)
        .await
        .map_err(|_| {
            (
                "session",
                "user is no longer an editor of the knowledge base".to_string(),
            )
        })?;
    // In the minutes on the consent page, someone else may have connected first
    refuse_someone_elses_connection(&source, user.id).map_err(|_| {
        (
            "taken",
            "someone else connected this source in the meantime".to_string(),
        )
    })?;

    let cfg = google_drive::oauth_config().ok_or(("unavailable", "not configured".to_string()))?;
    let grant = google_drive::exchange_code(&cfg, &code, &verifier)
        .await
        .map_err(|e| {
            let msg = format!("{e:#}");
            (
                if msg.contains("read access to Drive") {
                    "scope"
                } else {
                    "unavailable"
                },
                msg,
            )
        })?;

    let previous_token = source.config["refresh_token"].as_str();
    let previous_account = source.config["account_email"].as_str();
    // The person connecting switched to another Google account: folders selected under the previous account are dropped, and its grant is revoked if nobody else is using it.
    // The same account is never revoked -- Google revokes the whole grant, and the newly obtained token would be invalidated with it
    let changed = !same_account(previous_account, grant.account_email.as_deref());
    let grant_keys = json!({
        "refresh_token": grant.refresh_token,
        "account_email": grant.account_email,
        // The token is not visible in the list; the UI knows "connected" from this field: unverified emails are not shown and must not be used as the flag
        "connected_at": chrono::Utc::now().to_rfc3339(),
        "connected_by": user.id,
        // When asking before disconnecting, state whose Drive it is; the database's member list is admin-only, so editors cannot look up the name
        "connected_by_name": user.display_name,
    });
    let dropped: &[&str] = if changed { &["folders"] } else { &[] };
    utopia_store::sources::patch_config(&state.pool, source_id, &grant_keys, dropped)
        .await
        .map_err(|e| ("unavailable", format!("saving the grant: {e}")))?;
    let revoke = revokes_previous(previous_account, grant.account_email.as_deref());
    if let (true, Some(token)) = (revoke, previous_token) {
        revoke_unless_still_used(&state.pool, source_id, token, previous_account)
            .await
            .map_err(|e| ("unavailable", format!("checking the previous grant: {e}")))?;
    }
    state.emit_source(source.kb_id);
    // Audit does not store tokens; it records only which Google account was connected
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(source.kb_id),
        user.id,
        "source.google_drive_connected",
        "source",
        Some(source_id),
        json!({ "account_email": grant.account_email }),
    )
    .await;
    Ok(())
}

/// Disconnect: delete the refresh token and selected folders, then ask Google to revoke via [`revoke_unless_still_used`]
pub async fn disconnect(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, source_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let source = drive_source(&state, kb_id, source_id).await?;
    utopia_store::sources::patch_config(&state.pool, source_id, &json!({}), CONNECTION_KEYS)
        .await?;
    if let Some(token) = source.config["refresh_token"].as_str() {
        revoke_unless_still_used(
            &state.pool,
            source_id,
            token,
            source.config["account_email"].as_str(),
        )
        .await?;
    }
    state.emit_source(kb_id);
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "source.google_drive_disconnected",
        "source",
        Some(source_id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct FoldersQuery {
    /// No value given = top level (My Drive, shared with me, each shared drive)
    parent: Option<String>,
}

/// Folder browser: lists subfolders as the person who connected, visible only to him
pub async fn folders(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, source_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<FoldersQuery>,
) -> ApiResult<Json<Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let source = drive_source(&state, kb_id, source_id).await?;
    require_connector(&source, user.id)?;
    let cfg = configured()?;
    let refresh = source.config["refresh_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            AppError::invalid(
                "google_drive_not_connected",
                "Connect a Google account first",
            )
        })?;
    let unavailable = |e: anyhow::Error| {
        AppError::invalid_detail(
            "google_drive_unavailable",
            "Google Drive could not be read",
            format!("{e:#}"),
        )
    };
    let drive = Drive::connect(&cfg, refresh).await.map_err(unavailable)?;
    let folders: Vec<FolderEntry> = match q.parent.as_deref().filter(|p| !p.is_empty()) {
        None => drive.roots().await,
        Some(parent) => drive.child_folders(parent).await,
    }
    .map_err(unavailable)?;
    Ok(Json(json!({ "folders": folders })))
}

#[derive(Deserialize)]
pub struct SelectBody {
    folders: Vec<FolderEntry>,
}

/// Save the selected folders and sync once immediately. Only the person who connected can select. When the folder changes, documents from the previous batch
/// that fall outside the new scope are marked "not in source" in this round; whether to delete them is up to a human, as with other sources
pub async fn select(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, source_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SelectBody>,
) -> ApiResult<Json<Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let source = drive_source(&state, kb_id, source_id).await?;
    require_connector(&source, user.id)?;
    if body.folders.len() > MAX_FOLDERS {
        return Err(AppError::invalid(
            "google_drive_too_many_folders",
            format!("Choose at most {MAX_FOLDERS} folders"),
        )
        .into());
    }
    if body.folders.iter().any(|f| !google_drive::valid_id(&f.id)) {
        return Err(AppError::invalid("google_drive_invalid_folder", "Invalid folder id").into());
    }
    let updated = utopia_store::sources::patch_config(
        &state.pool,
        source_id,
        &json!({ "folders": body.folders }),
        &[],
    )
    .await?;
    if !body.folders.is_empty() {
        utopia_store::sources::enqueue_sync_after_config_change(&state.pool, source_id).await?;
    }
    state.emit_source(kb_id);
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "source.updated",
        "source",
        Some(source_id),
        json!({ "google_drive_folders": body.folders.len() }),
    )
    .await;
    Ok(Json(json!({ "source": updated.without_secrets() })))
}

/// A small page with no script. The strings are hardcoded here; nothing from the request is concatenated in
fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem\">\
         <h1 style=\"font-size:1.25rem\">{title}</h1><p>{body}</p></body></html>"
    ))
}

#[cfg(test)]
#[path = "google_drive_routes_tests.rs"]
mod tests;
