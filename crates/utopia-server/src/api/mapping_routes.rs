//! Data mapping API: listing, revising and the revision history of definitions.
//!
//! Approval (`decide`) stays in `review_routes`: that endpoint already writes the audit ledger
//! (`mapping.decided`), so calling it from another screen is enough; there is no reason to move an endpoint just to move a page.

use axum::extract::{Path, Query, State};
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

/// Rows per page. Definitions are denser than the review queue (one row is one definition), so a page can afford 25
const MAPPING_PAGE: i64 = 25;

#[derive(Deserialize)]
pub struct ListQuery {
    /// proposed | confirmed | rejected; default = all
    status: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// A page of definitions, plus how many there are in each of the three states.
///
/// **Viewers can see it.** A definition is "how this number is computed", and it directly decides the answer to a data question;
/// seeing an answer without seeing its definition is asking a person to trust an algorithm they are not allowed to look at. Revising (`revise`)
/// requires Editor.
pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let status = q.status.as_deref().filter(|s| !s.is_empty());
    if let Some(s) = status {
        if !matches!(s, "proposed" | "confirmed" | "rejected") {
            return Err(AppError::invalid(
                "bad_status",
                "status must be proposed, confirmed or rejected",
            )
            .into());
        }
    }
    let needle = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let limit = q.limit.unwrap_or(MAPPING_PAGE).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let (items, total) =
        utopia_store::mappings::page(&state.pool, kb_id, status, needle, limit, offset).await?;
    let (proposed, confirmed, rejected) =
        utopia_store::mappings::status_counts(&state.pool, kb_id).await?;
    // The tally of the previous exploration round (#503). **The list alone cannot answer "how much was missed"**: eleven proposals against
    // a wide table of eighty columns look exactly the same in items as eleven that happen to cover a small KB completely
    let last_run = utopia_store::exploration_runs::recent(&state.pool, kb_id, 1)
        .await?
        .into_iter()
        .next();
    Ok(Json(json!({
        "items": items,
        "total": total,
        "counts": { "proposed": proposed, "confirmed": confirmed, "rejected": rejected },
        "last_run": last_run,
    })))
}

#[derive(Deserialize)]
pub struct ReviseReq {
    table_name: Option<String>,
    expr: Option<String>,
    sql: Option<String>,
    unit: Option<String>,
    summary: Option<String>,
    #[serde(default)]
    derived: bool,
}

/// Revise one definition.
///
/// **Before this, `mappings::revise` had zero callers**: the function existed, the audit table existed, but there was no route.
/// So once a definition was confirmed, no one could change it or even see it: data questions were answered by it, yet people could not reach it.
pub async fn revise(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, mapping_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<ReviseReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    // Blank counts as unset: when the frontend clears an input box it sends "", which should be stored as NULL, not an empty string,
    // otherwise the check for "is expr configured" would have to ask both IS NULL and = ''
    let clean = |s: &Option<String>| -> Option<String> {
        s.as_deref()
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
    };
    let (table_name, expr, sql, unit, summary) = (
        clean(&req.table_name),
        clean(&req.expr),
        clean(&req.sql),
        clean(&req.unit),
        clean(&req.summary),
    );
    if table_name.is_none() && expr.is_none() && sql.is_none() {
        return Err(AppError::invalid(
            "empty_mapping",
            "A mapping needs at least one of table, expression or SQL",
        )
        .into());
    }
    utopia_store::mappings::revise(
        &state.pool,
        kb_id,
        mapping_id,
        table_name.as_deref(),
        expr.as_deref(),
        sql.as_deref(),
        unit.as_deref(),
        summary.as_deref(),
        req.derived,
        user.id,
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "mapping.revised",
        "concept_mapping",
        Some(mapping_id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// Revision history of one definition. 0006 says the audit trail exists so you can answer "how was this number computed last quarter",
/// and this is where that promise is kept.
pub async fn revisions(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, mapping_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let rows = utopia_store::mappings::revisions(&state.pool, kb_id, mapping_id).await?;
    Ok(Json(json!({ "revisions": rows })))
}

#[derive(Deserialize)]
pub struct RelevantQuery {
    q: String,
    k: Option<usize>,
}

/// Definitions related to a question, ranked by relevance: the data-question prompt uses exactly this retrieval (#574).
///
/// It is exposed as an endpoint for two reasons: the measurement bench can measure recall@k directly (is the right definition in the top k),
/// without asking the question for real; and the page can later list "definitions used" beside a question. Viewers can see it,
/// for the same reason as the list: seeing an answer without seeing its definition is asking a person to trust an algorithm they are not allowed to look at.
pub async fn relevant(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<RelevantQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let kb = require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let k =
        q.k.unwrap_or(crate::mapping_index::DEFINITIONS_IN_PROMPT)
            .clamp(1, 50);
    let items = crate::mapping_index::relevant(&state, kb_id, kb.workspace_id, &q.q, k, None)
        .await
        .map_err(AppError::Other)?;
    Ok(Json(json!({ "items": items, "k": k })))
}

/// Blank counts as unset: when the frontend clears an input box it sends "", which should be stored as NULL, not an empty string.
fn clean(s: &Option<String>) -> Option<String> {
    s.as_deref()
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

/// A definition must have at least one of table, expression or SQL, otherwise it is not an executable definition.
/// `create`, `preview` and `revise` check the same thing.
fn definition_shape(
    table_name: &Option<String>,
    expr: &Option<String>,
    sql: &Option<String>,
) -> Result<(), AppError> {
    if table_name.is_none() && expr.is_none() && sql.is_none() {
        return Err(AppError::invalid(
            "empty_mapping",
            "A mapping needs at least one of table, expression or SQL",
        ));
    }
    Ok(())
}

/// The SQL a definition runs as: if `sql` is given use it, otherwise build one from `expr` + `table`.
/// The measurement bench (`proposalSql` in `scripts/bench/mappings.mjs`) scores with the same construction,
/// so the number previewed on the page is the number the bench will score.
fn render_sql(
    table_name: &Option<String>,
    expr: &Option<String>,
    sql: &Option<String>,
) -> Option<String> {
    if let Some(s) = sql {
        return Some(s.trim_end_matches(';').trim().to_string());
    }
    match (expr, table_name) {
        (Some(e), Some(t)) => Some(format!("SELECT {e} FROM {t}")),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct CreateReq {
    concept: String,
    /// metric | dimension; default metric
    #[serde(default = "default_kind")]
    kind: String,
    source: String,
    table_name: Option<String>,
    expr: Option<String>,
    sql: Option<String>,
    unit: Option<String>,
    summary: Option<String>,
    #[serde(default)]
    derived: bool,
}
fn default_kind() -> String {
    "metric".into()
}

/// A person writes a definition from scratch (#562).
///
/// Before, this table had only one route in: exploration. A data team has its own metrics definition document, but nowhere
/// to enter it; and definitions go into the data-question prompt, which took wide-table coverage from 1/18 to 17/18 (#520). What was missing
/// was not the structure, it was this door.
///
/// Concepts still become Metric / Dimension entities: that is how the table works today (`concept_id` non-null
/// points at an entity). 0035 is retiring this shape (#556), but the table is kept and rendered into, and SQL a person writes
/// carries over.
pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let concept = req.concept.trim();
    if concept.is_empty() {
        return Err(AppError::invalid("empty_concept", "A definition needs a concept name").into());
    }
    if !matches!(req.kind.as_str(), "metric" | "dimension") {
        return Err(AppError::invalid("bad_kind", "kind must be metric or dimension").into());
    }
    let (table_name, expr, sql, unit, summary) = (
        clean(&req.table_name),
        clean(&req.expr),
        clean(&req.sql),
        clean(&req.unit),
        clean(&req.summary),
    );
    definition_shape(&table_name, &expr, &sql)?;
    // Sources are picked by name, and must be mounted in this KB: the same safety boundary as `query_data`
    let mounted = utopia_store::datasources::mounted(&state.pool, kb_id).await?;
    let Some(ds) = mounted
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(req.source.trim()))
    else {
        return Err(AppError::invalid(
            "source_not_mounted",
            "That data source is not mounted on this knowledge base",
        )
        .into());
    };

    crate::mappings::ensure_concept_types(&state.pool, kb_id)
        .await
        .map_err(AppError::Other)?;
    let (type_id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM entity_types WHERE kb_id = $1 AND key = $2")
            .bind(kb_id)
            .bind(&req.kind)
            .fetch_one(&state.pool)
            .await?;
    // Concept entities go through resolution: same-name ones are merged, landing in the same place as those exploration proposed
    let resolved = utopia_store::resolution::resolve_mention(
        &state.pool,
        kb_id,
        Some(type_id),
        concept,
        None,
        None,
        None,
        &[],
    )
    .await?;
    let id = utopia_store::mappings::create(
        &state.pool,
        kb_id,
        resolved.entity_id,
        &ds.name,
        table_name.as_deref(),
        expr.as_deref(),
        sql.as_deref(),
        unit.as_deref(),
        summary.as_deref(),
        req.derived,
        user.id,
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "mapping.written",
        "concept_mapping",
        Some(id),
        json!({ "concept": concept, "source": ds.name }),
    )
    .await;
    Ok(Json(json!({ "id": id, "concept_id": resolved.entity_id })))
}

#[derive(Deserialize)]
pub struct PreviewReq {
    source: String,
    table_name: Option<String>,
    expr: Option<String>,
    sql: Option<String>,
}

/// Run a definition through the read-only gate and return the first row: the person writing the definition sees the number it computes on the spot.
///
/// This is exactly the action the measurement bench takes when scoring (#501 / #520: run the definition and compare the numbers to gold),
/// brought onto the page. A definition that fails to run is harmless, since it fails loudly; only one that runs and computes wrongly is a real risk,
/// and looking at the number is the only check a person can do.
pub async fn preview(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<PreviewReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let (table_name, expr, sql) = (clean(&req.table_name), clean(&req.expr), clean(&req.sql));
    definition_shape(&table_name, &expr, &sql)?;
    let Some(rendered) = render_sql(&table_name, &expr, &sql) else {
        return Err(AppError::invalid(
            "empty_mapping",
            "A preview needs SQL, or an expression together with a table",
        )
        .into());
    };
    let mounted = utopia_store::datasources::mounted(&state.pool, kb_id).await?;
    let Some(ds) = mounted
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(req.source.trim()))
    else {
        return Err(AppError::invalid(
            "source_not_mounted",
            "That data source is not mounted on this knowledge base",
        )
        .into());
    };
    // Pass the engine's error back as-is: a misspelled column name, a syntax error and a timeout are three different things
    let out = super::tools::run_query(&state, ds.id, &rendered)
        .await
        .map_err(|e| AppError::invalid("preview_failed", e.to_string()))?;
    // The result is JSON Lines; the first line is the number (or group of numbers) this definition computes
    let rows: Vec<serde_json::Value> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .take(5)
        .collect();
    Ok(Json(
        json!({ "sql": rendered, "row": rows.first(), "rows": rows }),
    ))
}

#[cfg(test)]
mod tests {
    use super::{definition_shape, render_sql};

    #[test]
    fn a_definition_needs_something_to_run() {
        let none: Option<String> = None;
        assert!(definition_shape(&none, &none, &none).is_err());
        assert!(definition_shape(&Some("t".into()), &none, &none).is_ok());
        assert!(definition_shape(&none, &Some("sum(x)".into()), &none).is_ok());
        assert!(definition_shape(&none, &none, &Some("SELECT 1".into())).is_ok());
    }

    #[test]
    fn preview_runs_what_the_bench_scores() {
        let none: Option<String> = None;
        // sql takes precedence, and the trailing semicolon is removed: the gate only lets through a single statement
        assert_eq!(
            render_sql(
                &Some("t".into()),
                &Some("sum(x)".into()),
                &Some("SELECT 2;".into())
            )
            .as_deref(),
            Some("SELECT 2")
        );
        assert_eq!(
            render_sql(
                &Some("dw.orders".into()),
                &Some("sum(amt) / 100.0".into()),
                &none
            )
            .as_deref(),
            Some("SELECT sum(amt) / 100.0 FROM dw.orders")
        );
        // Only a table name is not an executable definition
        assert_eq!(render_sql(&Some("t".into()), &none, &none), None);
    }
}
