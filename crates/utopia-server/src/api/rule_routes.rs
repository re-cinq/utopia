//! Create, read, update and delete for business rules (0021 / #277).
//!
//! **Writing a rule needs Editor**: a rule is part of the ontology -- changing one rule changes the conclusions across the whole database,
//! on the same order of magnitude as changing one axiom.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use utopia_store::business_rules::{ConclusionInput, ConditionInput};
use uuid::Uuid;

use super::graph_routes::require_kb;
use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct RuleReq {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub subject_type_id: Uuid,
    /// typing | attribute
    pub conclusion: String,
    #[serde(default)]
    pub conclude_type_id: Option<Uuid>,
    #[serde(default)]
    pub conclude_predicate_id: Option<Uuid>,
    #[serde(default)]
    pub conclude_value: Option<serde_json::Value>,
    /// The tree of computed conclusions (0032): given when `conclusion = "computed"`
    #[serde(default)]
    pub conclude_expr: Option<serde_json::Value>,
    #[serde(default)]
    pub join_predicate_id: Option<Uuid>,
    pub conditions: Vec<ConditionInput>,
}

#[derive(Deserialize)]
pub struct RulePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// If given, the whole group is replaced; if not, the conditions are left alone
    #[serde(default)]
    pub conditions: Option<Vec<ConditionInput>>,
    /// The conclusion is also replaced as a whole group: the three slots define each other, and changing only one would leave a half-state
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub conclude_type_id: Option<Uuid>,
    #[serde(default)]
    pub conclude_predicate_id: Option<Uuid>,
    #[serde(default)]
    pub conclude_value: Option<serde_json::Value>,
    #[serde(default)]
    pub conclude_expr: Option<serde_json::Value>,
    #[serde(default)]
    pub join_predicate_id: Option<Uuid>,
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let rules = utopia_store::business_rules::list(&state.pool, kb_id).await?;
    Ok(Json(json!({ "rules": rules })))
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(mut req): Json<RuleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    super::rule_expression_input::compile_inputs(
        &state.pool,
        kb_id,
        req.conclude_expr.as_mut(),
        &mut req.conditions,
    )
    .await?;
    let id = utopia_store::business_rules::create(
        &state.pool,
        kb_id,
        &req.name,
        &req.description,
        req.subject_type_id,
        &req.conclusion,
        req.conclude_type_id,
        req.conclude_predicate_id,
        req.conclude_value.clone(),
        req.conclude_expr.clone(),
        req.join_predicate_id,
        &req.conditions,
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "rule.created",
        "attribute_rule",
        Some(id),
        json!({ "name": req.name, "conclusion": req.conclusion }),
    )
    .await;
    Ok(Json(json!({ "id": id })))
}

pub async fn update(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, rule_id)): Path<(Uuid, Uuid)>,
    Json(mut req): Json<RulePatch>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    super::rule_expression_input::compile_inputs(
        &state.pool,
        kb_id,
        if req.conclusion.is_some() {
            req.conclude_expr.as_mut()
        } else {
            None
        },
        req.conditions.as_deref_mut().unwrap_or_default(),
    )
    .await?;
    let conclusion = req.conclusion.as_ref().map(|kind| ConclusionInput {
        kind: kind.clone(),
        type_id: req.conclude_type_id,
        predicate_id: req.conclude_predicate_id,
        value: req.conclude_value.clone(),
        expr: req.conclude_expr.clone(),
        join_predicate_id: req.join_predicate_id,
    });
    utopia_store::business_rules::update(
        &state.pool,
        kb_id,
        rule_id,
        req.name.as_deref(),
        req.description.as_deref(),
        req.enabled,
        req.conditions.as_deref(),
        conclusion.as_ref(),
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "rule.updated",
        "attribute_rule",
        Some(rule_id),
        json!({ "enabled": req.enabled, "conditions_replaced": req.conditions.is_some() }),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, rule_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    utopia_store::business_rules::delete(&state.pool, kb_id, rule_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "rule.deleted",
        "attribute_rule",
        Some(rule_id),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "ok": true })))
}

/// Who a rule is tagged with right now. Clicking the number in the UI opens this list.
pub async fn matches(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, rule_id)): Path<(Uuid, Uuid)>,
    axum::extract::Query(q): axum::extract::Query<MatchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let per = q.per.unwrap_or(20).clamp(1, 100);
    let page = q.page.unwrap_or(0).max(0);
    let (rows, total) =
        utopia_store::business_rules::matches(&state.pool, kb_id, rule_id, per, page * per).await?;
    Ok(Json(json!({ "matches": rows, "total": total })))
}

/// A rule's definition history (0060): what each version said, from when to when, and how many facts it currently supports
pub async fn versions(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, rule_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Viewer).await?;
    let versions = utopia_store::business_rules::versions(&state.pool, kb_id, rule_id).await?;
    Ok(Json(json!({ "versions": versions })))
}

#[derive(Deserialize)]
pub struct MatchQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per: Option<i64>,
}

/// Run it now, computing this rule's conclusions.
///
/// **Do not wait for the next materialization cycle**: the person writing the rule wants to see right away what it infers, and the cycle defaults to an hour.
/// It goes through the same `materialize` -- preview and the real run are the same path, with no separate logic that would drift.
pub async fn run_now(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    require_kb(&state, &user, kb_id, Role::Editor).await?;
    let report = utopia_store::reasoning::materialize(&state.pool, kb_id).await?;
    state.emit_graph(kb_id);
    Ok(Json(json!({
        "rules": report.attribute_rules,
        "hits": report.rule_hits,
        "capped": report.rule_capped,
        "inserted": report.inserted,
        "invalidated": report.invalidated,
        // How many rounds the chain ran, and whether it stopped at the cap (0030). **Hitting the cap must be stated** --
        // an unconnected link and "not satisfied" look exactly the same in the result
        "rounds": report.rule_rounds,
        "rounds_capped": report.rule_rounds_capped,
    })))
}
