//! Data sources for question answering: system-level registration (credentials centralized, reusable across KBs) + KB-level mounting (permissions follow the KB).
//! The safety gates at query time (read-only session, SQL parse allowlist, LIMIT/timeout) live on the server side.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use utopia_core::models::DataSourceView;
use utopia_core::{secrets, AppError, AppResult};
use uuid::Uuid;

/// Row projection of data_sources, shared by list and mounted.
type DataSourceRow = (
    Uuid,
    String,
    String,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<bool>,
);

/// Connection string → credential-free summary (host[:port]/path). On a parse failure it gives a placeholder, never echoing the original string.
/// The port is not filled in when it was not written: the four schemes have different default ports, and filling in the wrong one is more misleading than leaving it out
pub fn conn_summary(conn: &str) -> String {
    url::Url::parse(conn)
        .ok()
        .map(|u| {
            format!(
                "{}{}{}",
                u.host_str().unwrap_or("?"),
                u.port().map(|p| format!(":{p}")).unwrap_or_default(),
                u.path()
            )
        })
        .unwrap_or_else(|| "(unparsed)".into())
}

/// Row → view. **The connection string is swapped for a credential-free summary here**; this is the gate that keeps it from leaking;
/// the four queries share one, so a query added someday cannot forget to swap it
fn row_to_view(
    (id, name, engine, conn, created_at, last_test_at, last_test_ok): DataSourceRow,
) -> DataSourceView {
    // The connection string is sealed in the database; if it cannot be opened (the key was changed), say so and do not parse the ciphertext as a URL
    let summary = match secrets::open(&conn) {
        Ok(plain) => conn_summary(&plain),
        Err(_) => "(sealed with another key)".into(),
    };
    DataSourceView {
        id,
        name,
        engine,
        summary,
        created_at,
        last_test_at,
        last_test_ok,
    }
}

/// All sources in the deployment. **For the system admin registration console only.**
/// The old mountable list also went through it — the door 0014 closed; the KB side now goes through
/// `granted_to_workspace`.
pub async fn list(pool: &PgPool) -> AppResult<Vec<DataSourceView>> {
    let rows: Vec<DataSourceRow> = sqlx::query_as(
        "SELECT id, name, engine, conn_string, created_at, last_test_at, last_test_ok
             FROM data_sources ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_view).collect())
}

pub async fn create(
    pool: &PgPool,
    name: &str,
    engine: &str,
    conn_string: &str,
    created_by: Uuid,
) -> AppResult<Uuid> {
    if name.trim().is_empty() {
        return Err(AppError::invalid(
            "ds_name_required",
            "Data source name is required",
        ));
    }
    // The caller picks the engine from the connection string's scheme (`query_engine::engine_from_conn`);
    // the allowed values are in migration 0020's CHECK, so they are not copied here again
    if engine.is_empty() || conn_string.trim().is_empty() {
        return Err(AppError::invalid(
            "bad_conn_string",
            "A connection string is required",
        ));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO data_sources (id, name, engine, conn_string, created_by)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(name.trim())
    .bind(engine)
    .bind(secrets::seal(conn_string.trim()))
    .bind(created_by)
    .execute(pool)
    .await?;
    Ok(id)
}

pub async fn delete(pool: &PgPool, id: Uuid) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM data_sources WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// The connection string only flows inside the server (test connection / query execution).
pub async fn conn_string(pool: &PgPool, id: Uuid) -> AppResult<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT conn_string FROM data_sources WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let (conn,) = row.ok_or(AppError::NotFound)?;
    secrets::open(&conn).map_err(AppError::Other)
}

pub async fn record_test(pool: &PgPool, id: Uuid, ok: bool) -> AppResult<()> {
    sqlx::query("UPDATE data_sources SET last_test_at = now(), last_test_ok = $2 WHERE id = $1")
        .bind(id)
        .bind(ok)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// KB mounts
// ---------------------------------------------------------------------------

pub async fn mount(pool: &PgPool, kb_id: Uuid, data_source_id: Uuid) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO kb_data_sources (kb_id, data_source_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(kb_id)
    .bind(data_source_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn unmount(pool: &PgPool, kb_id: Uuid, data_source_id: Uuid) -> AppResult<()> {
    sqlx::query("DELETE FROM kb_data_sources WHERE kb_id = $1 AND data_source_id = $2")
        .bind(kb_id)
        .bind(data_source_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mounted(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<DataSourceView>> {
    let rows: Vec<DataSourceRow> = sqlx::query_as(
        "SELECT d.id, d.name, d.engine, d.conn_string, d.created_at,
                    d.last_test_at, d.last_test_ok
             FROM kb_data_sources m JOIN data_sources d ON d.id = m.data_source_id
             WHERE m.kb_id = $1 ORDER BY d.name",
    )
    .bind(kb_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_view).collect())
}

/// (engine, conn_string): used for query execution, testing, and fetching schema (credentials do not leave the server).
pub async fn engine_and_conn(pool: &PgPool, id: Uuid) -> AppResult<(String, String)> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT engine, conn_string FROM data_sources WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let (engine, conn) = row.ok_or(AppError::NotFound)?;
    Ok((engine, secrets::open(&conn).map_err(AppError::Other)?))
}

/// Which sources this workspace is authorized to use (0014).
///
/// **The mountable list now goes through here, not `list`.** The old path showed KB admins
/// `datasources::list(pool)` — every source in the deployment, unfiltered. So any knowledge base's admin
/// could mount any production database into their own KB, and then every Viewer of that KB could run read-only SQL against it.
pub async fn granted_to_workspace(
    pool: &PgPool,
    workspace_id: Uuid,
) -> AppResult<Vec<DataSourceView>> {
    let rows: Vec<DataSourceRow> = sqlx::query_as(
        "SELECT d.id, d.name, d.engine, d.conn_string, d.created_at,
                d.last_test_at, d.last_test_ok
           FROM data_source_grants g JOIN data_sources d ON d.id = g.data_source_id
          WHERE g.workspace_id = $1 ORDER BY d.name",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(row_to_view).collect())
}

/// Which workspaces this source is authorized for. The admin console reads it.
pub async fn grants_for_source(
    pool: &PgPool,
    data_source_id: Uuid,
) -> AppResult<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT w.id, w.name FROM data_source_grants g
           JOIN workspaces w ON w.id = g.workspace_id
          WHERE g.data_source_id = $1 ORDER BY w.name",
    )
    .bind(data_source_id)
    .fetch_all(pool)
    .await?)
}

/// Authorizes a workspace to use this source. Idempotent.
pub async fn grant(
    pool: &PgPool,
    data_source_id: Uuid,
    workspace_id: Uuid,
    actor: Uuid,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO data_source_grants (data_source_id, workspace_id, granted_by)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(data_source_id)
    .bind(workspace_id)
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revokes the authorization, **and also unmounts what that workspace had already mounted**.
///
/// Deleting only the authorization row is not enough: `mounted` reads `kb_data_sources`, and question answering reads it too. Leaving the mount
/// means revoking the authorization while access carries on as before — a revoke that does not take effect is more dangerous than none at all.
///
/// Done in one transaction: if it crashed between the two DELETEs, what would be left is exactly the 'mounted without authorization' state,
/// and that is precisely what this migration exists to eliminate.
pub async fn revoke(pool: &PgPool, data_source_id: Uuid, workspace_id: Uuid) -> AppResult<u64> {
    let mut tx = pool.begin().await?;
    let unmounted = sqlx::query(
        "DELETE FROM kb_data_sources
          WHERE data_source_id = $1
            AND kb_id IN (SELECT id FROM knowledge_bases WHERE workspace_id = $2)",
    )
    .bind(data_source_id)
    .bind(workspace_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query("DELETE FROM data_source_grants WHERE data_source_id = $1 AND workspace_id = $2")
        .bind(data_source_id)
        .bind(workspace_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(unmounted)
}

/// Whether this source is authorized for this knowledge base. **Must be checked before mounting** — the guard cannot sit only on the list side:
/// the list filter blocks "visible", but the mount endpoint is called by id, and anyone can construct one themselves.
pub async fn is_granted(pool: &PgPool, kb_id: Uuid, data_source_id: Uuid) -> AppResult<bool> {
    let found: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM data_source_grants g
           JOIN knowledge_bases kb ON kb.workspace_id = g.workspace_id
          WHERE kb.id = $1 AND g.data_source_id = $2",
    )
    .bind(kb_id)
    .bind(data_source_id)
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}
