//! Interface for capability questions (0061 decision 1) and the ontology agent (decisions 2 and 3).
//! View = viewer; change = editor: questions are the acceptance criteria for the ontology, and accepting a proposal changes the ontology directly.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::Role;
use utopia_core::AppError;
use utopia_store::competency_questions as questions;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::ontology_agent;
use crate::state::AppState;

pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<Vec<questions::Question>>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    Ok(Json(questions::list(&state.pool, kb_id).await?))
}

#[derive(Deserialize)]
pub struct CreateQuestionReq {
    pub question: String,
    #[serde(default)]
    pub expected_answer: Option<String>,
    #[serde(default)]
    pub needs: Option<serde_json::Value>,
}

/// A question a person raises is accepted the moment it lands: it is the person's requirement for this KB
pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<CreateQuestionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let text = req.question.trim();
    if text.is_empty() {
        return Err(AppError::invalid("question_required", "question is required").into());
    }
    let id = questions::create(
        &state.pool,
        kb_id,
        questions::NewQuestion {
            question: text,
            expected_answer: req
                .expected_answer
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            needs: req.needs.as_ref(),
            origin: "person",
            status: "accepted",
            created_by: Some(user.id),
        },
    )
    .await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(kb_id),
        user.id,
        "competency_question.created",
        "competency_question",
        Some(id),
        json!({ "question": text }),
    )
    .await;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
pub struct UpdateQuestionReq {
    #[serde(default)]
    pub question: Option<String>,
    /// Empty string = clear it
    #[serde(default)]
    pub expected_answer: Option<String>,
    #[serde(default)]
    pub needs: Option<serde_json::Value>,
    /// accepted | rejected | retired
    #[serde(default)]
    pub status: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, qid)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateQuestionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    if let Some(q) = req.question.as_deref() {
        if q.trim().is_empty() {
            return Err(AppError::invalid("question_required", "question is required").into());
        }
    }
    if req.question.is_some() || req.expected_answer.is_some() || req.needs.is_some() {
        questions::update(
            &state.pool,
            kb_id,
            qid,
            req.question.as_deref().map(str::trim),
            req.expected_answer.as_deref().map(str::trim),
            req.needs.as_ref(),
        )
        .await?;
    }
    if let Some(status) = req.status.as_deref() {
        questions::set_status(&state.pool, kb_id, qid, status).await?;
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, qid)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    questions::delete(&state.pool, kb_id, qid).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Have the agent raise questions for this KB: queue one job. The result is questions with status = proposed, which count once a person accepts them
pub async fn propose_questions(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let queued = utopia_store::jobs::enqueue_unless_queued(
        &state.pool,
        "propose_questions",
        json!({ "kb_id": kb_id }),
    )
    .await?;
    Ok(Json(json!({ "queued": queued.is_some() })))
}

#[derive(Deserialize)]
pub struct QuestionResultReq {
    pub answered: bool,
    /// The answer received (stored truncated)
    #[serde(default)]
    pub answer: Option<String>,
    /// expected | shape: judged against the expected answer, or only checked for the shape it needs
    #[serde(default)]
    pub judged_by: Option<String>,
    /// graph | text | both | none: where the answer came from (the graph tool returned facts, or only the body text was read)
    #[serde(default)]
    pub via: Option<String>,
    #[serde(default)]
    pub detail: Option<serde_json::Value>,
}

/// One question was asked (decision 5). It asks the competency bench, which asks the way a person asks in chat;
/// the server only records the result and does not ask itself, since chat does not yet have an in-process entry point
pub async fn record_result(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, qid)): Path<(Uuid, Uuid)>,
    Json(req): Json<QuestionResultReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let answer: Option<String> = req.answer.map(|a| a.chars().take(4000).collect());
    let via = req
        .via
        .as_deref()
        .filter(|v| matches!(*v, "graph" | "text" | "both" | "none"))
        .unwrap_or("none");
    let result = json!({
        "answered": req.answered,
        "answer": answer,
        "judged_by": req.judged_by.unwrap_or_else(|| "expected".into()),
        "via": via,
        "detail": req.detail,
    });
    if !questions::record_result(&state.pool, kb_id, qid, &result).await? {
        return Err(AppError::NotFound.into());
    }
    Ok(Json(json!({ "ok": true })))
}

/// Two numbers (decision 5): how many questions were answered correctly; how much of the agent's proposals a person changed or rejected. Also records how many
/// category words and phrase shapes the document mentions that the ontology still cannot hold, which the workbench uses to show what the ontology is still missing
pub async fn report(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    let q = questions::report(&state.pool, kb_id).await?;
    let p = utopia_store::ontology::agent_proposal_report(&state.pool, kb_id).await?;
    let uncovered = utopia_store::ontology::uncovered(&state.pool, kb_id).await?;
    let decided = p.adopted + p.rejected;
    let changed = p.adopted_edited + p.rejected;
    Ok(Json(json!({
        "questions": q,
        "uncovered": uncovered,
        "proposals": {
            "open": p.open,
            "adopted": p.adopted,
            "adopted_edited": p.adopted_edited,
            "rejected": p.rejected,
            "decided": decided,
            "changed": changed,
            "changed_share": if decided > 0 { Some(changed as f64 / decided as f64) } else { None },
        }
    })))
}

#[derive(Deserialize, Default)]
pub struct ProposeReq {
    /// For measurement (bench/agent.mjs): `full` = send the full vocabulary with each batch. The default is the per-batch trimmed one
    #[serde(default)]
    pub glossary: Option<String>,
}

/// Ask the agent to take a look: queue one job and return at once. The result lands in `ontology_proposals`, from which the UI reads
pub async fn propose(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<ProposeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let payload = if req.glossary.as_deref() == Some("full") {
        json!({ "kb_id": kb_id, "glossary": "full" })
    } else {
        json!({ "kb_id": kb_id })
    };
    let queued =
        utopia_store::jobs::enqueue_unless_queued(&state.pool, "propose_ontology", payload).await?;
    Ok(Json(json!({ "queued": queued.is_some() })))
}

#[derive(Deserialize)]
pub struct AdoptReq {
    pub section: String,
    pub key: String,
    #[serde(default, flatten)]
    pub edits: ontology_agent::AdoptEdits,
}

/// Accept one proposal (the few cells a person changed come with the request): elements, regression cases, the accepted status and follow-up tasks are committed in one submission.
/// On reuse, if after reading it the agent a person chose a different shape or category word from the proposal, it is left unwritten and listed in `skipped`
pub async fn adopt(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<AdoptReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let adopted =
        ontology_agent::adopt(&state, kb_id, &req.section, &req.key, req.edits, user.id).await?;
    Ok(Json(
        json!({ "id": adopted.id, "skipped": adopted.skipped }),
    ))
}
