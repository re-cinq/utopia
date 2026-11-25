//! Audit log: who did what to what, and when. Pure audit: it only records and displays, and carries no derived features such as rollback.
//! Ordinary events are recorded by the caller on a best-effort basis; review verdicts are committed together with the graph change via `record_on`.

use sqlx::PgPool;
use utopia_core::models::AuditEventView;
use utopia_core::AppResult;
use uuid::Uuid;

pub async fn record(
    pool: &PgPool,
    kb_id: Option<Uuid>,
    actor_id: Uuid,
    action: &str,
    target_kind: &str,
    target_id: Option<Uuid>,
    detail: serde_json::Value,
) -> AppResult<()> {
    record_opt(
        pool,
        kb_id,
        Some(actor_id),
        action,
        target_kind,
        target_id,
        detail,
    )
    .await
}

/// The action performed on behalf of a token is recorded with which one it was in detail (0014 decision 6): before revoking, an admin must be able to tell
/// what that token did. Session-originated calls (None) are returned as-is
pub fn via_token(mut detail: serde_json::Value, token_id: Option<Uuid>) -> serde_json::Value {
    if let (Some(token_id), Some(fields)) = (token_id, detail.as_object_mut()) {
        fields.insert(
            "token_id".into(),
            serde_json::Value::String(token_id.to_string()),
        );
    }
    detail
}

/// The request's source info. The HTTP layer scopes it into [`CLIENT`] around each request, and `record_opt`
/// reads it itself — otherwise each of the 25 call sites would have to carry two parameters unrelated to the business logic.
///
/// Background tasks (batched verdicts, scheduled syncs) are not inside any request, so they read None, which is exactly right:
/// those actions really have no client.
#[derive(Debug, Clone, Default)]
pub struct ClientContext {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

tokio::task_local! {
    pub static CLIENT: ClientContext;
}

/// Gets the current request's source; returns all-empty when not inside a request context (background tasks).
fn client_context() -> ClientContext {
    CLIENT.try_with(|c| c.clone()).unwrap_or_default()
}

/// System events with no human actor (such as AI verdicts auto-merging) go through here: actor is NULL.
pub async fn record_opt(
    pool: &PgPool,
    kb_id: Option<Uuid>,
    actor_id: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Option<Uuid>,
    detail: serde_json::Value,
) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    record_on(
        &mut conn,
        kb_id,
        actor_id,
        action,
        target_kind,
        target_id,
        detail,
    )
    .await
}

/// Reviewed graph changes must not commit without their actor's decision.
pub async fn record_on(
    conn: &mut sqlx::PgConnection,
    kb_id: Option<Uuid>,
    actor_id: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Option<Uuid>,
    detail: serde_json::Value,
) -> AppResult<()> {
    let ctx = client_context();
    // Identity snapshot: when the user is later deleted (rows after 0025 keep it), the ledger can still tell who it was, rather than
    // only a UUID. Looking this up now is reliable — the operation is happening, and the person is still around.
    let actor_label: Option<String> = match actor_id {
        Some(id) => {
            sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?
        }
        None => None,
    };
    sqlx::query(
        "INSERT INTO audit_events
            (id, kb_id, actor_id, action, target_kind, target_id, detail,
             client_ip, user_agent, actor_label)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(actor_id)
    .bind(action)
    .bind(target_kind)
    .bind(target_id)
    .bind(detail)
    .bind(ctx.ip)
    .bind(ctx.user_agent)
    .bind(actor_label)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Review decision ledger: only takes review-domain actions (review./fact./conflict./merge.), paginated server-side.
pub async fn review_history(
    pool: &PgPool,
    kb_id: Uuid,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<AuditEventView>, i64)> {
    const COND: &str = "e.kb_id = $1 AND (e.action LIKE 'review.%' OR e.action LIKE 'fact.%'
                        OR e.action LIKE 'conflict.%' OR e.action LIKE 'merge.%')";
    let rows: Vec<AuditEventView> = sqlx::query_as(&format!(
        "SELECT e.id, e.action, e.target_kind, e.target_id, e.detail,
                e.actor_id, u.display_name AS actor_name, e.created_at
         FROM audit_events e LEFT JOIN users u ON u.id = e.actor_id
         WHERE {COND} ORDER BY e.created_at DESC, e.id DESC LIMIT $2 OFFSET $3"
    ))
    .bind(kb_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let (total,): (i64,) =
        sqlx::query_as(&format!("SELECT count(*) FROM audit_events e WHERE {COND}"))
            .bind(kb_id)
            .fetch_one(pool)
            .await?;
    Ok((rows, total))
}

/// A knowledge base's audit ledger, **with pagination and filtering**.
///
/// It used to be fixed at the latest 100 entries, with no pagination and no filtering — but the ledger is compliance material, and "only able to see the latest
/// one hundred" makes history impossible to query. The three filters were chosen from real ways of querying:
///
/// - `action`: query one class of action ("who changed a type", "what was rejected"). Prefix match rather than
///   exact match, because action names are layered (`entity.retyped` / `entity.renamed`),
///   so passing `entity.` pulls out a whole family
/// - `actor`: query what one person did. The most common question in compliance audits
/// - `since` / `until`: query a span of time. Incident reviews need exactly this
///
/// Also returns the total, otherwise the pager does not know how many pages there are — and "not knowing how many pages" is the same
/// as the 100 cap it replaces.
#[allow(clippy::too_many_arguments)]
pub async fn list_for_kb(
    pool: &PgPool,
    kb_id: Uuid,
    action: Option<&str>,
    actor: Option<Uuid>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    limit: i64,
    offset: i64,
) -> AppResult<(Vec<AuditEventView>, i64)> {
    // All four filters are written as "no effect when the parameter is empty", so one SQL statement covers every combination —
    // string concatenation would grow sixteen branches here, and each branch is an injection surface
    const WHERE: &str = "WHERE e.kb_id = $1
           AND ($2::text IS NULL OR e.action LIKE $2 || '%')
           AND ($3::uuid IS NULL OR e.actor_id = $3)
           AND ($4::timestamptz IS NULL OR e.created_at >= $4)
           AND ($5::timestamptz IS NULL OR e.created_at < $5)";

    let rows: Vec<AuditEventView> = sqlx::query_as(&format!(
        "SELECT e.id, e.action, e.target_kind, e.target_id, e.detail,
                e.actor_id, u.display_name AS actor_name, e.created_at
         FROM audit_events e LEFT JOIN users u ON u.id = e.actor_id
         {WHERE}
         ORDER BY e.created_at DESC, e.id DESC LIMIT $6 OFFSET $7"
    ))
    .bind(kb_id)
    .bind(action)
    .bind(actor)
    .bind(since)
    .bind(until)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let (total,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM audit_events e {WHERE}"))
        .bind(kb_id)
        .bind(action)
        .bind(actor)
        .bind(since)
        .bind(until)
        .fetch_one(pool)
        .await?;
    Ok((rows, total))
}

/// The actions that have occurred in this KB's ledger. **Filter dropdowns must be filled from what actually exists** — a hardcoded
/// list of all actions would show the user a pile of options that never happened in this KB.
pub async fn actions_for_kb(pool: &PgPool, kb_id: Uuid) -> AppResult<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT DISTINCT action FROM audit_events WHERE kb_id = $1 ORDER BY 1")
            .bind(kb_id)
            .fetch_all(pool)
            .await?,
    )
}
