//! Ontology editor API: type/relation CRUD + unmatched-term statistics + LLM extension suggestions.
//! Viewing = viewer; modifying = editor (the ontology directly affects the extraction whitelist that follows).

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use utopia_core::AppError;
use utopia_store::ontology::{Lacking, LinkEnd};
use uuid::Uuid;

use crate::api::documents_routes::require_reader_kb;
use crate::auth::{AuthUser, SessionOrToken};
use crate::error::ApiResult;
use crate::state::AppState;

async fn require_kb(
    state: &AppState,
    user: &utopia_core::models::User,
    kb_id: Uuid,
    min: Role,
) -> Result<utopia_core::models::KnowledgeBase, AppError> {
    utopia_store::access::require_kb(&state.pool, user, kb_id, min).await
}

/// Tokens can read this too (0014 decision 7): a class key must first be turned into a type id before instances can be looked up
pub async fn get(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let (entity_types, relation_types) = tokio::try_join!(
        utopia_store::ontology::entity_type_views(&state.pool, kb_id),
        utopia_store::ontology::relation_type_views(&state.pool, kb_id),
    )?;
    Ok(Json(json!({
        "entity_types": entity_types,
        "relation_types": relation_types,
    })))
}

#[derive(Deserialize)]
pub struct InstancesQuery {
    #[serde(default)]
    pub page: i64,
    #[serde(default = "default_per")]
    pub per: i64,
    /// Lists only members missing this relation: the key of a property, or an open phrase (#1269)
    #[serde(default)]
    pub missing_predicate: Option<String>,
    /// out (default, the member is the subject) | in (the member is the object) | either
    #[serde(default)]
    pub missing_direction: Option<String>,
}

fn default_per() -> i64 {
    12
}

/// "Which relation is missing, and which end to look at": no predicate means no filter; an empty predicate or an unrecognized direction is 422
fn missing_link(q: &InstancesQuery) -> Result<Option<(&str, LinkEnd)>, AppError> {
    let end = match q.missing_direction.as_deref() {
        None | Some("out") => LinkEnd::Out,
        Some("in") => LinkEnd::In,
        Some("either") => LinkEnd::Either,
        Some(other) => {
            return Err(AppError::invalid(
                "bad_missing_direction",
                format!("missing_direction must be out, in or either, not `{other}`"),
            ))
        }
    };
    let Some(predicate) = q.missing_predicate.as_deref() else {
        return Ok(None);
    };
    if predicate.trim().is_empty() {
        return Err(AppError::invalid(
            "bad_missing_predicate",
            "missing_predicate must name a relation or a phrase",
        ));
    }
    Ok(Some((predicate.trim(), end)))
}

/// List of entity instances under a class (right side of the detail pane, paginated).
/// With `missing_predicate`, lists only members missing that relation, and echoes back the properties and phrases recognized as it.
/// Tokens can read this too (0014 decision 7).
pub async fn list_entity_instances(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path((kb_id, type_id)): Path<(Uuid, Uuid)>,
    Query(q): Query<InstancesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let missing = missing_link(&q)?;
    require_reader_kb(&state, &reader, kb_id).await?;
    let per = q.per.clamp(1, 100);
    let page = q.page.max(0);
    let predicate = match missing {
        Some((p, end)) => Some((
            utopia_store::ontology::link_predicate(&state.pool, kb_id, p).await?,
            end,
        )),
        None => None,
    };
    let lacking = predicate.as_ref().map(|(predicate, end)| Lacking {
        predicate,
        end: *end,
    });
    let (rows, total) = utopia_store::ontology::entity_instances(
        &state.pool,
        kb_id,
        type_id,
        lacking,
        per,
        page * per,
    )
    .await?;
    let mut body = json!({ "entities": rows, "total": total });
    if let Some((p, _)) = &predicate {
        body["missing"] = json!({
            "relation_type_id": p.relation_type_id,
            "phrases": p.phrases(),
        });
    }
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct EntityTypeReq {
    pub key: Option<String>,
    pub label: String,
    #[serde(default)]
    pub color: Option<String>,
    /// circle | square
    #[serde(default)]
    pub shape: Option<String>,
    /// All parent classes; **the first is the primary parent** (the left pane draws the tree under that branch).
    /// The UI states this, so there is no extra "pick primary parent" control
    #[serde(default)]
    pub parents: Vec<Uuid>,
    /// Classes mutually exclusive with it. **Default = leave alone** -- regardless of callers that are mutually exclusive (proposal adoption, cold start),
    /// must not clear existing declarations just because a class was created
    #[serde(default)]
    pub disjoint: Option<Vec<Uuid>>,
    /// Semantic guidance, injected into the extraction prompt
    #[serde(default)]
    pub description: Option<String>,
    /// How the type is shown (RC-008): a flat object of string values. Absent = untouched
    #[serde(default)]
    pub display_hints: Option<serde_json::Value>,
}

/// After the editor or an import changes the ontology: first backfill the vector index, then queue alignment.
///
/// **New elements need vectors, or the alignment shortlist cannot see them.** When candidates outnumber the shortlist, pick by vector
/// (`phrase_alignment::shortlist`); candidate classes for category words are also searched by vector
/// (`type_alignment::candidates_for`); an element without a vector never makes the list, so its shape fingerprint does not change,
/// and it is not re-judged after creation -- this is how `bordered_by` went the first time it really ran (the adoption path in 0061 cut 1.1 already backfills it;
/// this covers the editor's four entry points and OWL import). A changed description counts too: vectors judge staleness by the "source text embedded at the time", and a row with a changed description
/// is re-embedded by the same task.
///
/// **Queue a task, do not run inline.** Embedding inline costs a fixed overhead per embedding request; if the ontology page creates 28 properties at once, that is 28 round trips;
/// with a queued task, one batch of edits embeds only once. It blocks only what is queued (`enqueue_unless_queued_after`), not what is running:
/// a running one has already read the set awaiting embedding and cannot see this edit, so another must be queued.
/// The debounce is shorter than alignment; before alignment starts it also checks whether this task is queued or running, and if so, waits
/// (see `align_phrases_reasking` / `align_types_reasking`).
async fn reindex_then_align(
    state: &AppState,
    kb_id: Uuid,
    align_kind: &str,
) -> Result<(), AppError> {
    utopia_store::jobs::enqueue_unless_queued_after(
        &state.pool,
        "embed_ontology",
        json!({ "kb_id": kb_id }),
        std::time::Duration::from_secs(2),
    )
    .await?;
    utopia_store::jobs::enqueue_unless_pending(
        &state.pool,
        align_kind,
        json!({ "kb_id": kb_id }),
        // Debounce: one batch of edits (importing a package, creating a series of properties) queues only once
        std::time::Duration::from_secs(5),
    )
    .await?;
    Ok(())
}

pub async fn create_entity_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<EntityTypeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let key = req
        .key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::invalid("key_required", "key is required"))?;
    let id = utopia_store::ontology::create_entity_type(
        &state.pool,
        kb_id,
        key,
        req.label.trim(),
        // With no color given, pick one by key, rather than every class sharing one blue-grey
        req.color
            .as_deref()
            .unwrap_or_else(|| utopia_store::palette::color_for_key(key)),
        req.shape.as_deref().unwrap_or("circle"),
        &req.parents,
        req.description.as_deref().unwrap_or("").trim(),
    )
    .await?;
    // Mutually exclusive default = leave alone: its callers (proposal adoption, cold start) must not clear existing declarations
    // just because a class was created
    if let Some(d) = req.disjoint.as_deref() {
        utopia_store::ontology::set_disjoint_for(&state.pool, kb_id, id, d).await?;
    }
    if let Some(h) = &req.display_hints {
        utopia_store::ontology::set_type_display_hints(&state.pool, kb_id, id, h).await?;
    }
    // Audit records only, never blocks
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "entity_type.created",
        "entity_type",
        Some(id),
        json!({ "key": key, "label": req.label.trim() }),
    )
    .await;
    // The ontology gained a class: among the category words' bindings, the "no" and "undetermined" ones must be re-judged (0044 alignment slice one).
    // Backfill its vector first, or the candidate-class search will not find it
    let _ = reindex_then_align(&state, kb_id, "align_types").await;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_entity_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EntityTypeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    utopia_store::ontology::update_entity_type(
        &state.pool,
        kb_id,
        id,
        req.label.trim(),
        // **No color given = keep the original color**, not reset. This edits by id, so there is no key at all;
        // and previously writing "#8ea5bd" unconditionally meant that any rename without a color
        // would wipe out the color the user had picked
        req.color.as_deref(),
        req.shape.as_deref().unwrap_or("circle"),
        &req.parents,
        req.description.as_deref().unwrap_or("").trim(),
    )
    .await?;
    if let Some(d) = req.disjoint.as_deref() {
        utopia_store::ontology::set_disjoint_for(&state.pool, kb_id, id, d).await?;
    }
    if let Some(h) = &req.display_hints {
        utopia_store::ontology::set_type_display_hints(&state.pool, kb_id, id, h).await?;
    }
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "entity_type.updated",
        "entity_type",
        Some(id),
        json!({ "label": req.label.trim(), "color": req.color, "shape": req.shape,
                "description": req.description }),
    )
    .await;
    // The class definition changed: category words bound to it go stale and are re-judged. If the label or description changed, the vector is stale too, so re-embed first
    let _ = reindex_then_align(&state, kb_id, "align_types").await;
    // Reconciliation may have accepted or opened violation rows in the same transaction -- the review queue changed, so say so (0062)
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_entity_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    utopia_store::ontology::delete_entity_type(&state.pool, kb_id, id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "entity_type.deleted",
        "entity_type",
        Some(id),
        json!({}),
    )
    .await;
    // Reconciliation may have accepted or opened violation rows in the same transaction -- the review queue changed, so say so (0062)
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct RelationTypeReq {
    pub key: Option<String>,
    pub label: String,
    #[serde(default = "default_temporal")]
    pub temporal: String,
    #[serde(default)]
    pub functional: bool,
    #[serde(default)]
    pub inverse_functional: bool,
    /// The other four OWL axioms. **Must be editable in the UI**: the reasoner's (0002) criteria all
    /// come from these, and until now they could only be brought in by importing an OWL file; a user who builds
    /// the ontology by hand in the UI could never start that machine.
    ///
    /// Alongside `functional` / `inverse_functional`, because they are the same family;
    /// only those two were already persisted first
    #[serde(default)]
    pub is_transitive: bool,
    #[serde(default)]
    pub is_symmetric: bool,
    #[serde(default)]
    pub is_asymmetric: bool,
    #[serde(default)]
    pub is_irreflexive: bool,
    /// Two that point at another relation (`inverseOf` / `subPropertyOf`).
    ///
    /// **Default = clear, same rule as the six above.** They are one group of declarations
    /// submitted together in one form submission; half overwritten and half kept would make "I removed the inverse" and "I did not touch the inverse"
    /// look identical. The property form does not fill these two, and properties are not allowed to have them in the first place --
    /// the store layer blocks it, and the stored value is NULL anyway
    #[serde(default)]
    pub inverse_of: Option<Uuid>,
    #[serde(default)]
    pub sub_property_of: Option<Uuid>,
    #[serde(default)]
    pub description: Option<String>,
    /// relation | attribute (fixed at creation, ignored on update)
    #[serde(default)]
    pub kind: Option<String>,
    /// Classes that may be the subject. An attribute needs at least one; for a relation, empty = unrestricted.
    /// **Only when both are omitted is it left alone**: the signature is submitted as one group -- sending only ranges would clear
    /// domains, not "ranges change, domains stay"
    #[serde(default)]
    pub domains: Option<Vec<Uuid>>,
    /// Which properties an edge of this relation can carry (0037): the ids of property definitions. None = leave alone
    #[serde(default)]
    pub qualifiers: Option<Vec<Uuid>>,
    /// Classes that may be the object. Only meaningful for relations; same group-submission rule as domains
    #[serde(default)]
    pub ranges: Option<Vec<Uuid>>,
    /// For attributes only: text | number | date | bool
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    /// How the property is shown (RC-008): a flat object of string values. Absent = untouched
    #[serde(default)]
    pub display_hints: Option<serde_json::Value>,
}

impl RelationTypeReq {
    /// Axiom bundle. **Passing them separately sooner or later gets the order wrong** -- all six are bool, so the compiler cannot help;
    /// the last two are both `Option<Uuid>`, likewise.
    fn axioms(&self) -> utopia_core::models::RelationAxioms {
        utopia_core::models::RelationAxioms {
            functional: self.functional,
            inverse_functional: self.inverse_functional,
            transitive: self.is_transitive,
            symmetric: self.is_symmetric,
            asymmetric: self.is_asymmetric,
            irreflexive: self.is_irreflexive,
            inverse_of: self.inverse_of,
            sub_property_of: self.sub_property_of,
        }
    }
}

fn default_temporal() -> String {
    "state".into()
}

pub async fn create_relation_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<RelationTypeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let key = req
        .key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::invalid("key_required", "key is required"))?;
    let kind = req.kind.as_deref().unwrap_or("relation");
    let id = utopia_store::ontology::create_relation_type(
        &state.pool,
        kb_id,
        key,
        req.label.trim(),
        &req.temporal,
        req.axioms(),
        req.description.as_deref().unwrap_or("").trim(),
        kind,
        req.domains.as_deref().unwrap_or(&[]),
        req.ranges.as_deref().unwrap_or(&[]),
        req.datatype.as_deref(),
        req.unit.as_deref().map(str::trim).filter(|s| !s.is_empty()),
    )
    .await?;
    if let Some(q) = req.qualifiers.as_deref() {
        utopia_store::ontology::set_relation_qualifiers(&state.pool, kb_id, id, q).await?;
    }
    if let Some(h) = &req.display_hints {
        utopia_store::ontology::set_relation_display_hints(&state.pool, kb_id, id, h).await?;
    }
    // One more property: signatures judged none / undecided may now match (0044 alignment slice two).
    // Backfill its vector first; the shortlist can only see it then
    reindex_then_align(&state, kb_id, "align_phrases").await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "relation_type.created",
        "relation_type",
        Some(id),
        json!({ "key": key, "label": req.label.trim(), "temporal": req.temporal, "kind": kind }),
    )
    .await;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_relation_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, id)): Path<(Uuid, Uuid)>,
    Json(req): Json<RelationTypeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    utopia_store::ontology::update_relation_type(
        &state.pool,
        kb_id,
        id,
        req.label.trim(),
        &req.temporal,
        req.axioms(),
        req.description.as_deref().unwrap_or("").trim(),
        req.datatype.as_deref(),
        req.unit.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        // If the request does not include these two fields, leave them alone -- the attribute form does not manage domain
        req.domains.as_deref(),
        req.ranges.as_deref(),
    )
    .await?;
    if let Some(q) = req.qualifiers.as_deref() {
        utopia_store::ontology::set_relation_qualifiers(&state.pool, kb_id, id, q).await?;
    }
    if let Some(h) = &req.display_hints {
        utopia_store::ontology::set_relation_display_hints(&state.pool, kb_id, id, h).await?;
    }
    // The property's definition or domain/range changed: signatures bound to it go stale, and ones judged none may now match.
    // If the label or description changed, the vector is stale too; re-embed first (if only domain/range changed, the backfill task's check exits at once)
    reindex_then_align(&state, kb_id, "align_phrases").await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "relation_type.updated",
        "relation_type",
        Some(id),
        json!({ "label": req.label.trim(), "temporal": req.temporal,
                "functional": req.functional, "inverse_functional": req.inverse_functional,
                "description": req.description }),
    )
    .await;
    // Reconciliation may have accepted or opened violation rows in the same transaction -- the review queue changed, so say so (0062)
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_relation_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    utopia_store::ontology::delete_relation_type(&state.pool, kb_id, id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "relation_type.deleted",
        "relation_type",
        Some(id),
        json!({}),
    )
    .await;
    // Reconciliation may have accepted or opened violation rows in the same transaction -- the review queue changed, so say so (0062)
    state.emit_review(kb_id);
    Ok(Json(json!({ "ok": true })))
}

/// The holder of one end of a predicate with more than two open values (#341).
///
/// In a library that grew out of the ontology itself, nobody declared uniqueness, so a successor does not close out a predecessor; three people manage
/// one project at once -- and "who is managing" is the whole point of this product. The engine never infers functional automatically
/// (bootstrap_ontology.rs says why), so it can only be **asked**: here the evidence is laid out --
/// which predicate, which end, how many holders, how many rows reconciliation would close, how many would go to human review -- and a person decides.
/// Those with `declared` true are declared but not yet reconciled against the ledger (imported ones, or ones that predate the declaration).
pub async fn uniqueness_candidates(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let cands = utopia_store::temporal::uniqueness_candidates(&state.pool, kb_id).await?;
    let items: Vec<serde_json::Value> = cands
        .iter()
        .map(|c| {
            json!({
                "predicate_id": c.predicate_id,
                "key": c.key,
                "label": c.label,
                "kind": c.kind,
                "side": c.side,
                "axiom": if c.side == "subject" { "functional" } else { "inverse_functional" },
                "declared": c.declared,
                "holders": c.holders,
                "open_facts": c.open_facts,
                "would_close": c.would_close,
                "would_review": c.would_review,
                "examples": c.examples.iter().map(|e| json!({
                    "holder": e.holder,
                    "values": e.values.iter().map(|v| json!({
                        "fact_id": v.fact_id,
                        "name": v.name,
                        "valid_from": v.valid_from.map(|t| t.to_rfc3339()),
                        "confidence": v.confidence,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "candidates": items })))
}

/// After the declaration is in place, reconcile this predicate's open rows already on the ledger (#341).
///
/// Same path as at persist time: supersede and rewrite, the supersedes chain points back to the old row, and the record axis is rewound to before the declaration,
/// so the original row stays visible; uncertain ones go to human review. When the predicate has no uniqueness declaration, refuse --
/// declaring is a human's job, and here we only execute. The response reports how many were closed and how many went to human review.
pub async fn reconcile_relation_type(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let report = utopia_store::temporal::reconcile_predicate(&state.pool, kb_id, id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "relation_type.reconciled",
        "relation_type",
        Some(id),
        json!({ "corrected": report.corrected.len(), "conflicts": report.conflicts }),
    )
    .await;
    Ok(Json(json!({
        "corrected": report.corrected.len(),
        "conflicts": report.conflicts,
        "corrected_ids": report.corrected,
    })))
}

/// Proposals still awaiting a person's review, reassembled in the shape the endpoint originally had.
///
/// So the frontend need not distinguish "just computed" from "last time stored" -- both are the same type, with the same rendering.
pub async fn stored_proposals(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let stored = utopia_store::ontology::open_proposals(&state.pool, kb_id).await?;
    let mut out = json!({
        "entity_types": [], "relation_types": [], "attribute_types": [], "map_to": []
    });
    for p in stored {
        // Only proposals made by the agent (0061). The old Suggest path has been retired, and proposals it left in old databases have no
        // adoption entry point now; showing them would only display a row of buttons that do nothing
        if p.proposed_by != "agent" {
            continue;
        }
        if let Some(arr) = out.get_mut(&p.section).and_then(|v| v.as_array_mut()) {
            let mut item = p.payload;
            // Three extra columns: who proposed it, which issues it serves, and which shapes it would bind to
            if let Some(o) = item.as_object_mut() {
                o.insert("proposed_by".into(), json!("agent"));
                o.insert("serves".into(), json!(p.serves));
                o.insert("signatures".into(), p.signatures);
            }
            arr.push(item);
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct DecideProposalReq {
    pub section: String,
    pub key: String,
    /// Accepts only rejected; adoption goes through `/ontology/proposals/adopt`
    pub status: String,
    /// The reason for rejection (0061: the next round of the agent can read it)
    #[serde(default)]
    pub reason: Option<String>,
}

/// Reject a proposal. **Change the status, do not delete the row**: the rejection record is exactly why the next agent round will not bring it back for review.
///
/// This used to also accept adopted, which only changed the status: no elements, regression cases, or alignment were created. Adoption must submit these
/// together with the status, and only `/ontology/proposals/adopt` can do that (0061 decision 3)
pub async fn decide_proposal(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<DecideProposalReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    if req.status == "adopted" {
        return Err(AppError::invalid(
            "adoption_endpoint",
            "Adopt a proposal through /ontology/proposals/adopt; this endpoint only rejects",
        )
        .into());
    }
    if req.status != "rejected" {
        return Err(AppError::invalid("bad_status", "status 只能是 rejected").into());
    }
    utopia_store::ontology::reject_proposal(
        &state.pool,
        kb_id,
        &req.section,
        &req.key,
        req.reason.as_deref(),
        user.id,
    )
    .await?;
    Ok(Json(json!({ "ok": true })))
}

/// OWL import: first see what would happen; persist only after confirmation.
///
/// **Never let uploading a file irreversibly change the ontology** -- preview and persist go through the same plan;
/// two independent paths sooner or later diverge, and divergence means what happens after confirmation is not what was just reviewed.
pub async fn preview_import(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    multipart: axum::extract::Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let (filename, bytes) = read_upload(multipart).await?;
    let (plan, _, _) = crate::owl_import::plan(&state, kb_id, &filename, &bytes).await?;
    Ok(Json(json!({ "filename": filename, "plan": plan })))
}

/// A write token can also export (0014 decision 6): a new database's ontology is set up together with the database by the same client
pub async fn apply_import(
    State(state): State<AppState>,
    caller: SessionOrToken,
    Path(kb_id): Path<Uuid>,
    multipart: axum::extract::Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    caller.require_write(Some(kb_id))?;
    let token_id = caller.token_id();
    let user = caller.user;
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let (filename, bytes) = read_upload(multipart).await?;
    // The import changes the criteria (axiom chain, domain/range, parent edges): first record the violations the old ontology would detect
    // (0062), then after apply persists, reconcile against the new ontology -- stale rows detected under the old criteria end as criterion_changed,
    // not wiped out in bulk; if reconciliation fails, report the error honestly, do not pretend it is clean
    // The baseline runs in one transaction: on an autocommit connection each statement sees its own snapshot,
    // and if another commit arrives midway it would mix two moments into one baseline
    let mut baseline_tx = state.pool.begin().await?;
    let was_detected = utopia_store::reasoning::detection_keys(&mut baseline_tx, kb_id).await?;
    baseline_tx.commit().await?;
    let (import_id, plan) = match crate::owl_import::apply(
        &state, kb_id, user.id, token_id, &filename, &bytes,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            // apply makes several commits: if it fails midway, some criteria are already persisted. Best-effort reconciliation --
            // a committed criterion is the "current ontology", and open rows must not carry stale criteria overnight. Reconciliation
            // failing on its own only logs a warning; the original error is still returned, not swallowed
            match state.pool.begin().await {
                Ok(mut tx) => {
                    if let Err(re) =
                        utopia_store::reasoning::reconcile_ontology(&mut tx, kb_id, &was_detected)
                            .await
                    {
                        tracing::warn!(error = ?re, "the violation reconciliation after the failed import also did not run");
                    } else if let Err(ce) = tx.commit().await {
                        tracing::warn!(error = ?ce, "failed to commit violation reconciliation after the failed import");
                    }
                }
                Err(be) => {
                    tracing::warn!(error = ?be, "could not even open the reconciliation transaction after the failed import")
                }
            }
            return Err(e.into());
        }
    };
    // apply makes several commits on its own; this transaction only reconciles: `open` means reconciled against the current ontology
    let mut tx = state.pool.begin().await?;
    let violations =
        utopia_store::reasoning::reconcile_ontology(&mut tx, kb_id, &was_detected).await?;
    tx.commit().await?;
    // The ontology gained classes and properties: category words judged "no" without a class, and signatures that do not match a property, must be re-judged (0044 alignment)
    // Same step as the editor: backfill vectors first, then queue category-word alignment, which queues phrase alignment when it finishes. The backfill apply already
    // queued it, so a queued one is not queued again; the import has already persisted, and failing to queue it is not reported as failure
    let _ = reindex_then_align(&state, kb_id, "align_types").await;
    state.emit_review(kb_id);
    Ok(Json(
        json!({ "import_id": import_id, "plan": plan, "violations": violations }),
    ))
}

pub async fn list_imports(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let imports = utopia_store::ontology::list_imports(&state.pool, kb_id).await?;
    Ok(Json(json!({ "imports": imports })))
}

/// Takes the first file from the multipart body. Limit: 8 MB -- FOAF is 44 KB, DCTerms 48 KB,
/// and large modular ones like FIBO are in the hundreds of KB; anything bigger is usually the wrong file.
const MAX_ONTOLOGY_BYTES: usize = 8 * 1024 * 1024;

async fn read_upload(
    mut multipart: axum::extract::Multipart,
) -> Result<(String, Vec<u8>), AppError> {
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        AppError::invalid_detail("bad_upload", "Could not read the upload", e.to_string())
    })? {
        let Some(filename) = field.file_name().map(String::from) else {
            continue;
        };
        let bytes = field.bytes().await.map_err(|e| {
            AppError::invalid_detail(
                "upload_read_failed",
                "Could not read the uploaded file",
                e.to_string(),
            )
        })?;
        if bytes.len() > MAX_ONTOLOGY_BYTES {
            return Err(AppError::invalid(
                "file_too_large",
                "Ontology file is too large (max 8 MB)",
            ));
        }
        if bytes.is_empty() {
            return Err(AppError::invalid("empty_file", "Ontology file is empty"));
        }
        return Ok((filename, bytes.to_vec()));
    }
    Err(AppError::invalid("no_files", "No file in the upload"))
}

/// The **compute-only, no-write** step of type resolution: the profile and candidate classes of each entity awaiting refinement.
///
/// Same pattern as ontology import: look at the plan first, then decide whether to persist. Here it has one more use --
/// when retrieval finds nothing, the receipt carries "what we searched with", so at a glance you know whether to change the profile
/// or change the class description.
pub async fn type_resolution_preview(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let items = crate::type_resolution::preview(&state, kb_id).await?;
    Ok(Json(json!({ "items": items })))
}

/// Run type resolution once and persist: retrieve candidates -> adjudicate -> three-tier disposition.
///
/// Separating it from preview is this repo's existing shape (ontology import also looks at the plan before persisting). Here there is one more reason:
/// changing a class **does not enter the timeline**, so unlike fact rewrites it does not surface on its own in entity history;
/// looking before acting is the only moment it can be seen.
pub async fn type_resolution_apply(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let outcome = crate::type_resolution::resolve(&state, kb_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "ontology.types_resolved",
        "knowledge_base",
        Some(kb_id),
        json!({ "retyped": outcome.retyped, "for_review": outcome.for_review.len(),
                "left_alone": outcome.left_alone.len(), "batch": outcome.batch }),
    )
    .await;
    Ok(Json(json!(outcome)))
}

/// Undo one type resolution: put that batch of entities back into their original classes.
pub async fn type_resolution_undo(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, batch_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let reverted = utopia_store::resolution::unadopt_types(&state.pool, kb_id, batch_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "ontology.types_resolution_reverted",
        "knowledge_base",
        Some(kb_id),
        json!({ "batch": batch_id, "reverted": reverted }),
    )
    .await;
    Ok(Json(json!({ "reverted": reverted })))
}

#[derive(Deserialize)]
pub struct ApproveRefinementReq {
    pub from_type_id: Uuid,
    pub to_type_id: Uuid,
    /// Entities to change along with this one. **The approved thing is the class pair, the change is to entities** --
    /// the two are separate, so the caller can approve the rule alone without touching any entity yet
    #[serde(default)]
    pub entity_ids: Vec<Uuid>,
}

/// Approve a "coarse class -> fine class" pairing, and move the entities that came with the request into it.
///
/// The human-review tier is triggered by "crosses a classification axis or not", and the real measurement shows that criterion mostly tests **whether the seed class connected to the imported vocabulary**,
/// not risk -- schema.org's Place uses a different key, so every city would be asked about.
/// Once the pairing is approved it is not asked again: that is a judgment between classes; the entities just happened to collide with it.
pub async fn approve_refinement(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<ApproveRefinementReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    utopia_store::resolution::approve_refinement(
        &state.pool,
        kb_id,
        req.from_type_id,
        req.to_type_id,
        user.id,
    )
    .await?;
    let picks: Vec<(Uuid, Uuid)> = req
        .entity_ids
        .iter()
        .map(|id| (*id, req.to_type_id))
        .collect();
    let (batch, moved) = if picks.is_empty() {
        (None, 0)
    } else {
        let (b, n) =
            utopia_store::resolution::retype_entities(&state.pool, kb_id, &picks, Some(user.id))
                .await?;
        (Some(b), n)
    };
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "ontology.refinement_approved",
        "entity_type",
        Some(req.to_type_id),
        json!({ "from": req.from_type_id, "to": req.to_type_id, "moved": moved }),
    )
    .await;
    Ok(Json(json!({ "moved": moved, "batch": batch })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(predicate: Option<&str>, direction: Option<&str>) -> InstancesQuery {
        InstancesQuery {
            page: 0,
            per: 12,
            missing_predicate: predicate.map(str::to_owned),
            missing_direction: direction.map(str::to_owned),
        }
    }

    fn code(r: Result<Option<(&str, LinkEnd)>, AppError>) -> &'static str {
        match r {
            Err(AppError::Invalid { code, .. }) => code,
            _ => "",
        }
    }

    #[test]
    fn a_missing_link_reads_its_predicate_and_end() {
        assert_eq!(missing_link(&query(None, None)).ok(), Some(None));
        assert_eq!(
            missing_link(&query(Some(" works_for "), None)).ok(),
            Some(Some(("works_for", LinkEnd::Out)))
        );
        assert_eq!(
            missing_link(&query(Some("acquired"), Some("either"))).ok(),
            Some(Some(("acquired", LinkEnd::Either)))
        );
    }

    #[test]
    fn a_blank_predicate_or_an_unknown_end_is_422() {
        assert_eq!(
            code(missing_link(&query(Some("  "), None))),
            "bad_missing_predicate"
        );
        assert_eq!(
            code(missing_link(&query(Some("works_for"), Some("up")))),
            "bad_missing_direction"
        );
    }
}
