use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use utopia_core::AppError;
use uuid::Uuid;

use super::documents_routes::require_reader_kb;
use crate::auth::{AuthUser, SessionOrToken};
use crate::error::ApiResult;
use crate::state::AppState;

#[cfg(test)]
#[path = "graph_routes_time_tests.rs"]
mod time_tests;

pub(super) async fn require_kb(
    state: &AppState,
    user: &utopia_core::models::User,
    kb_id: Uuid,
    min: Role,
) -> Result<utopia_core::models::KnowledgeBase, AppError> {
    utopia_store::access::require_kb(&state.pool, user, kb_id, min).await
}

/// Time parameter (YYYY-MM-DD or RFC3339): server-side time travel.
///
/// Both axes share this one parser: `at` goes on the world axis (what the world was like then), `as_of` goes on the record axis
/// (what we believed the world was like then). **The two parameters stay separate** (0019): merging them into one control
/// answers a different question, and the screen gives no sign of it
pub(super) fn parse_instant(
    field: &str,
    raw: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Some(d.and_hms_opt(0, 0, 0).unwrap().and_utc()));
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| Some(t.with_timezone(&chrono::Utc)))
        .map_err(|_| {
            AppError::Validation(format!(
                "Invalid `{field}` (expected YYYY-MM-DD or RFC3339)"
            ))
        })
}

/// **Default** number of nodes drawn in the overview. The cap itself is reasonable: nobody can make sense of ten thousand dots;
/// what is misleading is presenting it as the scale, so the API also returns the total
const GRAPH_NODE_CAP: i64 = 150;
/// Even when raised, there must be a ceiling. **This number was not picked at random**: nodes are taken in descending degree order, so the further down, the more
/// peripheral the nodes; and force-directed layout is roughly O(n²), so past this number what breaks first is "can be dragged" rather than
/// "can be seen". Viewing tens of thousands of points is a different kind of view, not something to get by raising this
const GRAPH_NODE_CAP_MAX: i64 = 1000;

#[derive(Deserialize)]
pub struct OverviewQuery {
    #[serde(default)]
    pub at: Option<String>,
    /// Record axis: the graph *we held* at that time (0019). Omitted means now
    #[serde(default)]
    pub as_of: Option<String>,
    /// How many to draw. Omitted uses the default; if given it is still clamped to [10, GRAPH_NODE_CAP_MAX].
    /// The UI buttons offer only a few steps, but the API is public, so do not let a `limit=999999` drag the library down
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn overview(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<OverviewQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let at = parse_instant("at", q.at.as_deref())?;
    let as_of = parse_instant("as_of", q.as_of.as_deref())?;
    // How many to draw is a rendering matter; how many the knowledge base has is a knowledge-base matter, so return both and the UI can say
    // "drew 150 of 325" instead of presenting the cap as the scale
    let limit = q
        .limit
        .unwrap_or(GRAPH_NODE_CAP)
        .clamp(10, GRAPH_NODE_CAP_MAX);
    let (nodes, edges, total_nodes, total_edges) =
        utopia_store::graph::overview(&state.pool, kb_id, limit, at, as_of).await?;
    Ok(Json(json!({
        "nodes": nodes, "edges": edges,
        "total_nodes": total_nodes, "total_edges": total_edges,
    })))
}

#[derive(Deserialize)]
pub struct NeighborhoodQuery {
    pub entity: Uuid,
    #[serde(default)]
    pub hops: Option<u8>,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub as_of: Option<String>,
}

pub async fn neighborhood(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<NeighborhoodQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let at = parse_instant("at", q.at.as_deref())?;
    let as_of = parse_instant("as_of", q.as_of.as_deref())?;
    let (nodes, edges) = utopia_store::graph::neighborhood(
        &state.pool,
        kb_id,
        q.entity,
        q.hops.unwrap_or(2),
        at,
        as_of,
    )
    .await?;
    Ok(Json(json!({ "nodes": nodes, "edges": edges })))
}

#[derive(Deserialize)]
pub struct EntitySearchQuery {
    pub q: String,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    /// Record axis: a search box for points on the graph during replay; results are ordered by `degree` as it was **then** (0019).
    /// Omitted means now
    #[serde(default)]
    pub as_of: Option<String>,
    /// Only exact-name matches (canonical name or current alias), without scanning the whole library; omitted means substring search
    #[serde(default)]
    pub exact: bool,
}

pub async fn search_entities(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(kb_id): Path<Uuid>,
    Query(query): Query<EntitySearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    // Validate the time first: a mistyped `as_of` with an empty query should also return 400, not appear to succeed
    let as_of = parse_instant("as_of", query.as_of.as_deref())?;
    if query.q.trim().is_empty() {
        return Ok(Json(json!({ "entities": [], "total": 0 })));
    }
    // Also return the total: "prefer splitting over merging" naturally produces a pile of same-named entities, and with a fixed ten
    // the one you want may not be among those ten at all, and the UI gives no hint of that
    let (entities, total) = utopia_store::graph::search_entities(
        &state.pool,
        kb_id,
        &query.q,
        query.limit.unwrap_or(10).clamp(1, 100),
        query.offset.unwrap_or(0).max(0),
        as_of,
        query.exact,
    )
    .await?;
    Ok(Json(json!({ "entities": entities, "total": total })))
}

#[derive(Deserialize)]
pub struct EntityDetailQuery {
    /// Record axis: when a node on the graph during replay is opened, the panel should show the facts as they were **then** (0019)
    #[serde(default)]
    pub as_of: Option<String>,
}

pub async fn entity_detail(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path((kb_id, entity_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<EntityDetailQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let as_of = parse_instant("as_of", q.as_of.as_deref())?;
    // The world axis is not filtered (at = None): the panel wants the whole timeline; the "holds right now" tier is split out by the front end
    // by holds_from / holds_to (0022)
    let (entity, facts) =
        utopia_store::graph::entity_detail(&state.pool, kb_id, entity_id, None, as_of).await?;
    // Derived facts that were inferred come back under their own key, not mixed into `facts`. The front end uses this to give them their own tier:
    // when a derived edge and an asserted edge share one list, the user cannot tell "this is written in the document" from
    // "the engine inferred this", and that is exactly how reasoning pollutes knowledge
    // The same as_of (#549): on the replay panel, the derived tier is also what was derived **then**
    let derived =
        utopia_store::reasoning::derived_for_entity(&state.pool, kb_id, entity_id, None, as_of)
            .await?;
    // Same-named ones are supplied **when the panel opens**, not only after a rename.
    //
    // Previously they only came back in the response to `update_entity`, so the "merge the same-named ones in" action
    // was reachable only after a rename first. Yet two Zhang Weis coexisting is a legitimate product of "prefer splitting over merging", not something
    // produced by renaming. The merge entry point should live where same-named entities are visible.
    let same_name =
        utopia_store::graph::same_name_peers(&state.pool, kb_id, entity_id, as_of).await?;
    // Derivations that did not land (0017 §3) also get their own key: they are not even in `derived_facts`
    let blocked =
        utopia_store::reasoning::blocked_for_entity(&state.pool, kb_id, entity_id, as_of).await?;
    // Names also get their own key (0041): the main name, short forms and former names, each with its source and validity period
    let names = utopia_store::names::for_entity(&state.pool, kb_id, entity_id, as_of).await?;
    Ok(Json(json!({
        "entity": entity, "facts": facts, "names": names,
        "derived": derived, "blocked": blocked, "same_name": same_name,
    })))
}

#[derive(Deserialize)]
pub struct EntityPatch {
    #[serde(default)]
    pub type_id: Option<Uuid>,
    #[serde(default)]
    pub canonical_name: Option<String>,
}

/// Manually correct an entity's type or name. Extraction gives a first guess; if it was wrong, the only option before was to re-extract the whole library.
///
/// Renaming onto an existing same-named entity is not blocked (two Zhang Weis is a legitimate product of "prefer splitting over merging"); after the change the same-named entities are reported back,
/// and the UI prompts whether to merge. Deciding whether they really are the same is a human's job.
pub async fn update_entity(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, entity_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EntityPatch>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    if req.type_id.is_none() && req.canonical_name.is_none() {
        return Err(AppError::invalid("nothing_to_update", "Nothing to update").into());
    }
    let (before, after) = utopia_store::graph::update_entity(
        &state.pool,
        kb_id,
        entity_id,
        req.type_id,
        req.canonical_name.as_deref(),
    )
    .await?;

    // The ledger snapshot is self-contained: even if the type is deleted later, this record remains readable.
    // P4 must aggregate "in one month 37 entities moved from Product to Concept", so the two actions are recorded separately.
    if before.type_key != after.type_key {
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "entity.retyped",
            "entity",
            Some(entity_id),
            json!({
                "name": after.name,
                "from": { "key": before.type_key, "label": before.type_label },
                "to": { "key": after.type_key, "label": after.type_label },
            }),
        )
        .await;
    }
    if before.name != after.name {
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "entity.renamed",
            "entity",
            Some(entity_id),
            json!({ "from": before.name, "to": after.name, "type": after.type_label }),
        )
        .await;
    }

    // The same-named list returned right after writing asks about the present, not the replay: pass None so it
    // takes the "today" path, consistent with what the panel shows when not replaying
    let peers = utopia_store::graph::same_name_peers(&state.pool, kb_id, entity_id, None).await?;
    state.emit_review(kb_id);
    Ok(Json(json!({ "entity": after, "same_name": peers })))
}

/// New validity interval for a fact. **Whole replacement, not a partial update**: the two ends define each other,
/// so "clear the end" and "leave the end alone this time" must be distinguishable, and in JSON a missing field and null
/// cannot be told apart. The UI submits the four values in the form, not a diff. `end_after_period` is the exception: omitted means
/// keep it; see that field.
#[derive(Deserialize)]
pub struct FactTimePatch {
    #[serde(default)]
    pub valid_from: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub valid_from_precision: Option<String>,
    #[serde(default)]
    pub valid_to: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub valid_to_precision: Option<String>,
    /// Why it changed. It goes into the ledger, not the graph: "we recorded it wrong before" and "new evidence says June" look
    /// the same in the ledger, and adding an enum type would make the entry heavier and the criterion fuzzy. If the distinction is ever really needed, promote it
    /// to a field from here
    #[serde(default)]
    pub note: Option<String>,
    /// Whether the end is the moment after the period stated in the source (`facts.end_after_period`, #994). **The only field
    /// that is kept when omitted**: a client that reads a value and sends it back (even one that does not know this field) must not
    /// change it. When not given, if the end (the value truncated to precision, and the precision) is unchanged and the start is still there, keep the stored mark; clear
    /// the start's kept moment and drop the mark (a row with no start is not marked on the first cut); other ends given are accepted as they are. The route never
    /// converts: what is stored and what is accepted are the boundary itself; displaying and converting by period is the UI's job
    #[serde(default)]
    pub end_after_period: Option<bool>,
}

// There is only one ladder (0024): the same column as the database CHECK, the extraction side and the display side
const DATE_PRECISIONS: [&str; 6] = utopia_store::graph::WORLD_PRECISIONS;

/// The interval before the change, used for the "not one character changed" check and for the before/after comparison in the ledger.
#[derive(sqlx::FromRow)]
struct FactInterval {
    valid_from: Option<chrono::DateTime<chrono::Utc>>,
    valid_from_precision: Option<String>,
    valid_to: Option<chrono::DateTime<chrono::Utc>>,
    valid_to_precision: Option<String>,
    end_after_period: bool,
    /// The predicate is declared as a state: the mark can only land on such rows
    declared_state: bool,
}

/// Explicitly asks for the mark, but this is not an end that can carry it
fn bad_end_after_period() -> AppError {
    AppError::invalid(
        "bad_end_after_period",
        "An end that names a period needs a state property, a start and an end date.",
    )
}

/// Mirrors the `facts` table's CHECK: precision at both ends; the mark must fit both ends (`period_mark_fits`; whether the attribute is a state is
/// checked after the row is read); plus one rule the database does not have: after truncation to each one's own precision, the start cannot be later than the end.
///
/// This is blocked here so the error says something readable. Leaving it to the database would give a constraint name, and these three states
/// (still ongoing / ended on an unknown day / ended on a given day) are exactly what is easiest to mix up in the ledger.
fn check_interval(p: &FactTimePatch) -> Result<(), AppError> {
    // Start end: no date means no precision, and there is no unknown tier: "started but not known when" and
    // "not known whether it started" are indistinguishable in this ledger (see the comment in migration 0003)
    match (&p.valid_from, p.valid_from_precision.as_deref()) {
        (Some(_), Some(prec)) if DATE_PRECISIONS.contains(&prec) => {}
        (None, None) => {}
        _ => return Err(AppError::invalid(
            "bad_valid_from",
            "A start date needs a precision of year, month, day, hour, minute or second, and a precision needs a date.",
        )),
    }
    // End end: a date always has a precision; no date can only mean still ongoing (all empty) or ended on an unknown day
    match (&p.valid_to, p.valid_to_precision.as_deref()) {
        (Some(_), Some(prec)) if DATE_PRECISIONS.contains(&prec) => {}
        (None, None) => {}
        (None, Some(prec)) if prec == utopia_store::graph::ENDED_UNKNOWN => {}
        _ => {
            return Err(AppError::invalid(
                "bad_valid_to",
                "An end date needs a precision of year, month, day, hour, minute or second. Leave the date empty for still going, or mark it ended with an unknown date.",
            ))
        }
    }
    // Explicitly asks for the mark: both ends must fit it. Whether the attribute is a state is known only after reading the row, so for now assume it is, and check only the ends
    if p.end_after_period == Some(true)
        && !utopia_store::graph::period_mark_fits(true, p.valid_from, p.valid_to, false)
    {
        return Err(bad_end_after_period());
    }
    // Compare the two ends after truncation to each one's precision: what is stored is the truncated value. Comparing with the original values, an hour-precision end 08:30 and a minute-precision
    // start 08:10 would pass, and after truncation 08:10 becomes 08:00, storing a reversed interval
    let truncate = utopia_store::graph::truncate_to;
    if let (Some(from), Some(to)) = (p.valid_from, p.valid_to) {
        if truncate(from, p.valid_from_precision.as_deref())
            > truncate(to, p.valid_to_precision.as_deref())
        {
            return Err(AppError::invalid(
                "interval_inverted",
                "The start of the interval is after its end.",
            ));
        }
    }
    Ok(())
}

/// Manually correct a fact's validity interval. Extraction gives a first guess; if it was wrong, the only option before was to delete the document and re-extract.
/// Wrong names can be fixed in Review (see `update_entity`), but there is no entry point for wrong times, and this
/// is exactly what the product sells.
///
/// Goes through the ledger rather than an in-place edit: `correct_interval` voids the old row and inserts a correction row with `supersedes`.
/// That way the edit itself appears on the record axis (the entity panel's History), and the world axis keeps only the new interval;
/// the old interval was our mistake, not the world's past, and showing both side by side would be read as a job changed twice.
pub async fn update_fact_time(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, fact_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<FactTimePatch>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    check_interval(&req)?;

    // Ownership and status check: only this KB's facts that have not been voided may be changed. Derived facts are not in this set:
    // their interval comes from their premises, so changing it means changing a computed value, which the next reasoning round would overwrite
    let before: Option<FactInterval> = sqlx::query_as(
        "SELECT f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                f.end_after_period, COALESCE(r.temporal = 'state', false) AS declared_state
         FROM facts f
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         WHERE f.id = $1 AND f.kb_id = $2 AND f.invalidated_at IS NULL
           AND f.derived_by_rule IS NULL",
    )
    .bind(fact_id)
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Db)?;
    let Some(before) = before else {
        return Err(AppError::NotFound.into());
    };
    // What is stored is the value truncated to precision: comparisons and mark checks are both made against it
    let truncate = utopia_store::graph::truncate_to;
    let from = req
        .valid_from
        .map(|t| truncate(t, req.valid_from_precision.as_deref()));
    let to = req
        .valid_to
        .map(|t| truncate(t, req.valid_to_precision.as_deref()));
    // The row has been read, so whether the attribute is a state is also known: the explicitly requested mark is checked again against the full rules
    if req.end_after_period == Some(true)
        && !utopia_store::graph::period_mark_fits(before.declared_state, from, to, false)
    {
        return Err(bad_end_after_period().into());
    }
    // No mark given: send back the stored end (time and precision). If this change's two ends still fit it, it was not touched, so keep it; clear the start
    // if it no longer fits, and drop it (the time is unchanged). The attribute is not checked again here: the stored mark was written under the attribute at that time, and a request that sends it back unchanged
    // does not rewrite this row
    let end_after_period = match req.end_after_period {
        Some(mark) => mark,
        None => {
            before.end_after_period
                && to == before.valid_to
                && req.valid_to_precision == before.valid_to_precision
                && utopia_store::graph::period_mark_fits(true, from, to, false)
        }
    };
    // Do not write a correction to the ledger if not one character changed: History would gain a "changed" row with no change in the interval,
    // and readers would go looking for a difference that does not exist
    if before.valid_from == from
        && before.valid_from_precision == req.valid_from_precision
        && before.valid_to == to
        && before.valid_to_precision == req.valid_to_precision
        && before.end_after_period == end_after_period
    {
        return Ok(Json(json!({ "ok": true, "unchanged": true })));
    }

    let snap = crate::api::review_routes::fact_snapshot(&state, kb_id, fact_id).await;
    let validity = utopia_store::graph::Validity {
        from: req.valid_from,
        from_precision: req.valid_from_precision.as_deref(),
        to: req.valid_to,
        to_precision: req.valid_to_precision.as_deref(),
        // Interval edited by a person: the dates are given directly, without passing through time parsing (0045, step 3, option A)
        from_grade: None,
        // The correction row inherits the anchor from the row it replaces (in correct_interval's SQL): changing the interval restates
        // the same evidence; it is not newer evidence
        attested_at: None,
        end_after_period,
    };
    let Some(corrected) =
        utopia_store::temporal::correct_interval(&state.pool, fact_id, validity).await?
    else {
        // Concurrency: another path has already rewritten or voided it. Make the UI refetch, rather than stacking the two corrections
        return Err(AppError::invalid(
            "fact_changed",
            "This fact was changed by someone else. Reload and try again.",
        )
        .into());
    };

    // After the change, reconcile again: moving the start earlier may hit the predecessor's open interval, which is exactly
    // what the temporal engine should handle. Reuse the path moved over: changing the interval is like changing subject/object, both make the uniqueness
    // invariant see the collision for the first time
    let report =
        utopia_store::temporal::reconcile_moved_facts(&state.pool, kb_id, &[corrected]).await?;

    if let Some(mut d) = snap {
        d["from"] = json!({
            "valid_from": before.valid_from.map(|t| t.to_rfc3339()),
            "valid_from_precision": before.valid_from_precision,
            "valid_to": before.valid_to.map(|t| t.to_rfc3339()),
            "valid_to_precision": before.valid_to_precision,
            "end_after_period": before.end_after_period,
        });
        d["to"] = json!({
            "valid_from": req.valid_from.map(|t| t.to_rfc3339()),
            "valid_from_precision": req.valid_from_precision,
            "valid_to": req.valid_to.map(|t| t.to_rfc3339()),
            "valid_to_precision": req.valid_to_precision,
            "end_after_period": end_after_period,
        });
        if let Some(note) = req.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            d["note"] = json!(note);
        }
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "fact.time_corrected",
            "fact",
            Some(fact_id),
            d,
        )
        .await;
    }

    state.emit_review(kb_id);
    Ok(Json(json!({
        "ok": true,
        "fact_id": corrected,
        "closed": report.corrected.len(),
        "conflicts": report.conflicts,
    })))
}

pub async fn fact_evidence(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path((kb_id, fact_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let evidence = utopia_store::graph::fact_evidence(&state.pool, kb_id, fact_id).await?;
    Ok(Json(json!({ "evidence": evidence })))
}

/// The proof of a derived fact (0002 R2): premises in derivation order, each with evidence, all the way back to the original sentence.
/// When the derivation is invalid or absent, `proof` is null: not an error; the UI falls back to the text premises on that basis
pub async fn derived_proof(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, derived_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let proof = utopia_store::reasoning::proof(&state.pool, kb_id, derived_id).await?;
    Ok(Json(json!({ "proof": proof })))
}

/// The proof chain of a derivation that did not land (0017 §3): the premises are in the `path` of that `derived_contradiction` violation,
/// expanded the same way as a landed one. When there is no violation, `steps` is null
pub async fn blocked_proof(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, violation_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let steps = utopia_store::reasoning::blocked_proof(&state.pool, kb_id, violation_id).await?;
    Ok(Json(json!({ "steps": steps })))
}

/// Manually trigger extraction (retry after failure, or extraction after a model is configured).
pub async fn extract(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(document_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, document_id).await?;
    require_kb(&state, &user, doc.kb_id, Role::Editor).await?;
    // The source says not to extract (a schema document, 0035 decision 7): say why, rather than queuing a
    // pipeline task that would skip it at that step
    if let Some(source_id) = doc.source_id {
        if !utopia_store::sources::get(&state.pool, source_id)
            .await?
            .extracts()
        {
            return Err(utopia_core::AppError::invalid(
                "source_not_extracted",
                "Documents under this source are searched, not extracted",
            )
            .into());
        }
    }
    // Manual trigger = forced full run: clear incremental markers, cancel running tasks, set queued, create the task, all in one transaction
    let job_id = utopia_store::documents::queue_extraction_one(&state.pool, document_id).await?;
    state.emit_document(doc.kb_id, document_id);
    Ok(Json(json!({ "job_id": job_id })))
}

/// Graph rebuild (clearing semantics, KB admin): clear the whole graph layer, then re-extract everything in full.
/// Division of labour with source-level re-extraction: re-extraction keeps existing decisions, a rebuild discards them, in exchange for
/// a deterministic replay of "current corpus × current ontology" (the cleanup for early dirty extractions and large ontology changes).
/// The decision ledger and adjudication cache are deliberately kept (see store::graph::purge_graph).
pub async fn rebuild(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Admin).await?;
    let (entities, facts) = utopia_store::graph::purge_graph(&state.pool, kb_id).await?;
    // The task is created in the same transaction as queue_extraction and the status; here it only pushes
    let ids = utopia_store::documents::queue_extraction(&state.pool, kb_id, None).await?;
    for id in &ids {
        state.emit_document(kb_id, *id);
    }
    state.emit_review(kb_id);
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "graph.rebuild",
        "kb",
        Some(kb_id),
        json!({ "entities_removed": entities, "facts_removed": facts, "documents": ids.len() }),
    )
    .await;
    Ok(Json(json!({
        "entities_removed": entities, "facts_removed": facts, "queued": ids.len()
    })))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub page: i64,
    #[serde(default = "default_history_per")]
    pub per: i64,
}

fn default_history_per() -> i64 {
    30
}

/// Cognitive change history of an entity (record timeline): when we believed this, and when we changed our mind.
/// Complements entity_detail (validity timeline, current only).
pub async fn entity_history(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path((kb_id, entity_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let per = q.per.clamp(1, 200);
    let page = q.page.max(0);
    let (events, total) =
        utopia_store::graph::entity_history(&state.pool, kb_id, entity_id, per, page * per).await?;
    Ok(Json(json!({ "events": events, "total": total })))
}
