use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::models::{LlmSettings, Role};
use utopia_llm::ChatMessage;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::llm_util;
use crate::state::AppState;

/// GET: redacted view (for keys, only whether they are configured is returned).
pub async fn get(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(workspace_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Admin).await?;
    let s = utopia_store::settings::get(&state.pool, workspace_id).await?;
    Ok(Json(match s {
        None => json!({}),
        Some(s) => json!({
            "chat_base_url": s.chat_base_url,
            "chat_model": s.chat_model,
            "chat_reasoning_effort": s.chat_reasoning_effort,
            "has_chat_key": s.chat_api_key.as_deref().is_some_and(|k| !k.is_empty()),
            "embed_base_url": s.embed_base_url,
            "embed_model": s.embed_model,
            "embed_dim": s.embed_dim,
            "has_embed_key": s.embed_api_key.as_deref().is_some_and(|k| !k.is_empty()),
            "ocr_provider": s.ocr_provider,
            "ocr_base_url": s.ocr_base_url,
            "ocr_backend": s.ocr_backend,
            "ocr_model": s.ocr_model,
            "has_ocr_key": s.ocr_api_key.as_deref().is_some_and(|k| !k.is_empty()),
            "transcribe_base_url": s.transcribe_base_url,
            "transcribe_model": s.transcribe_model,
            "has_transcribe_key": s.transcribe_api_key.as_deref().is_some_and(|k| !k.is_empty()),
        }),
    }))
}

#[derive(Deserialize)]
pub struct PutSettingsReq {
    pub chat_base_url: Option<String>,
    /// None or empty string = keep the old key
    pub chat_api_key: Option<String>,
    pub chat_model: Option<String>,
    /// minimal | low | medium | high; empty = field omitted, the endpoint uses its default. Absent = unchanged
    pub chat_reasoning_effort: Option<String>,
    pub embed_base_url: Option<String>,
    pub embed_api_key: Option<String>,
    pub embed_model: Option<String>,
    pub embed_dim: Option<i32>,
}

pub async fn put(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<PutSettingsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Admin).await?;
    let nonempty = |v: &Option<String>| -> Option<String> {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    utopia_store::settings::upsert(
        &state.pool,
        workspace_id,
        nonempty(&req.chat_base_url).as_deref(),
        nonempty(&req.chat_api_key).as_deref(),
        nonempty(&req.chat_model).as_deref(),
        nonempty(&req.embed_base_url).as_deref(),
        nonempty(&req.embed_api_key).as_deref(),
        nonempty(&req.embed_model).as_deref(),
        req.embed_dim,
    )
    .await?;
    if let Some(effort) = &req.chat_reasoning_effort {
        let effort = nonempty(&Some(effort.clone()));
        if let Some(e) = effort.as_deref() {
            if !matches!(e, "none" | "minimal" | "low" | "medium" | "high") {
                return Err(utopia_core::AppError::invalid(
                    "bad_reasoning_effort",
                    "reasoning effort is one of none, minimal, low, medium, high, or empty",
                )
                .into());
            }
        }
        utopia_store::settings::set_chat_reasoning_effort(
            &state.pool,
            workspace_id,
            effort.as_deref(),
        )
        .await?;
    }
    // **The moment the embedding model is configured is the earliest moment the ontology index can start.**
    //
    // The default KB created automatically at registration gets the ontology pack installed, but at that moment there is no model yet: `embed_ontology`
    // runs empty and reports done, and nothing queues it again after that. The ontology then sits in the KB with no vectors
    // until someone edits it or runs type resolution. So the default KB's first documents necessarily take the expensive, poor path of
    // "populate the whole ontology".
    //
    // Backfilling here builds the index while the user is still deciding what to upload. Extraction also has its own
    // fallback (see extraction.rs); this just brings it forward, and is not the only safeguard.
    if nonempty(&req.embed_model).is_some() {
        for kb in utopia_store::kbs::list(&state.pool, workspace_id).await? {
            let _ = utopia_store::jobs::enqueue_unless_queued(
                &state.pool,
                "embed_ontology",
                serde_json::json!({ "kb_id": kb.id }),
            )
            .await;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct PutOcrReq {
    /// `mineru` or `ark` (0065); when an old client does not send it, the existing provider is kept; new settings default to MinerU
    pub provider: Option<String>,
    /// Empty = off: scanned documents and images are degraded as in the first slice
    pub base_url: Option<String>,
    /// None or empty string = keep the old key; not kept when the provider changes
    pub api_key: Option<String>,
    pub backend: Option<String>,
    /// The vision model name for Ark; ignored by MinerU
    pub model: Option<String>,
}

/// Layout recognition service (0040 second slice). Its own endpoint: on the admin page it is its own card, and saving it must not
/// clear the chat and embedding columns according to empty form values.
///
/// The moment it is configured, scanned documents and images that stopped at failed in this workspace for lack of it are re-queued for processing;
/// the alert's "it will reprocess itself once configured" refers to this
pub async fn put_ocr(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<PutOcrReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Admin).await?;
    let nonempty = |v: &Option<String>| -> Option<String> {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let provider = nonempty(&req.provider);
    if provider
        .as_deref()
        .is_some_and(|p| !matches!(p, "mineru" | "ark"))
    {
        return Err(utopia_core::AppError::invalid(
            "unsupported_reader_provider",
            "The OCR provider must be mineru or ark",
        )
        .into());
    }
    let base_url = nonempty(&req.base_url);
    let saved = utopia_store::settings::upsert_ocr(
        &state.pool,
        workspace_id,
        provider.as_deref(),
        base_url.as_deref(),
        nonempty(&req.api_key).as_deref(),
        nonempty(&req.backend).as_deref(),
        nonempty(&req.model).as_deref(),
    )
    .await?;
    let mut requeued = 0usize;
    if saved.ocr_ready() {
        let docs =
            utopia_store::documents::requeue_waiting_for_reader(&state.pool, workspace_id, "ocr")
                .await?;
        requeued = docs.len();
        for (document_id, kb_id) in docs {
            state.emit_document(kb_id, document_id);
        }
    }
    Ok(Json(json!({ "ok": true, "requeued": requeued })))
}

#[derive(Deserialize)]
pub struct PutTranscribeReq {
    /// Address or model empty = off: recordings degrade as in the first slice
    pub base_url: Option<String>,
    /// None or empty string = keep the old key
    pub api_key: Option<String>,
    pub model: Option<String>,
}

/// Transcription model (0040 third slice). Like the layout recognition service, its own endpoint; once configured in this slice, the recordings in this workspace
/// waiting to be transcribed are re-queued, including those the previous model could not separate by speaker and rejected
pub async fn put_transcribe(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(workspace_id): Path<Uuid>,
    Json(req): Json<PutTranscribeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Admin).await?;
    let nonempty = |v: &Option<String>| -> Option<String> {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let (base_url, model) = (nonempty(&req.base_url), nonempty(&req.model));
    utopia_store::settings::upsert_transcribe(
        &state.pool,
        workspace_id,
        base_url.as_deref(),
        nonempty(&req.api_key).as_deref(),
        model.as_deref(),
    )
    .await?;
    let mut requeued = 0usize;
    if base_url.is_some() && model.is_some() {
        let docs = utopia_store::documents::requeue_waiting_for_reader(
            &state.pool,
            workspace_id,
            "transcribe",
        )
        .await?;
        requeued = docs.len();
        for (document_id, kb_id) in docs {
            state.emit_document(kb_id, document_id);
        }
    }
    Ok(Json(json!({ "ok": true, "requeued": requeued })))
}

/// Each of the three kinds of request the chat model actually sends by task is tried once (#1354): a plain question; a one-shot streamed request with a completion cap and
/// temperature 0 as extraction does; and a question with one tool. If only the plain question is sent, an endpoint that rejects `max_tokens`, `temperature` or tools
/// still "passes the test", and the fault only shows once extraction jobs fail en masse or chat quietly falls back to one-step retrieval.
/// If the plain question itself fails, the endpoint itself is unreachable, so the last two are not sent, to avoid reporting the same error three times. Each question needs only one word,
/// so it costs few tokens on a hosted model. The last two go out together, each capped at [`PROBE_LIMIT`]: an endpoint that accepts the connection but never
/// replies makes the person wait at most two caps, rather than three five-minute silent timeouts
async fn chat_checks(client: &utopia_llm::LlmClient) -> serde_json::Value {
    let msg = [ChatMessage {
        role: "user".into(),
        content: "Reply with exactly one word: OK".into(),
    }];
    let reply = match limited(client.chat(&msg)).await {
        Ok(reply) => reply.chars().take(50).collect::<String>(),
        Err(e) => return json!({ "ok": false, "error": e.to_string() }),
    };
    let tool = json!([{ "type": "function", "function": {
        "name": "ping", "description": "Answers pong.",
        "parameters": { "type": "object", "properties": {} } } }]);
    let ask = [json!({ "role": "user", "content": "Call the ping tool." })];
    let (streaming, tools) = tokio::join!(
        limited(async { client.chat_at_streaming(&msg, Some(0.0)).await.map(|_| ()) }),
        limited(async { client.chat_tools(&ask, &tool).await.map(|_| ()) }),
    );
    let failed: Vec<String> = [("streaming", &streaming), ("tools", &tools)]
        .into_iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| format!("{name}: {e}")))
        .collect();
    let mut out = json!({
        "ok": failed.is_empty(),
        "reply": reply,
        "checks": { "plain": { "ok": true }, "streaming": check(&streaming), "tools": check(&tools) },
    });
    if !failed.is_empty() {
        out["error"] = json!(format!("plain ok; {}", failed.join("; ")));
    }
    out
}

/// The cap on each question in the settings-page test
const PROBE_LIMIT: std::time::Duration = std::time::Duration::from_secs(45);

async fn limited<T>(f: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    limited_for(PROBE_LIMIT, f).await
}

async fn limited_for<T>(
    limit: std::time::Duration,
    f: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(limit, f)
        .await
        .unwrap_or_else(|_| Err(TimedOut(limit).into()))
}

/// A question that did not finish within its cap. Reported separately from a rejection (#1363 review): a rejection says the endpoint does not accept this shape, while a timeout says
/// it did not reply; the person reading it needs to check different things
#[derive(Debug)]
struct TimedOut(std::time::Duration);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no answer within {} s", self.0.as_secs())
    }
}

impl std::error::Error for TimedOut {}

/// Did not finish: hit the probe cap, or hit the probe client's own read timeout earlier (`chat_probe_client` is 30 s, shorter than the cap)
fn timed_out(e: &anyhow::Error) -> bool {
    e.downcast_ref::<TimedOut>().is_some()
        || e.chain().any(|c| {
            c.downcast_ref::<utopia_llm::Unreachable>()
                .is_some_and(|u| u.0.is_timeout())
        })
}

fn check(r: &anyhow::Result<()>) -> serde_json::Value {
    match r {
        Ok(()) => json!({ "ok": true }),
        Err(e) if timed_out(e) => {
            json!({ "ok": false, "timed_out": true, "error": e.to_string() })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

/// The four model cards on the settings page, each with a "Test"
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelCard {
    Chat,
    Embed,
    Ocr,
    Transcribe,
}

#[derive(Deserialize)]
pub struct TestQuery {
    /// Test only this card. Absent = test all four
    pub card: Option<ModelCard>,
}

/// Connectivity test: the chat model is tried once for each of the three request shapes by task (see [`chat_checks`]); embedding tries one item and
/// returns the dimension; the layout recognition service is pinged for health. With `?card=` ([`ModelCard`]) only that card is tested, and only its
/// item comes back; without it all four cards are tested (#1424).
pub async fn test(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(workspace_id): Path<Uuid>,
    Query(q): Query<TestQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::workspaces::require_role(&state.pool, user.id, workspace_id, Role::Admin).await?;
    let s = utopia_store::settings::get(&state.pool, workspace_id).await?;
    Ok(Json(probe(s.as_ref(), q.card).await))
}

/// Tests only the card that was clicked (#1424). Before, the four cards shared one test, so pressing any one of them tested all four at once; since #1363
/// a chat test sends three questions, and testing the embedding card or the scanned-document card wasted those tokens too. Only the items tested come back
async fn probe(s: Option<&LlmSettings>, only: Option<ModelCard>) -> serde_json::Value {
    let wants = |card| only.is_none_or(|c| c == card);
    let not_configured = || json!({ "ok": false, "error": "Not configured" });
    let mut out = serde_json::Map::new();

    if wants(ModelCard::Chat) {
        let chat_result = match s.and_then(llm_util::chat_probe_client) {
            None => not_configured(),
            Some(client) => chat_checks(&client).await,
        };
        out.insert("chat".into(), chat_result);
    }

    if wants(ModelCard::Embed) {
        let embed_result = match s.and_then(llm_util::embed_client) {
            None => not_configured(),
            Some(client) => match client.embed(&["connectivity test".to_string()]).await {
                Ok(v) if !v.is_empty() => json!({ "ok": true, "dim": v[0].len() }),
                Ok(_) => json!({ "ok": false, "error": "Empty response" }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            },
        };
        out.insert("embed".into(), embed_result);
    }

    if wants(ModelCard::Ocr) {
        let ocr_result = match s.and_then(crate::readers::Ocr::from_settings) {
            None => not_configured(),
            Some(ocr) => match ocr.health().await {
                Ok(h) => json!({ "ok": true, "version": h["version"] }),
                Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
            },
        };
        out.insert("ocr".into(), ocr_result);
    }

    if wants(ModelCard::Transcribe) {
        let transcribe_result = match s.and_then(crate::readers::Transcriber::from_settings) {
            None => not_configured(),
            Some(t) => match t.check().await {
                Ok(()) => json!({ "ok": true }),
                Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
            },
        };
        out.insert("transcribe".into(), transcribe_result);
    }

    out.into()
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// Only accepts plain and streamed requests, and returns 400 on seeing a tool; `down` returns 500 for everything. Each incoming request body is checked one by one by the tests
    async fn endpoint(down: bool) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |r: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                if down {
                    return ResponseTemplate::new(500).set_body_string("down");
                }
                if body.get("tools").is_some() {
                    return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                        "error": { "message": "tools are not supported by this deployment" } }));
                }
                if body["stream"] == true {
                    let frame = serde_json::json!({ "choices": [{ "index": 0, "delta": { "content": "OK" }, "finish_reason": "stop" }] });
                    return ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(format!("data: {frame}\n\ndata: [DONE]\n\n"));
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "role": "assistant", "content": "OK" }, "finish_reason": "stop" }] }))
            })
            .mount(&server)
            .await;
        server
    }

    async fn bodies(server: &MockServer) -> Vec<serde_json::Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn each_job_shape_is_sent_and_a_refused_one_is_named() {
        let server = endpoint(false).await;
        let out =
            super::chat_checks(&utopia_llm::LlmClient::new(&server.uri(), None, "scripted")).await;
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["checks"]["plain"]["ok"], true);
        assert_eq!(out["checks"]["streaming"]["ok"], true, "{out}");
        assert_eq!(out["checks"]["tools"]["ok"], false);
        let error = out["error"].as_str().unwrap();
        assert!(error.starts_with("plain ok; tools: "), "{error}");
        assert!(!error.contains("streaming:"), "{error}");

        let sent = bodies(&server).await;
        assert_eq!(sent.len(), 3, "{sent:?}");
        let plain = &sent[0];
        assert!(
            plain["stream"] == false
                && plain.get("tools").is_none()
                && plain.get("max_tokens").is_none(),
            "{plain}"
        );
        // The last two go out together, and their arrival order is not fixed
        let streamed = sent
            .iter()
            .find(|b| b["stream"] == true)
            .expect("a streamed request");
        assert_eq!(streamed["temperature"], 0.0, "{streamed}");
        assert!(
            streamed["max_tokens"].as_u64().is_some_and(|n| n > 0),
            "the job's completion ceiling rides along: {streamed}"
        );
        let tooled = sent
            .iter()
            .find(|b| b.get("tools").is_some())
            .expect("a request with a tool");
        assert_eq!(tooled["tools"][0]["function"]["name"], "ping");
    }

    #[tokio::test]
    async fn a_probe_that_runs_out_of_time_reads_as_timed_out_not_refused() {
        let slow = async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            anyhow::Ok(())
        };
        let r = super::limited_for(std::time::Duration::from_millis(20), slow).await;
        let c = super::check(&r);
        assert_eq!(c["timed_out"], true, "{c}");
        assert!(
            c["error"].as_str().unwrap().starts_with("no answer within"),
            "{c}"
        );
        let refused = super::check(&Err(anyhow::anyhow!("HTTP 400")));
        assert!(refused.get("timed_out").is_none(), "{refused}");
    }

    #[tokio::test]
    async fn a_read_timeout_before_the_limit_reads_as_timed_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(2)))
            .mount(&server)
            .await;
        let client = utopia_llm::LlmClient::with_timeouts(
            &server.uri(),
            None,
            "m",
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(100),
        );
        let msg = [utopia_llm::ChatMessage {
            role: "user".into(),
            content: "x".into(),
        }];
        let r = super::limited_for(std::time::Duration::from_secs(10), async {
            client.chat(&msg).await.map(|_| ())
        })
        .await;
        let c = super::check(&r);
        assert_eq!(c["timed_out"], true, "{c}");
    }

    #[tokio::test]
    async fn an_endpoint_that_is_down_is_reported_once() {
        let server = endpoint(true).await;
        let out =
            super::chat_checks(&utopia_llm::LlmClient::new(&server.uri(), None, "scripted")).await;
        assert_eq!(out["ok"], false);
        assert!(out.get("checks").is_none(), "{out}");
        assert_eq!(
            bodies(&server).await.len(),
            1,
            "the streamed and tool shapes are not sent when the plain one fails"
        );
    }

    /// #1424: a click on the embedding card's "Test" gets no chat-model question at all; a legacy call that does not name a card still returns all four items
    #[tokio::test]
    async fn testing_one_card_sends_nothing_to_the_others() {
        let chat = endpoint(false).await;
        let s = utopia_core::models::LlmSettings {
            workspace_id: uuid::Uuid::nil(),
            chat_base_url: Some(chat.uri()),
            chat_api_key: None,
            chat_model: Some("scripted".into()),
            embed_base_url: None,
            embed_api_key: None,
            embed_model: None,
            embed_dim: None,
            updated_at: chrono::Utc::now(),
            ocr_base_url: None,
            ocr_api_key: None,
            ocr_backend: None,
            ocr_provider: "mineru".into(),
            ocr_model: None,
            transcribe_base_url: None,
            transcribe_api_key: None,
            transcribe_model: None,
            chat_reasoning_effort: None,
        };

        let out = super::probe(Some(&s), Some(super::ModelCard::Embed)).await;
        assert_eq!(
            out,
            serde_json::json!({ "embed": { "ok": false, "error": "Not configured" } })
        );
        assert!(bodies(&chat).await.is_empty());

        let all = super::probe(Some(&s), None).await;
        let cards: Vec<_> = all.as_object().unwrap().keys().collect();
        assert_eq!(cards, ["chat", "embed", "ocr", "transcribe"], "{all}");
        assert_eq!(bodies(&chat).await.len(), 3);
    }
}
