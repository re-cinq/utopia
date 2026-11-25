use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::{KnowledgeBase, Role, User};
use utopia_core::AppError;
use uuid::Uuid;

use crate::auth::{AuthUser, SessionOrToken};
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreateKbReq {
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
    /// IDs of preset ontology packs, installed in the given order. Empty = only the ten seeds.
    ///
    /// **Order matters:** the first pack's classes claim same-named seed classes (they have no IRI),
    /// and later packs that collide look up the alignment table. schema.org goes first so the others line up.
    #[serde(default)]
    pub ontology_packs: Vec<String>,
    /// Scenario packs (0073): classes, properties and capability questions preset for business scenarios, built through the editor's path.
    /// Installed after the standard packs; colliding keys are skipped
    #[serde(default)]
    pub scenario_packs: Vec<String>,
}

#[derive(Deserialize)]
pub struct UpdateKbReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    /// Automatic ontology expansion switch (on by default; turning it off does not stop "watching", it only turns it into a proposal you click through)
    #[serde(default)]
    pub auto_extend_ontology: Option<bool>,
    /// Ontology language (`en` | `zh`): follows the corpus, not the UI. See docs/decisions/0004
    #[serde(default)]
    pub ontology_lang: Option<String>,
    /// Materialized inference switch (off by default). See docs/decisions/0002 R1
    #[serde(default)]
    pub materialize_inferences: Option<bool>,
    /// How often to re-derive, in minutes. Facts keep changing, and relying only on manual clicks leaves derivations permanently stale
    #[serde(default)]
    pub inference_interval_minutes: Option<i32>,
    /// Queue a round of type resolution automatically when extraction finishes (on by default). See docs/decisions/0016 C2
    #[serde(default)]
    pub auto_type_resolution: Option<bool>,
    /// Governance switch (off by default): when on, the agent works through waiting duplicate pairs first-in-first-out and reads the ledger before ruling;
    /// turning it on queues a round right away, and turning it off makes the task stop at the next boundary between two clusters. See docs/decisions/0025
    #[serde(default)]
    pub governance: Option<bool>,
    /// Human-written data conventions (rules like "test tickets don't count" that are not in the schema); the data-question and exploration
    /// prompts both read it. Exploration-generated descriptions live in another field and cannot be PATCHed. See #570
    #[serde(default)]
    pub data_conventions: Option<String>,
}

/// KB list visible to the user (restricted libraries are visible only to matrix members and system admins).
///
/// Read tokens can list too (#1243): a client looks up by name before creating a library, so it does not create a same-named library on every run.
/// A token scoped by `kb_ids` sees only the libraries it lists
pub async fn list(
    State(state): State<AppState>,
    caller: SessionOrToken,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<Vec<KnowledgeBase>>> {
    let user = &caller.user;
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Viewer)
        .await?;
    let mut list =
        utopia_store::kbs::list_visible(&state.pool, workspace_id, user.id, user.is_admin).await?;
    list.retain(|kb| caller.reaches(kb.id));
    Ok(Json(list))
}

/// Create a library: deployment admin (workspace Admin+ or system admin). The creator is automatically added to the matrix as admin.
///
/// Write tokens can create too (0014 decision 6): as the person who issued the token, role requirements are not loosened at all
pub async fn create(
    State(state): State<AppState>,
    caller: SessionOrToken,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<CreateKbReq>,
) -> ApiResult<Json<KnowledgeBase>> {
    caller.require_write(None)?;
    let token_id = caller.token_id();
    let user = caller.user;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(AppError::invalid("bad_name", "Name must be 1-64 characters").into());
    }
    let kind = req.kind.as_deref().unwrap_or("knowledge");
    if !matches!(kind, "knowledge" | "memory") {
        return Err(AppError::Validation("kind must be 'knowledge' or 'memory'".into()).into());
    }
    // Creating a library is a deployment admin action (entry point is System settings): system admin or workspace Admin+.
    // The frontend defaults user-created libraries to restricted, so they do not pollute the everyone-switcher; General is created open by the system
    let ws_role =
        utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Viewer)
            .await?;
    if !user.is_admin && ws_role < Role::Admin {
        return Err(AppError::Forbidden.into());
    }
    let kb = utopia_store::kbs::create(
        &state.pool,
        workspace_id,
        name,
        kind,
        req.description.as_deref(),
    )
    .await?;
    if let Some(v) = req.visibility.as_deref() {
        utopia_store::kbs::update(
            &state.pool,
            kb.id,
            None,
            None,
            Some(v),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
    }
    utopia_store::access::set_kb_member(&state.pool, kb.id, user.id, "admin", Some(user.id))
        .await?;
    install_packs(&state, kb.id, user.id, token_id, &req.ontology_packs).await?;
    let kb = utopia_store::kbs::get(&state.pool, kb.id).await?;
    // Scenario packs come after the standard packs: a class key that collides with a standard pack yields to it, while declarations can still point at them (0073)
    let scenarios = crate::scenario_packs::install(
        &state,
        kb.id,
        user.id,
        &kb.ontology_lang,
        &req.scenario_packs,
    )
    .await?;
    if scenarios.classes + scenarios.properties + scenarios.questions > 0 {
        tracing::info!(kb_id = %kb.id, classes = scenarios.classes, properties = scenarios.properties,
                       questions = scenarios.questions, "场景包装好");
    }
    Ok(Json(kb))
}

/// My knowledge-base overview (account level): visible libraries + my role + join info + overview stats.
pub async fn my_kbs(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Viewer)
        .await?;
    let kbs =
        utopia_store::kbs::list_visible(&state.pool, workspace_id, user.id, user.is_admin).await?;
    let ids: Vec<Uuid> = kbs.iter().map(|k| k.id).collect();
    let infos = utopia_store::access::my_kb_infos(&state.pool, &ids, user.id).await?;
    let mut rows = Vec::with_capacity(kbs.len());
    for kb in &kbs {
        let info = infos.iter().find(|i| i.kb_id == kb.id);
        let role = utopia_store::access::kb_role(&state.pool, &user, kb).await?;
        rows.push(json!({
            "kb": kb,
            "my_role": role.map(|r| r.as_str()),
            "joined_at": info.and_then(|i| i.joined_at),
            "added_by_name": info.and_then(|i| i.added_by_name.clone()),
            "doc_count": info.map(|i| i.doc_count).unwrap_or(0),
            "member_count": info.map(|i| i.member_count).unwrap_or(0),
        }));
    }
    Ok(Json(json!({ "kbs": rows })))
}

async fn kb_with_role(
    state: &AppState,
    user: &User,
    kb_id: Uuid,
    min: Role,
) -> Result<KnowledgeBase, AppError> {
    utopia_store::access::require_kb(&state.pool, user, kb_id, min).await
}

/// The detail includes the caller's role in this library: the frontend uses it to gate entry points for destructive operations (rebuild/delete),
/// so users do not have to click all the way in to hit a 403.
pub async fn get_one(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let kb = kb_with_role(&state, &user, id, Role::Viewer).await?;
    let role = utopia_store::access::kb_role(&state.pool, &user, &kb).await?;
    let mut body = serde_json::to_value(&kb).map_err(|e| AppError::Other(e.into()))?;
    body["my_role"] = json!(role.map(|r| r.as_str()));
    Ok(Json(body))
}

/// Library settings (name/description/visibility): library admin to start.
pub async fn update(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateKbReq>,
) -> ApiResult<Json<KnowledgeBase>> {
    kb_with_role(&state, &user, id, Role::Admin).await?;
    let kb = utopia_store::kbs::update(
        &state.pool,
        id,
        req.name.as_deref().map(str::trim),
        req.description.as_deref(),
        req.visibility.as_deref(),
        req.auto_extend_ontology,
        req.ontology_lang.as_deref(),
        req.materialize_inferences,
        req.inference_interval_minutes,
        req.auto_type_resolution,
        req.governance,
        req.data_conventions.as_deref().map(str::trim),
    )
    .await?;
    // Turning the switch on starts processing right away: queue a round; one already queued for this library is not duplicated
    if req.governance == Some(true) {
        utopia_store::jobs::enqueue_unless_queued(&state.pool, "govern", json!({ "kb_id": id }))
            .await?;
    }
    // Audit records only, never blocks
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(id),
        user.id,
        "kb.updated",
        "kb",
        Some(id),
        // A convention change must leave a trace: the text goes into every data-question prompt, so who changed it and when must be traceable
        json!({
            "name": req.name,
            "visibility": req.visibility,
            "governance": req.governance,
            "data_conventions": req.data_conventions.is_some(),
        }),
    )
    .await;
    Ok(Json(kb))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let kb = kb_with_role(&state, &user, id, Role::Admin).await?;
    utopia_store::kbs::delete(&state.pool, id).await?;
    // kb_id set to NULL: the library was already cascade-deleted, so the event stays at the deployment level (actor and library name are in detail)
    let _ = utopia_store::audit::record(
        &state.pool,
        None,
        user.id,
        "kb.deleted",
        "kb",
        Some(id),
        json!({ "name": kb.name }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// How far this library has got (#313): the empty states on all four pages share one check.
///
/// Viewer to start: anyone who can see this library should know whether it is empty or running. The returns are all booleans and
/// counts; the model item only says whether it is configured, so the workspace admin gate is not needed.
pub async fn readiness(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<utopia_core::models::Readiness>> {
    kb_with_role(&state, &user, id, Role::Viewer).await?;
    Ok(Json(utopia_store::kbs::readiness(&state.pool, id).await?))
}

// ---------------------------------------------------------------------------
// KB member matrix (the library's own Settings → Members)
// ---------------------------------------------------------------------------

pub async fn members(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    kb_with_role(&state, &user, id, Role::Admin).await?;
    let members = utopia_store::access::kb_members(&state.pool, id).await?;
    Ok(Json(json!({ "members": members })))
}

#[derive(Deserialize)]
pub struct SetMemberReq {
    /// viewer | editor | admin
    pub role: String,
}

pub async fn set_member(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetMemberReq>,
) -> ApiResult<Json<serde_json::Value>> {
    kb_with_role(&state, &user, id, Role::Admin).await?;
    utopia_store::access::set_kb_member(&state.pool, id, user_id, &req.role, Some(user.id)).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(id),
        user.id,
        "kb.member_set",
        "user",
        Some(user_id),
        json!({ "role": req.role }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn remove_member(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    kb_with_role(&state, &user, id, Role::Admin).await?;
    utopia_store::access::remove_kb_member(&state.pool, id, user_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(id),
        user.id,
        "kb.member_removed",
        "user",
        Some(user_id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// Library-level audit log (Admin to start; pure audit display).
#[derive(Deserialize)]
pub struct AuditQuery {
    /// Action prefix. `entity.` pulls in the entity.retyped / entity.renamed family
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub actor: Option<Uuid>,
    /// The start is included and the end is not, matching the half-open interval convention
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// Date parameters: accepts either a day (`2026-08-30`) or RFC3339.
fn parse_day(raw: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>, AppError> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Some(d.and_hms_opt(0, 0, 0).unwrap().and_utc()));
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| Some(t.with_timezone(&chrono::Utc)))
        .map_err(|_| AppError::invalid("bad_date", "expected YYYY-MM-DD or RFC3339"))
}

/// Audit ledger. **Paging and filtering**: it used to be a fixed latest 100 entries, and the ledger is compliance material,
/// so "only the latest hundred visible" meant history could not be queried.
pub async fn audit_log(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    kb_with_role(&state, &user, id, Role::Admin).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    let action = q.action.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (events, total) = utopia_store::audit::list_for_kb(
        &state.pool,
        id,
        action,
        q.actor,
        parse_day(q.since.as_deref())?,
        parse_day(q.until.as_deref())?,
        limit,
        offset,
    )
    .await?;
    // The dropdown is filled with the actions this library has actually seen, not a hard-coded list:
    // the latter would list many options this library never had
    let actions = utopia_store::audit::actions_for_kb(&state.pool, id).await?;
    Ok(Json(
        json!({ "events": events, "total": total, "actions": actions }),
    ))
}
/// Install the selected ontology packs when creating a library.
///
/// This used to need `ensure_default_ontology` run first: a pack's classes must claim same-named seed classes (`schema:Organization` takes over `organization`),
/// and claiming requires that row to already exist.
/// **Now there are no seeds to claim**: after 0009 removed the built-in entity classes, 0010 and `#125` removed the seed relations, and
/// 0011 moved `mapped_to` to the semantic layer, the seeding function itself retired too. Packs land directly in an empty library.
///
/// **A failing pack does not roll back the ones already installed**: the ontology is additive, and a half-installed library still works,
/// while a rollback would have to remove the classes already created, which is exactly why 0008 decided against import undo.
/// The error message names the failing pack, so people know where to resume.
///
/// No default pack (#580): a General library created at registration and a library created in the dialog both start empty,
/// and which pack to install is chosen by the person creating the library.
/// Only called by `create_kb` (a precondition of 0062): a new library has no facts, so there are no violations to reconcile,
/// which is why it skips the baseline + criteria reconciliation that `apply_import` does. If this is ever reused on
/// an existing library, this comment stops holding: add the reconciliation first, then reuse it
pub(super) async fn install_packs(
    state: &AppState,
    kb_id: Uuid,
    actor: Uuid,
    via_token: Option<Uuid>,
    pack_ids: &[String],
) -> ApiResult<()> {
    if pack_ids.is_empty() {
        return Ok(());
    }
    let mut packs = Vec::with_capacity(pack_ids.len());
    for id in pack_ids {
        let pack = crate::ontology_packs::get(id)
            .ok_or_else(|| AppError::invalid("unknown_pack", format!("未知的本体包：{id}")))?;
        packs.push((pack, crate::ontology_packs::bytes(pack)?));
    }
    for (pack, bytes) in &packs {
        crate::owl_import::apply(state, kb_id, actor, via_token, pack.filename, bytes)
            .await
            .map_err(|e| {
                AppError::Other(anyhow::anyhow!(
                    "failed to install ontology package {}: {e}",
                    pack.id
                ))
            })?;
    }
    // Second pass: cross-pack domain / range. Packs are installed one at a time, so an earlier pack cannot see classes from later ones:
    // W3C Org's headOf needs FOAF's Agent (#222). With only one pack installed there is no "other pack"
    if packs.len() > 1 {
        for (pack, bytes) in &packs {
            let (d, r) =
                crate::owl_import::relink_domains_ranges(state, kb_id, pack.filename, bytes)
                    .await
                    .map_err(|e| {
                        AppError::Other(anyhow::anyhow!(
                            "failed to add signature for ontology package {}: {e}",
                            pack.id
                        ))
                    })?;
            tracing::debug!(%kb_id, pack = pack.id, domains = d, ranges = r, "backfilling cross-package signature chain");
        }
    }
    Ok(())
}

/// Scenario pack list (0073) for the creation UI: names in both languages are given, and the UI picks the one in its language;
/// when installed into a library, it is picked by the library's `ontology_lang`
pub async fn list_scenario_packs(AuthUser(_): AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let packs: Vec<_> = crate::scenario_packs::PACKS
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": { "zh": p.name.zh, "en": p.name.en },
                "summary": { "zh": p.summary.zh, "en": p.summary.en },
                "classes": p.classes.len(),
                "properties": p.attributes.len() + p.relations.len(),
                "questions": p.questions.len(),
            })
        })
        .collect();
    Ok(Json(json!({ "packs": packs })))
}

/// Optional ontology pack list for the creation UI. Needs nothing beyond being logged in: it is static data.
pub async fn list_packs(AuthUser(_): AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let packs: Vec<_> = crate::ontology_packs::PACKS
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "summary": p.summary,
                "classes": p.classes,
                "properties": p.properties,
            })
        })
        .collect();
    Ok(Json(json!({ "packs": packs })))
}
