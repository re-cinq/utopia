//! Query data sources: system-level registration (admin; credentials go in and never come out) and KB-level mounting (KB admin).
//! When a source is mounted or refreshed manually, the target database's schema is generated as markdown and ingested into the KB (updated in place under the same key),
//! so Chat can retrieve the table structure before writing SQL. The safety gate for query execution is in step 2 (the query_data tool).

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use utopia_core::AppError;
use uuid::Uuid;

use super::graph_routes::require_kb;
use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::state::AppState;

fn require_admin(user: &utopia_core::models::User) -> Result<(), AppError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

// ---------------------------------------------------------------------------
// System level: register / test / delete
// ---------------------------------------------------------------------------

pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    let sources = utopia_store::datasources::list(&state.pool).await?;
    Ok(Json(json!({ "data_sources": sources })))
}

#[derive(Deserialize)]
pub struct CreateBody {
    pub name: String,
    #[serde(default = "default_engine")]
    pub engine: String,
    pub conn_string: String,
}
fn default_engine() -> String {
    "postgres".into()
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    // The engine follows the scheme; the UI has only one connection-string box, and body.engine is kept only for old callers
    let engine = crate::query_engine::engine_from_conn(&body.conn_string).ok_or_else(|| {
        utopia_core::AppError::invalid(
            "unsupported_conn_scheme",
            format!(
                "Connection string must start with one of: postgres://, mysql://, trino://, databricks://, snowflake://, clickhouse:// (engines: {})",
                crate::query_engine::ENGINES.join(", ")
            ),
        )
    })?;
    let _ = &body.engine;
    // The shape of the connection string is validated at registration (missing token, missing warehouse, ...), with the correct form in the error message;
    // otherwise you only find out at "Test", which returns just ok:false
    crate::query_engine::engine_for(engine, &body.conn_string)
        .map_err(|e| utopia_core::AppError::invalid("bad_conn_string", e.to_string()))?;
    let id = utopia_store::datasources::create(
        &state.pool,
        &body.name,
        engine,
        &body.conn_string,
        user.id,
    )
    .await?;
    // **You can mount it right after registering.** Authorization is per workspace (0014), but the workspace has already become invisible in the UI:
    // a single-tenant deployment has just one, and nobody has seen its name. Before, after registering you still had to first
    // "authorize it to a workspace" on the card, which asked people to authorize something they had never seen, and mounting only got
    // "not authorized". So the registering person's workspaces are all authorized in one go; to narrow it, the card still lets you revoke
    for ws in utopia_store::workspaces::list_for_user(&state.pool, user.id).await? {
        utopia_store::datasources::grant(&state.pool, id, ws.id, user.id).await?;
    }
    Ok(Json(json!({ "id": id })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    utopia_store::datasources::delete(&state.pool, id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct TestConnBody {
    pub conn_string: String,
}

/// Try once before saving. **This step does not write to the DB**: the "Test connection" button in the registration form calls this.
///
/// Before, you had to save first and then test, and if it failed you had to go back and delete the entry; [`test`] only returns `ok: false`,
/// without saying why it failed to connect. Here the engine's error is passed back as-is: a wrong password, a misspelled database name,
/// and an unreachable port are three different things, and the person needs to know which one it is.
pub async fn test_conn(
    AuthUser(user): AuthUser,
    Json(body): Json<TestConnBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    let engine = crate::query_engine::engine_from_conn(&body.conn_string).ok_or_else(|| {
        AppError::invalid(
            "unsupported_conn_scheme",
            format!(
                "Connection string must start with one of: postgres://, mysql://, trino://, databricks://, snowflake://, clickhouse:// (engines: {})",
                crate::query_engine::ENGINES.join(", ")
            ),
        )
    })?;
    // The shape of the connection string (missing token, missing warehouse, ...) is reported separately from connect failure: the former means it was written wrong,
    // the latter is an environment problem, and the two messages call for different next steps
    let eng = crate::query_engine::engine_for(engine, &body.conn_string)
        .map_err(|e| AppError::invalid("bad_conn_string", e.to_string()))?;
    match eng.test().await {
        Ok(()) => Ok(Json(json!({ "ok": true, "engine": engine }))),
        Err(e) => Ok(Json(
            json!({ "ok": false, "engine": engine, "error": e.to_string() }),
        )),
    }
}

pub async fn test(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    let (engine, conn) = utopia_store::datasources::engine_and_conn(&state.pool, id).await?;
    let ok = match crate::query_engine::engine_for(&engine, &conn) {
        Ok(eng) => eng.test().await.is_ok(),
        Err(_) => false,
    };
    utopia_store::datasources::record_test(&state.pool, id, ok).await?;
    Ok(Json(json!({ "ok": ok })))
}

// ---------------------------------------------------------------------------
// KB level: mount / unmount / schema refresh
// ---------------------------------------------------------------------------

pub async fn mounted(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let mounted = utopia_store::datasources::mounted(&state.pool, kb_id).await?;
    Ok(Json(json!({ "data_sources": mounted })))
}

/// What the KB admin can mount (the list gives name/summary, without credentials).
///
/// **Lists only the sources authorized to this workspace (0014).** Before, this returned `datasources::list`:
/// every source in the deployment, so any KB admin could see and mount any production database.
pub async fn mountable(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let kb = require_kb(&state, &user, kb_id, Role::Admin).await?;
    let granted =
        utopia_store::datasources::granted_to_workspace(&state.pool, kb.workspace_id).await?;
    Ok(Json(json!({ "data_sources": granted })))
}

pub async fn mount(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, ds_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Admin).await?;
    // **List filtering is not a guard.** It only hides what is visible, while this endpoint is called by id:
    // anyone can craft a uuid and send it. Authorization is checked again here
    if !utopia_store::datasources::is_granted(&state.pool, kb_id, ds_id).await? {
        return Err(AppError::invalid(
            "source_not_granted",
            "This data source is not available to this workspace",
        )
        .into());
    }
    utopia_store::datasources::mount(&state.pool, kb_id, ds_id).await?;
    // Mounting ingests the schema: Chat can retrieve the table structure before writing SQL
    //
    // **If this step fails, it must not be reported as a mount failure.** The row above has already been written to kb_data_sources,
    // so the source really is mounted; before, the `?` here returned a 500, and people thought it was not mounted when it was. The mounting is now stated as done, the schema as not done, and
    // the failure is reported to the alert center, since from then on it is a silent missing state (0009);
    // query data cannot see which tables it has, without that
    match sync_schema_doc(&state, kb_id, ds_id).await {
        Ok(synced) => Ok(Json(json!({ "ok": true, "schema_tables": synced }))),
        Err(e) => {
            let name = source_name(&state, ds_id).await;
            crate::alerting::observe_schema_sync_failure(&state, kb_id, ds_id, &name, &e).await;
            Ok(Json(json!({
                "ok": true,
                "schema_tables": 0,
                "schema_error": e.to_string(),
            })))
        }
    }
}

/// The alert must keep the name: once the source is deleted, `subject_id` can no longer resolve a name. When the lookup fails, a placeholder is used
/// instead of failing the alert itself: **a failure on the alerting path must not swamp the thing it is reporting**.
async fn source_name(state: &AppState, ds_id: Uuid) -> String {
    utopia_store::datasources::list(&state.pool)
        .await
        .ok()
        .and_then(|all| all.into_iter().find(|d| d.id == ds_id).map(|d| d.name))
        .unwrap_or_else(|| ds_id.to_string())
}

pub async fn unmount(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, ds_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Admin).await?;
    utopia_store::datasources::unmount(&state.pool, kb_id, ds_id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Manual structure refresh.
///
/// **Here the error is still returned to the caller**: the person who clicked is watching, and nothing has half-happened.
/// But an alert is also reported: the consequence is the same as for a mount failure (the source stays mounted, the table structure is stale
/// or empty), and the person who clicked is not necessarily the one who needs to know about it.
pub async fn sync_schema(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, ds_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Admin).await?;
    match sync_schema_doc(&state, kb_id, ds_id).await {
        Ok(synced) => Ok(Json(json!({ "ok": true, "schema_tables": synced }))),
        Err(e) => {
            let name = source_name(&state, ds_id).await;
            crate::alerting::observe_schema_sync_failure(&state, kb_id, ds_id, &name, &e).await;
            // **If the structure cannot be read, return 422 with the engine's own words**; errors on our side (the DB, writing the document) stay 500.
            // Before, everything was AppError::Other, so the UI showed only "Internal server error" and the cause was only in the server
            // log, while Invalid is not logged, so one line is logged here by hand
            match e.downcast_ref::<SchemaUnreadable>() {
                Some(unreadable) => {
                    tracing::warn!(%kb_id, %ds_id, error = %unreadable, "数据源结构读取失败");
                    Err(AppError::invalid_detail(
                        "schema_sync_failed",
                        "The data source's schema could not be read",
                        bounded(&unreadable.to_string(), 600),
                    )
                    .into())
                }
                None => Err(AppError::Other(e).into()),
            }
        }
    }
}

/// Agentic exploration: a background job reads the mounted source's schema and proposes metric/dimension -> field mappings (low confidence goes to Review).
pub async fn explore(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Admin).await?;
    if utopia_store::datasources::mounted(&state.pool, kb_id)
        .await?
        .is_empty()
    {
        return Err(AppError::invalid("no_data_sources", "No data sources mounted").into());
    }
    utopia_store::jobs::enqueue(&state.pool, "explore_mappings", json!({ "kb_id": kb_id })).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Pulls information_schema, generates markdown, and ingests it through the three-way decision (updated in place under the same key).
/// The document sits under the per-KB "Data schemas" folder source.
/// Failure of the engine to read the database structure (connection, permissions, catalog missing, ...) is kept separate from our own errors:
/// the former is about the connection string or permissions and goes back to the person who clicked; the latter stays 500
#[derive(Debug)]
struct SchemaUnreadable(String);

impl std::fmt::Display for SchemaUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SchemaUnreadable {}

/// Truncate to at most `max` characters (on a character boundary); the engine's error may carry a whole stack trace
fn bounded(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

async fn sync_schema_doc(state: &AppState, kb_id: Uuid, ds_id: Uuid) -> anyhow::Result<usize> {
    const MAX_TABLES: usize = 200;
    let name = utopia_store::datasources::list(&state.pool)
        .await?
        .into_iter()
        .find(|d| d.id == ds_id)
        .map(|d| d.name)
        .ok_or_else(|| anyhow::anyhow!("Data source not found"))?;
    let (engine, conn) = utopia_store::datasources::engine_and_conn(&state.pool, ds_id).await?;
    let cols = crate::query_engine::engine_for(&engine, &conn)?
        .fetch_schema()
        .await
        .map_err(|e| anyhow::Error::new(SchemaUnreadable(e.to_string())))?;

    let mut md = format!(
        "# Data source: {name}\n\nEngine: {engine}. Tables and columns available for SQL queries against this source; write SQL in this engine's dialect.\n"
    );
    let mut current = String::new();
    let mut tables = 0usize;
    for c in &cols {
        let key = format!("{}.{}", c.schema, c.table);
        if key != current {
            if tables >= MAX_TABLES {
                md.push_str("\n(further tables omitted)\n");
                break;
            }
            current = key.clone();
            tables += 1;
            md.push_str(&format!("\n## {key}\n"));
        }
        md.push_str(&format!(
            "- {} ({}){}\n",
            c.column,
            c.data_type,
            c.comment
                .as_deref()
                .map(|x| format!(" — {x}"))
                .unwrap_or_default()
        ));
    }

    // Per-KB "Data schemas" container source (a folder: pure container semantics).
    //
    // **This document is only retrieval corpus, and is not sent through extraction** (0035 decision 7). When it goes through extraction like other documents,
    // the extractor treats every column name as an entity: in the wide-table corpus, twenty-eight of the forty concept entities were
    // column names (#553). "Do not extract" is recorded in the source's config, which the pipeline reads
    let folder = match sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM sources WHERE kb_id = $1 AND kind = 'folder' AND name = 'Data schemas'",
    )
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await?
    {
        Some((id,)) => id,
        None => {
            utopia_store::sources::create(
                &state.pool,
                kb_id,
                "folder",
                "Data schemas",
                &serde_json::json!({ "extract": false }),
                Some("database"),
                None,
                None,
            )
            .await?
            .id
        }
    };
    crate::ingest_sources::ingest_item(
        state,
        kb_id,
        folder,
        &format!("datasource:{ds_id}:schema"),
        &format!("{name}-schema.md"),
        "text/markdown",
        md.as_bytes(),
        None,
    )
    .await?;
    state.emit_source(kb_id);
    Ok(tables)
}

// ---------------------------------------------------------------------------
// System level: authorization (0014)
//
// Authorization and mounting are two layers, each with its own owner:
//   Authorization = the system admin says which workspaces may use this source  <- here
//   Mounting = the KB admin says which sources this KB mounts          <- the group above
// Both layers are many-to-many. Mounting can only pick from the authorized set.
// ---------------------------------------------------------------------------

/// Which workspaces this source is authorized to.
pub async fn grants(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    let rows = utopia_store::datasources::grants_for_source(&state.pool, id).await?;
    let workspaces: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    Ok(Json(json!({ "workspaces": workspaces })))
}

pub async fn grant(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, workspace_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    utopia_store::datasources::grant(&state.pool, id, workspace_id, user.id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "data_source.granted",
        "data_source",
        Some(id),
        json!({ "workspace_id": workspace_id }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// Revoke authorization. **Also unmounts whatever is already mounted in that workspace**: deleting only the authorization row
/// would leave query data reading `kb_data_sources`, so the revocation would have no effect. Returns how many were unmounted,
/// so the UI can say "also unmounted 3 DBs" instead of silently cutting off someone's connection.
pub async fn revoke(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, workspace_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(&user)?;
    let unmounted = utopia_store::datasources::revoke(&state.pool, id, workspace_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "data_source.revoked",
        "data_source",
        Some(id),
        json!({ "workspace_id": workspace_id, "unmounted": unmounted }),
    )
    .await;
    Ok(Json(json!({ "ok": true, "unmounted": unmounted })))
}
