//! Review queue API: resolve suspected duplicate pairs, plus low-confidence facts, merge log and rollback.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use uuid::Uuid;

use super::graph_routes::require_kb;
use crate::auth::{AuthUser, SessionOrToken};
use crate::error::ApiResult;
use crate::state::AppState;

#[cfg(test)]
#[path = "review_routes_phrase_tests.rs"]
mod phrase_tests;

#[cfg(test)]
#[path = "review_conflict_tests.rs"]
mod conflict_tests;

/// Items per page. **The server default, not a cap**: the front end may ask for fewer, and more is stopped by the clamp
const REVIEW_PAGE: i64 = 10;

/// Fact snapshot (for the decision ledger): after reject the fact disappears from the graph, so the ledger must show the text on its own.
/// Always taken before the action runs.
pub(crate) async fn fact_snapshot(
    state: &AppState,
    kb_id: Uuid,
    fact_id: Uuid,
) -> Option<serde_json::Value> {
    // The object may be an entity or a literal value; for a structured literal, prefer the summary (the same display convention as the queue card)
    let row: Option<(String, Option<String>, Option<String>, f32)> = sqlx::query_as(
        "SELECT s.canonical_name, COALESCE(r.label, fact_surface_predicate(f.id)),
                COALESCE(o.canonical_name, f.object_value ->> 'summary',
                         f.object_value #>> '{}'), f.confidence
         FROM facts f
         JOIN entities s ON s.id = f.subject_id
         LEFT JOIN relation_types r ON r.id = f.predicate_id
         LEFT JOIN entities o ON o.id = f.object_id
         WHERE f.id = $1 AND f.kb_id = $2",
    )
    .bind(fact_id)
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    row.map(|(s, p, o, c)| json!({ "subject": s, "predicate": p, "object": o, "confidence": c }))
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    /// Which tier to look at. Default duplicates
    #[serde(default)]
    pub queue: Option<String>,
    /// Only meaningful for duplicates: any (default) / same (both sides the same kind) / conflict (type conflict between the two sides)
    #[serde(default)]
    pub types: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Review queue: **counts and contents are fetched separately**.
///
/// Previously all eight queues came back in one go, each fixed at 100 items, and the front end paged them client-side, so the left-column
/// badge was a truncated number (the library has 164 low-confidence facts, the UI showed 100), and anything beyond page eleven
/// did not exist on screen.
///
/// Now: counts come back every time (eight COUNTs in one query, using the same WHERE as the list),
/// and the content returns only one page of the current tier. Switching tiers or paging each sends one request: the cost is a few more round-trips,
/// and the gain is that **the numbers no longer mislead**, and a library with a hundred thousand items to review can still be paged through to the end.
pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<ReviewQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let counts = utopia_store::review::counts(&state.pool, kb_id).await?;
    let queue = q.queue.as_deref().unwrap_or("duplicates");
    // The cap stops "limit=1000000 drags the library down", while leaving enough headroom for a full page
    let limit = q.limit.unwrap_or(REVIEW_PAGE).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let items = match queue {
        // Facts extracted from memory, awaiting a person's nod (0015). First: they are things the person said themselves
        "pending" => json!(utopia_store::pending::list(&state.pool, kb_id, limit, offset).await?),
        "duplicates" => json!(
            utopia_store::resolution::list_reviews(
                &state.pool,
                kb_id,
                utopia_store::resolution::TypeFilter::parse(q.types.as_deref())?,
                limit,
                offset,
            )
            .await?
        ),
        "conflicts" => {
            json!(utopia_store::temporal::list_conflicts(&state.pool, kb_id, limit, offset).await?)
        }
        "unconfirmed" => {
            json!(utopia_store::graph::stale_facts(&state.pool, kb_id, limit, offset).await?)
        }
        "lowconf" => json!(
            utopia_store::graph::low_confidence_facts(
                &state.pool,
                kb_id,
                utopia_store::review::LOW_CONFIDENCE_BELOW,
                limit,
                offset,
            )
            .await?
        ),
        "mappings" => {
            json!(utopia_store::mappings::proposed(&state.pool, kb_id, limit, offset).await?)
        }
        // Signatures and category words where the two aligners disagree (#725 alignment queue, 0044 decision 3)
        "alignment" => {
            json!(utopia_store::alignment_queue::list(&state.pool, kb_id, limit, offset).await?)
        }
        // Actions the correction agent leaves for a person (0044 decision 7): retract, change, or add ones the gate stopped
        "errata" => json!(utopia_store::errata::held(&state.pool, kb_id, limit, offset).await?),
        "violations" => {
            json!(
                utopia_store::reasoning::open_violations(&state.pool, kb_id, limit, offset).await?
            )
        }
        "defects" => {
            json!(utopia_store::reasoning::open_defects(&state.pool, kb_id, limit, offset).await?)
        }
        "merges" => {
            json!(utopia_store::resolution::list_merges(&state.pool, kb_id, limit, offset).await?)
        }
        // Every entry of the agent (0025): suggestions, automatic rulings and the human's answers, newest first
        "agent" => json!(utopia_store::governance::list(&state.pool, kb_id, limit, offset).await?),
        // An unrecognised tier name is reported as a contract error rather than silently returning empty: silent empty would let the front end
        // misspell one letter and then show "this tier is empty"
        other => {
            return Err(utopia_core::AppError::invalid(
                "unknown_queue",
                format!("no review queue named {other}"),
            )
            .into())
        }
    };

    Ok(Json(json!({
        "counts": counts,
        "queue": queue,
        "items": items,
    })))
}

#[derive(Deserialize)]
pub struct CloseFactBody {
    pub valid_to: chrono::DateTime<chrono::Utc>,
    /// year | month | day. **The number of digits given is the precision** (same convention as interval editing on the graph page);
    /// omitted means day: this is what old clients that send only a date mean
    #[serde(default)]
    pub valid_to_precision: Option<String>,
}

/// Precision given by the person on the world axis. It used to always be written as "day" ("the person picked a date in the UI"), so
/// "ended June 2023" could only be encoded as 1 June: filling in a definite value where you do not know is exactly
/// the problem the `facts.valid_to_precision` comment describes
fn world_precision(p: Option<&str>) -> Result<&'static str, utopia_core::AppError> {
    let Some(p) = p else { return Ok("day") };
    utopia_store::graph::WORLD_PRECISIONS
        .iter()
        .copied()
        .find(|w| *w == p)
        .ok_or_else(|| {
            utopia_core::AppError::invalid(
                "bad_precision",
                "A closing date's precision is year, month, day, hour, minute or second.",
            )
        })
}

/// Manually close a fact's validity interval ("this ended at some point"): goes through void + rewrite,
/// the same mechanism as automatic closing, so the ledger can be replayed.
pub async fn close_fact(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, fact_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<CloseFactBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    // Ownership and status check: only this KB's facts that are not voided and have an open interval may be closed
    let ok: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM facts
         WHERE id = $1 AND kb_id = $2 AND invalidated_at IS NULL AND valid_to IS NULL",
    )
    .bind(fact_id)
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(utopia_core::AppError::Db)?;
    if ok.is_none() {
        return Err(utopia_core::AppError::NotFound.into());
    }
    let precision = world_precision(body.valid_to_precision.as_deref())?;
    let snap = fact_snapshot(&state, kb_id, fact_id).await;
    utopia_store::temporal::close_superseded(&state.pool, fact_id, body.valid_to, precision)
        .await?;
    if let Some(mut d) = snap {
        d["valid_to"] = json!(body.valid_to.to_rfc3339());
        d["valid_to_precision"] = json!(precision);
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "fact.close",
            "fact",
            Some(fact_id),
            d,
        )
        .await;
    }
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ConflictBody {
    /// close | retime_new | keep | reject_new
    pub action: String,
    /// Required when closing and the new fact has no start
    #[serde(default)]
    pub close_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Precision of `close_at` (year to second); omitted means day
    #[serde(default)]
    pub close_at_precision: Option<String>,
    /// Required for retime_new: only changes the new fact's start; the end is still set by the source text or the timeline.
    #[serde(default)]
    pub new_from: Option<chrono::DateTime<chrono::Utc>>,
    /// Precision of `new_from` (year to second); omitted means day.
    #[serde(default)]
    pub new_from_precision: Option<String>,
}

/// Temporal conflict ruling (S3: those automatic closing is unsure about).
pub async fn resolve_conflict(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, conflict_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<ConflictBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let precision = world_precision(body.close_at_precision.as_deref())?;
    let new_precision = world_precision(body.new_from_precision.as_deref())?;
    utopia_store::temporal::resolve_review_conflict(
        &state.pool,
        kb_id,
        conflict_id,
        utopia_store::temporal::ConflictDecision {
            action: &body.action,
            close_at: body.close_at,
            close_precision: precision,
            new_from: body.new_from,
            new_precision,
        },
        user.id,
    )
    .await?;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct DecideBody {
    /// merge | keep
    pub action: String,
    /// The sentence written when a person decides (0026): what made you decide this. Nullable
    #[serde(default)]
    pub rationale: Option<String>,
}

pub async fn decide(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, review_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<DecideBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let snap: Option<(String, String, f32)> = sqlx::query_as(
        "SELECT a.canonical_name, b.canonical_name, rr.score
         FROM resolution_reviews rr
         JOIN entities a ON a.id = rr.left_id
         JOIN entities b ON b.id = rr.right_id
         WHERE rr.id = $1 AND rr.kb_id = $2",
    )
    .bind(review_id)
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    utopia_store::resolution::decide_review(
        &state.pool,
        kb_id,
        review_id,
        &body.action,
        user.id,
        body.rationale.as_deref(),
    )
    .await?;
    if let Some((l, r, score)) = snap {
        // The reason also goes into the ledger: precedents are read from here, not from resolution_reviews
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            if body.action == "merge" {
                "review.merge"
            } else {
                "review.keep"
            },
            "review",
            Some(review_id),
            json!({ "left": l, "right": r, "score": score, "why": body.rationale }),
        )
        .await;
    }
    crate::governance::after_human_decision(
        &state,
        kb_id,
        &[review_id],
        Some(&body.action),
        user.id,
    )
    .await;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct AgentAnswerBody {
    /// merge | keep answers a suggestion (same as the suggestion means accept, different means overrule);
    /// revert withdraws an automatic merge; merge can also overturn an automatic split
    pub action: String,
    /// What made you answer this (0026). Especially worth writing when overruling the agent: that sentence becomes the next precedent
    #[serde(default)]
    pub rationale: Option<String>,
}

/// A person answers one of the agent's entries (0025). The answer takes the human ruling path: decided_by is this person,
/// and the ledger records this person; it becomes the precedent for the next round
pub async fn agent_answer(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, decision_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<AgentAnswerBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let d = utopia_store::governance::get(&state.pool, kb_id, decision_id).await?;
    let (l, r) = (
        d.left.clone().unwrap_or_default(),
        d.right.clone().unwrap_or_default(),
    );
    match (d.status.as_str(), body.action.as_str()) {
        ("proposed", act @ ("merge" | "keep")) => {
            utopia_store::resolution::decide_review(
                &state.pool,
                kb_id,
                d.target_id,
                act,
                user.id,
                body.rationale.as_deref(),
            )
            .await?;
            let status = if act == d.action {
                "accepted"
            } else {
                "overridden"
            };
            utopia_store::governance::settle(&state.pool, kb_id, decision_id, status, user.id)
                .await?;
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                if act == "merge" { "review.merge" } else { "review.keep" },
                "review",
                Some(d.target_id),
                json!({ "left": l, "right": r, "agent_decision": decision_id, "agent_action": d.action,
                        "why": body.rationale }),
            )
            .await;
        }
        ("applied", "revert") => {
            let merge_id = d.merge_id.ok_or_else(|| {
                utopia_core::AppError::invalid(
                    "not_a_merge",
                    "Only an applied merge can be reverted; a pair kept apart can be merged instead.",
                )
            })?;
            utopia_store::resolution::revert_merge(&state.pool, kb_id, merge_id).await?;
            utopia_store::governance::settle(&state.pool, kb_id, decision_id, "reverted", user.id)
                .await?;
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                "merge.revert",
                "merge",
                Some(merge_id),
                json!({ "source": l, "target": r, "agent_decision": decision_id }),
            )
            .await;
            // Withdrawing the agent's merge has gone to two: a fuse
            crate::governance::fuse(&state, kb_id).await;
        }
        ("applied", "merge") if d.action == "keep" => {
            let (left_id, right_id): (Uuid, Uuid) = sqlx::query_as(
                "SELECT left_id, right_id FROM resolution_reviews WHERE id = $1 AND kb_id = $2",
            )
            .bind(d.target_id)
            .bind(kb_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(utopia_core::AppError::from)?
            .ok_or(utopia_core::AppError::NotFound)?;
            let (target, source) =
                utopia_store::resolution::merge_direction(&state.pool, left_id, right_id).await?;
            let merge_id = utopia_store::resolution::merge_entities(
                &state.pool,
                kb_id,
                source,
                target,
                Some(user.id),
                "overrode agent keep",
            )
            .await?;
            utopia_store::governance::settle(
                &state.pool,
                kb_id,
                decision_id,
                "overridden",
                user.id,
            )
            .await?;
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                "merge.manual",
                "merge",
                Some(merge_id),
                json!({ "source": l, "target": r, "agent_decision": decision_id }),
            )
            .await;
        }
        (status, action) => {
            return Err(utopia_core::AppError::invalid(
                "agent_answer",
                format!("cannot {action} an agent decision that is {status}"),
            )
            .into())
        }
    }
    crate::governance::after_human_decision(&state, kb_id, &[d.target_id], None, user.id).await;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct BatchBody {
    pub ids: Vec<Uuid>,
    /// merge | keep, one action for the whole batch
    pub action: String,
    /// One reason for the whole batch (0026): a batch with one action is most likely based on one reason too
    #[serde(default)]
    pub rationale: Option<String>,
}

/// Maximum number of ids in one batch: enough for a "select all" on a screen, and stops one request from locking the library for minutes
const BATCH_MAX: usize = 500;

/// Batch ruling (#428): the same path as single `decide`; each item merges or splits on its own and records its own
/// ledger entry, and what comes back is a per-item result. The front-end refresh is pushed only once.
pub async fn batch(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(body): Json<BatchBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    if body.ids.is_empty() || body.ids.len() > BATCH_MAX {
        return Err(utopia_core::AppError::invalid(
            "batch_size",
            format!("between 1 and {BATCH_MAX} ids per batch"),
        )
        .into());
    }
    // The names and scores the ledger needs are taken before the ruling; after a merge the source side's names are no longer where they were
    let snaps: Vec<(Uuid, String, String, f32)> = sqlx::query_as(
        "SELECT rr.id, a.canonical_name, b.canonical_name, rr.score
         FROM resolution_reviews rr
         JOIN entities a ON a.id = rr.left_id
         JOIN entities b ON b.id = rr.right_id
         WHERE rr.kb_id = $1 AND rr.id = ANY($2)",
    )
    .bind(kb_id)
    .bind(&body.ids)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let outcomes = utopia_store::resolution::decide_reviews(
        &state.pool,
        kb_id,
        &body.ids,
        &body.action,
        user.id,
        body.rationale.as_deref(),
    )
    .await?;
    let action = if body.action == "merge" {
        "review.merge"
    } else {
        "review.keep"
    };
    for o in outcomes.iter().filter(|o| o.error.is_none()) {
        if let Some((_, l, r, score)) = snaps.iter().find(|s| s.0 == o.id) {
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                action,
                "review",
                Some(o.id),
                json!({ "left": l, "right": r, "score": score, "batch": body.ids.len(),
                        "why": body.rationale }),
            )
            .await;
        }
    }
    let decided_ids: Vec<Uuid> = outcomes
        .iter()
        .filter(|o| o.error.is_none())
        .map(|o| o.id)
        .collect();
    crate::governance::after_human_decision(
        &state,
        kb_id,
        &decided_ids,
        Some(&body.action),
        user.id,
    )
    .await;
    state.emit_review(kb_id);
    let decided = decided_ids.len();
    Ok(Json(json!({ "decided": decided, "outcomes": outcomes })))
}

pub async fn confirm_fact(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, fact_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let snap = fact_snapshot(&state, kb_id, fact_id).await;
    utopia_store::graph::confirm_fact(&state.pool, kb_id, fact_id).await?;
    if let Some(d) = snap {
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "fact.confirm",
            "fact",
            Some(fact_id),
            d,
        )
        .await;
    }
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn reject_fact(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, fact_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let snap = fact_snapshot(&state, kb_id, fact_id).await;
    utopia_store::graph::reject_fact(&state.pool, kb_id, fact_id).await?;
    if let Some(d) = snap {
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "fact.reject",
            "fact",
            Some(fact_id),
            d,
        )
        .await;
    }
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn revert_merge(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, merge_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let snap: Option<(String, String)> = sqlx::query_as(
        "SELECT s.canonical_name, t.canonical_name
         FROM entity_merges m
         JOIN entities s ON s.id = m.source_id
         JOIN entities t ON t.id = m.target_id
         WHERE m.id = $1 AND m.kb_id = $2",
    )
    .bind(merge_id)
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    utopia_store::resolution::revert_merge(&state.pool, kb_id, merge_id).await?;
    if let Some((s, t)) = snap {
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "merge.revert",
            "merge",
            Some(merge_id),
            json!({ "source": s, "target": t }),
        )
        .await;
    }
    // Withdrawing a merge that the agent itself ruled on: that row is recorded as reverted, and the fuse is checked (0025)
    crate::governance::after_revert(&state, kb_id, merge_id, user.id).await;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ManualMergeBody {
    pub source: Uuid,
    pub target: Uuid,
    /// What made you merge them (0026). Nullable
    #[serde(default)]
    pub rationale: Option<String>,
}

/// Manual merge (the "Merge into…" entry on the entity panel).
pub async fn manual_merge(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(body): Json<ManualMergeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let snap: Option<(String,)> = sqlx::query_as(
        "SELECT s.canonical_name || ' → ' || t.canonical_name
         FROM entities s, entities t WHERE s.id = $1 AND t.id = $2",
    )
    .bind(body.source)
    .bind(body.target)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let why = body
        .rationale
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let merge_id = utopia_store::resolution::merge_entities(
        &state.pool,
        kb_id,
        body.source,
        body.target,
        Some(user.id),
        why.unwrap_or("manual merge"),
    )
    .await?;
    if let Some((pair,)) = snap {
        let (s, t) = pair.split_once(" → ").unwrap_or((pair.as_str(), ""));
        let _ = utopia_store::audit::record(
            &state.pool,
            Some(kb_id),
            user.id,
            "merge.manual",
            "merge",
            Some(merge_id),
            json!({ "source": s, "target": t, "why": why }),
        )
        .await;
    }
    state.emit_review(kb_id);
    Ok(Json(json!({ "merge_id": merge_id })))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub page: i64,
    #[serde(default = "default_history_per")]
    pub per: i64,
}

fn default_history_per() -> i64 {
    20
}

/// Decision ledger: audit events of the review domain, paged server-side.
pub async fn history(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let per = q.per.clamp(1, 100);
    let page = q.page.max(0);
    let (events, total) =
        utopia_store::audit::review_history(&state.pool, kb_id, per, page * per).await?;
    Ok(Json(json!({ "events": events, "total": total })))
}

/// Overview of the review desk (#377): what is waiting, what has been handled, and the library's state. Three aggregates, returned in one response.
pub async fn summary(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<utopia_core::models::ReviewSummary>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    Ok(Json(
        utopia_store::review_summary::summary(&state.pool, kb_id).await?,
    ))
}

#[derive(Deserialize)]
pub struct DecideMappingReq {
    /// confirmed | rejected
    pub status: String,
}

/// Take a position on a semantic-layer mapping (0011).
///
/// **Changing status does not delete the row**: the confirmation happened, and so did the rejection. A rejection recorded at the time is useful in itself:
/// the next exploration round will compute the rejected pair again, and `propose`'s `WHERE status = 'proposed'`
/// uses this so that it is not sent back to waiting.
pub async fn decide_mapping(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, mapping_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DecideMappingReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    if !matches!(req.status.as_str(), "confirmed" | "rejected") {
        return Err(utopia_core::AppError::invalid(
            "bad_status",
            "status must be confirmed or rejected",
        )
        .into());
    }
    utopia_store::mappings::decide(&state.pool, kb_id, mapping_id, &req.status, user.id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "mapping.decided",
        "concept_mapping",
        Some(mapping_id),
        json!({ "status": req.status }),
    )
    .await;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct DecideViolationReq {
    /// fact_retracted | fact_closed | axiom_relaxed | accepted
    pub resolution: String,
    /// Required when `fact_closed`: the day on which the old assertion ends
    #[serde(default)]
    pub close_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Precision of `close_at` (year | month | day); omitted means day
    #[serde(default)]
    pub close_at_precision: Option<String>,
    /// Which one to retract when `fact_retracted`: required for two-fact and in-cycle violations, can be omitted for single-fact (#202)
    #[serde(default)]
    pub fact_id: Option<Uuid>,
}

/// A person rules on one axiom violation.
///
/// **Three exits, not two.** `axiom_relaxed` is unique to this tier: the contradiction may lie in the definition
/// rather than the data. The ontology the user imported declared some property as antisymmetric, while the relation in their own corpus
/// is in fact bidirectional. In that case it is the ontology that should change, not twenty facts.
///
/// **"The data is wrong", so really retract the fact** (#202): previously only the decision was recorded, the fact still lived in the graph, and the queue
/// no longer raised it. Which one to retract is named by the request (`fact_id`); the kind can be omitted for a single fact. After retracting, the check is recomputed,
/// and the other violations entangled with that fact are cleared too. Changing axioms still goes through the ontology page; that is another page's business.
pub async fn decide_violation(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, violation_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DecideViolationReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    if !matches!(
        req.resolution.as_str(),
        "fact_retracted" | "fact_closed" | "axiom_relaxed" | "accepted"
    ) {
        return Err(utopia_core::AppError::invalid(
            "bad_resolution",
            "resolution can only be fact_retracted, fact_closed, axiom_relaxed or accepted",
        )
        .into());
    }
    let row: Option<(String, Uuid, Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT kind, left_fact, right_fact, path FROM axiom_violations
          WHERE id = $1 AND kb_id = $2 AND status = 'open'",
    )
    .bind(violation_id)
    .bind(kb_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(utopia_core::AppError::Db)?;
    let Some((kind, left, right, path)) = row else {
        return Err(utopia_core::AppError::NotFound.into());
    };
    let repaired = kind == "derived_contradiction";
    // The retraction path itself marks the violation resolved; the other exits are recorded together below
    let mut decided = false;
    match (repaired, req.resolution.as_str()) {
        (_, "fact_retracted") => {
            let Some(target) =
                utopia_store::reasoning::pick_retraction(left, right, &path, req.fact_id)
            else {
                return Err(utopia_core::AppError::invalid(
                    "fact_required",
                    "this violation involves several facts: say which one to retract, and it must be one of those listed",
                )
                .into());
            };
            let snap = fact_snapshot(&state, kb_id, target).await;
            utopia_store::reasoning::retract_from_violation(
                &state.pool,
                kb_id,
                violation_id,
                Some(target),
                user.id,
            )
            .await?;
            decided = true;
            if let Some(d) = snap {
                let _ = utopia_store::audit::record(
                    &state.pool,
                    Some(kb_id),
                    user.id,
                    "fact.reject",
                    "fact",
                    Some(target),
                    d,
                )
                .await;
            }
        }
        (true, "fact_closed") => {
            let Some(at) = req.close_at else {
                return Err(utopia_core::AppError::invalid(
                    "close_at_required",
                    "fact_closed needs an end date",
                )
                .into());
            };
            let open: Option<(Uuid,)> = sqlx::query_as(
                "SELECT id FROM facts
                  WHERE id = $1 AND invalidated_at IS NULL AND valid_to IS NULL",
            )
            .bind(left)
            .fetch_optional(&state.pool)
            .await
            .map_err(utopia_core::AppError::Db)?;
            if open.is_none() {
                return Err(utopia_core::AppError::invalid(
                    "not_open",
                    "this assertion already has an end date, or has been retracted",
                )
                .into());
            }
            let precision = world_precision(req.close_at_precision.as_deref())?;
            let snap = fact_snapshot(&state, kb_id, left).await;
            utopia_store::temporal::close_superseded(&state.pool, left, at, precision).await?;
            if let Some(mut d) = snap {
                d["valid_to"] = json!(at.to_rfc3339());
                d["valid_to_precision"] = json!(precision);
                let _ = utopia_store::audit::record(
                    &state.pool,
                    Some(kb_id),
                    user.id,
                    "fact.close",
                    "fact",
                    Some(left),
                    d,
                )
                .await;
            }
        }
        (false, "fact_closed") => {
            return Err(utopia_core::AppError::invalid(
                "bad_resolution",
                "fact_closed is only for derived_contradiction",
            )
            .into());
        }
        _ => {}
    }
    if !decided {
        utopia_store::reasoning::decide(&state.pool, kb_id, violation_id, &req.resolution, user.id)
            .await?;
    }
    // Once that path is cleared, let the derivations land so a person need not click "derive" again. Retraction and closing have moved assertions away,
    // and acceptance is let through in materialize
    if repaired {
        utopia_store::reasoning::materialize(&state.pool, kb_id).await?;
    }
    // A retracted fact may still hang off other violations: recompute, those rows are cleared along with it, so the queue leaves no stale entries
    if req.resolution == "fact_retracted" {
        utopia_store::reasoning::run(&state.pool, kb_id).await?;
    }
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "violation.decided",
        "axiom_violation",
        Some(violation_id),
        json!({ "resolution": req.resolution }),
    )
    .await;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

/// Run a consistency check manually.
///
/// **Run synchronously rather than queuing a job**: this is pure computation, with no model call and no network. A library of tens of thousands
/// of facts runs in milliseconds. Queuing it would only make a person click a button and stare at "queued",
/// whereas what the queue really solves is "this work takes minutes".
///
/// `predicates_with_axioms` is returned to the front end too: **zero and zero mean different things**. With no axioms,
/// the conclusion is "no criteria", and the UI should say "first import an ontology with axioms", not "no contradictions found".
pub async fn run_consistency_check(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let report = utopia_store::reasoning::run(&state.pool, kb_id).await?;
    // The ontology's own self-consistency is computed too. Both use the same criteria (both are axioms the ontology declares); two buttons
    // would only make a person click twice
    let onto = utopia_store::reasoning::check_ontology(&state.pool, kb_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "consistency.checked",
        "knowledge_base",
        Some(kb_id),
        json!({
            "edges": report.edges,
            "predicates_with_axioms": report.predicates_with_axioms,
            "found": report.found,
            "inserted": report.inserted,
            "cleared": report.cleared,
            "cycles_capped": report.cycles_capped,
            "defects_found": onto.found,
        }),
    )
    .await;
    state.emit_review(kb_id);
    Ok(Json(json!({
        "edges": report.edges,
        "predicates_with_axioms": report.predicates_with_axioms,
        "found": report.found,
        "inserted": report.inserted,
        "cleared": report.cleared,
        // The number of predicates whose cycle search did not finish (#642): when non-zero, the cycles reported are only partial, and the UI should say
        // "too many cycles for this predicate, check the axioms first", rather than let a person think that is all of them
        "cycles_capped": report.cycles_capped,
        // The ontology's own tier is returned separately. **Not added into found**: the two counts are not the same kind of thing,
        // and after adding them "3 contradictions" could mean three facts clash, or the ontology itself has three reversed declarations
        "classes": onto.classes,
        "defects_found": onto.found,
        "defects_new": onto.inserted,
    })))
}

#[derive(Deserialize)]
pub struct DecideDefectReq {
    /// fixed | accepted
    pub resolution: String,
}

/// A person takes a position on one ontology defect.
///
/// **Two exits, not three**: an ontology defect never looked at the data, so there is no "the data is wrong" option.
pub async fn decide_defect(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, defect_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DecideDefectReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    if !matches!(req.resolution.as_str(), "fixed" | "accepted") {
        return Err(utopia_core::AppError::invalid(
            "bad_resolution",
            "resolution can only be fixed or accepted",
        )
        .into());
    }
    utopia_store::reasoning::decide_defect(&state.pool, kb_id, defect_id, &req.resolution, user.id)
        .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "defect.decided",
        "ontology_defect",
        Some(defect_id),
        json!({ "resolution": req.resolution }),
    )
    .await;
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

/// Run one round of inference (R1).
///
/// **Governed by the `materialize_inferences` switch.** Off by default, because this step adds things to the graph,
/// and 0001 criterion 2 says "the ontology guides, it does not enforce": a declaration may be wrong, so the graph should not
/// be changed by it before the user has taken a position. When the switch is off, do not skip silently: return an explicit error so the UI can say why nothing moved.
///
/// Runs synchronously, for the same reason as the consistency check: pure computation, with no model call and no network.
#[derive(Deserialize)]
pub struct PendingQuery {
    pub chunk_id: Uuid,
}

/// All the pending items from one memory extraction; the confirmation card in the conversation fetches these (0015).
/// Viewers can see them too: they see the proposals but no buttons, the same convention as the Review page
pub async fn pending_for_chunk(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<PendingQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let items = utopia_store::pending::for_chunk(&state.pool, kb_id, q.chunk_id).await?;
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub struct DecidePendingBody {
    /// `confirm` goes into the ledger; `reject` is recorded in `rejected_facts`, and the next extraction round no longer raises it
    pub action: String,
}

/// A person nods or shakes their head at one pending fact.
///
/// Both actions go into the decision ledger, with self-contained snapshots: after the row is deleted, the ledger can still show what was confirmed at the time.
/// Confirmation takes the same path as extraction (fact + evidence + temporal reconciliation), so after "Mira handed it to Devin" is nodded through,
/// Mira's fact is closed just as when it was extracted from a document
pub async fn decide_pending(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, pending_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<DecidePendingBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    match body.action.as_str() {
        "confirm" => {
            let done = utopia_store::pending::confirm(&state.pool, kb_id, pending_id).await?;
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                "fact.nod_confirmed",
                "fact",
                Some(done.fact_id),
                done.snapshot,
            )
            .await;
            // The open statement that was nodded through carries a time word: time parsing is re-run on the document (0045),
            // idempotent, so the same document queued twice does not repeat
            let _ = utopia_store::jobs::enqueue_unless_queued(
                &state.pool,
                "resolve_time",
                json!({ "document_id": done.document_id }),
            )
            .await;
            state.emit_pending(kb_id);
            state.emit_review(kb_id);
            state.emit_graph(kb_id);
            Ok(Json(json!({
                "ok": true,
                "fact_id": done.fact_id,
                "created": done.created,
                "conflicts": done.conflicts,
            })))
        }
        "reject" => {
            let snap = utopia_store::pending::reject(&state.pool, kb_id, pending_id, Some(user.id))
                .await?;
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                "fact.nod_rejected",
                "pending_fact",
                Some(pending_id),
                snap,
            )
            .await;
            state.emit_pending(kb_id);
            state.emit_review(kb_id);
            Ok(Json(json!({ "ok": true })))
        }
        other => Err(utopia_core::AppError::invalid(
            "unknown_action",
            format!("action must be confirm or reject, got {other}"),
        )
        .into()),
    }
}

pub async fn run_inference(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let on: bool =
        sqlx::query_scalar("SELECT materialize_inferences FROM knowledge_bases WHERE id = $1")
            .bind(kb_id)
            .fetch_one(&state.pool)
            .await
            .map_err(utopia_core::AppError::Db)?;
    if !on {
        return Err(utopia_core::AppError::invalid(
            "inference_off",
            "materialized inference is off for this knowledge base",
        )
        .into());
    }
    let report = utopia_store::reasoning::materialize(&state.pool, kb_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "inference.materialized",
        "knowledge_base",
        Some(kb_id),
        json!({
            "rules": report.rules,
            "edges": report.edges,
            "derived": report.derived,
            "inserted": report.inserted,
            "invalidated": report.invalidated,
            "capped": report.capped,
        }),
    )
    .await;
    state.emit_graph(kb_id);
    Ok(Json(json!({
        "rules": report.rules,
        "edges": report.edges,
        "derived": report.derived,
        "inserted": report.inserted,
        "invalidated": report.invalidated,
        "capped": report.capped,
    })))
}

#[derive(Deserialize)]
pub struct DecideAlignmentPhraseReq {
    /// The attribute's key; empty = no attribute matched, the statement stays in the open graph
    #[serde(default)]
    pub property: Option<String>,
    /// forward | reverse; if an attribute is given, a direction must be given too
    #[serde(default)]
    pub direction: Option<String>,
    /// start | end | none: under a state attribute, whether the moment a statement carries marks a start, an end, or neither (#966).
    /// Not given = unknown: such a statement does not count as a typed row, and its signature stays in the queue waiting for a person to fill it in
    #[serde(default)]
    pub marks: Option<String>,
}

/// A person decides which attribute a short-phrase signature binds to (#725 alignment queue). Written as the person's ruling, the agent does not change it again;
/// the typed graph is recomputed at once with the new binding.
#[derive(Deserialize)]
pub struct DecideAlignmentRuleReq {
    pub approve: bool,
}

/// A person approves or rejects an entailment rule (0044 decision 3, fifth slice). It is the same set as the phrase decision: the decision and its follow-up work
/// are submitted together, answering 202 with a job id. If approving and it needs reading, `read_phrases` is queued first (it fills the cache and then queues materialisation itself);
/// otherwise materialisation is queued directly. Rejection also needs recomputation, so the implied rows have to be retired
pub async fn decide_alignment_rule(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, rule_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DecideAlignmentRuleReq>,
) -> ApiResult<(axum::http::StatusCode, Json<serde_json::Value>)> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let rule = utopia_store::implication_rules::get(&state.pool, kb_id, rule_id)
        .await?
        .ok_or(utopia_core::AppError::NotFound)?;
    let votes = json!({ "person": if req.approve { "approve" } else { "reject" } });
    let job_id = utopia_store::implication_rules::decide_with_delivery(
        &state.pool,
        kb_id,
        rule_id,
        req.approve,
        &votes,
    )
    .await?
    .ok_or(utopia_core::AppError::NotFound)?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "alignment.rule_decided",
        "implication_rule",
        Some(rule_id),
        json!({ "trigger": rule.trigger, "phrase": rule.phrase, "reading": rule.reading,
                "approve": req.approve, "job_id": job_id }),
    )
    .await;
    state.emit_review(kb_id);
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "job_id": job_id, "status": "accepted" })),
    ))
}

pub async fn decide_alignment_phrase(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, binding_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DecideAlignmentPhraseReq>,
) -> ApiResult<(axum::http::StatusCode, Json<serde_json::Value>)> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let sig = utopia_store::phrase_bindings::signature_of(&state.pool, kb_id, binding_id)
        .await?
        .ok_or(utopia_core::AppError::NotFound)?;
    let property = match req
        .property
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(key) => {
            let found = utopia_store::ontology::relation_type_views(&state.pool, kb_id)
                .await?
                .into_iter()
                .find(|p| p.key == key)
                .map(|p| (p.id, p.temporal == "state"))
                .ok_or_else(|| {
                    utopia_core::AppError::invalid("unknown_property", "no property with that key")
                })?;
            Some(found)
        }
    };
    let marks = match (
        property,
        req.marks
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    ) {
        (_, None) => None,
        (Some((_, true)), Some(m @ ("start" | "end" | "none"))) => Some(m),
        (Some((_, true)), Some(_)) => {
            return Err(utopia_core::AppError::invalid(
                "unknown_marks",
                "marks is one of start, end or none",
            )
            .into())
        }
        (_, Some(_)) => {
            return Err(utopia_core::AppError::invalid(
                "marks_needs_state",
                "only a state property says which end a moment marks",
            )
            .into())
        }
    };
    let property = property.map(|(id, _)| id);
    let direction = match (property, req.direction.as_deref()) {
        (None, _) => None,
        (Some(_), Some(d @ ("forward" | "reverse"))) => Some(d),
        (Some(_), _) => {
            return Err(utopia_core::AppError::invalid(
                "direction_required",
                "a bound signature needs a direction: forward or reverse",
            )
            .into())
        }
    };
    let votes = json!({ "person": { "property": req.property, "direction": direction,
                                    "marks": marks } });
    // The decision and its recompute job are submitted together (0051). Recomputing here synchronously is **no longer** done: waiting for the materialisation lock holds a
    // connection from the pool, and the alignment that is already running may have read the last pass, and nobody there projects this decision. A job
    // is visible only after the decision is submitted, and the worker reads the current binding; what the screen waits on is the `review` / `graph` event
    let job_id = utopia_store::phrase_bindings::decide_with_delivery(
        &state.pool,
        kb_id,
        &sig,
        utopia_store::phrase_bindings::Decision {
            relation_type_id: property,
            direction,
            status: if property.is_some() { "bound" } else { "none" },
            votes: &votes,
            decided_by: "person",
            basis: None,
            marks,
            marks_asked: false,
        },
    )
    .await?
    .ok_or_else(|| utopia_core::AppError::Conflict("the decision was not written".into()))?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "alignment.phrase_decided",
        "phrase_binding",
        Some(binding_id),
        json!({ "phrase": sig.phrase, "property": req.property, "direction": direction,
                "marks": marks, "job_id": job_id }),
    )
    .await;
    state.emit_review(kb_id);
    // 202: accepted, the projection is on its way. Do not make up a typed: {added: 0}; that is not something this request knows
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "job_id": job_id, "status": "accepted" })),
    ))
}

#[derive(Deserialize)]
pub struct DecideAlignmentKindWordReq {
    /// Key of the class; empty = no class matched
    #[serde(default)]
    pub class: Option<String>,
}

/// A person binds a category word to a class (#725 alignment queue). The bound class is written onto the entities under it; when either side's class changes,
/// the short-phrase signature changes with it, so alignment is queued again in the same transaction as the decision (`decide_and_apply_human`),
/// queuing it after the submission would leave only the decision if the process exits between the two steps.
///
/// A write token can also decide (0014 decision 6): the decision is recorded under the name of the person who issued the token, just as with a click in the web page
pub async fn decide_alignment_kind_word(
    State(state): State<AppState>,
    caller: SessionOrToken,
    Path((kb_id, kind_word)): Path<(Uuid, String)>,
    Json(req): Json<DecideAlignmentKindWordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    caller.require_write(Some(kb_id))?;
    let token_id = caller.token_id();
    let user = caller.user;
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let class = match req
        .class
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(key) => Some(
            utopia_store::graph::entity_types(&state.pool, kb_id)
                .await?
                .into_iter()
                .find(|c| c.key == key)
                .map(|c| c.id)
                .ok_or_else(|| {
                    utopia_core::AppError::invalid("unknown_class", "no class with that key")
                })?,
        ),
    };
    let votes = json!({ "person": req.class });
    let written = utopia_store::type_bindings::decide_and_apply_human(
        &state.pool,
        kb_id,
        &kind_word,
        class,
        &votes,
    )
    .await?;
    if !written {
        return Err(utopia_core::AppError::NotFound.into());
    }
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "alignment.kind_word_decided",
        "type_binding",
        None,
        utopia_store::audit::via_token(
            json!({ "kind_word": kind_word, "class": req.class }),
            token_id,
        ),
    )
    .await;
    state.emit_review(kb_id);
    state.emit_graph(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct DecideErrataReq {
    pub approve: bool,
}

/// A person answers an entry left by the correction agent (0044 decision 7): approve and that action is carried out; reject and only one entry is recorded
pub async fn decide_errata(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, action_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<DecideErrataReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let found =
        utopia_store::errata::decide_held(&state.pool, kb_id, action_id, req.approve, user.id)
            .await?;
    if !found {
        return Err(utopia_core::AppError::NotFound.into());
    }
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "errata.decided",
        "errata_action",
        Some(action_id),
        json!({ "approve": req.approve }),
    )
    .await;
    state.emit_review(kb_id);
    state.emit_graph(kb_id);
    Ok(Json(json!({ "ok": true })))
}
