//! Hands the conversation loop over to rig's runner (#546).
//!
//! The old `chat.rs` had a hand-written 1,100-line loop in which every policy question became yet another
//! branch; the guard for #509 (the model says "let me look it up" and then the turn just ends) (#543) was heuristics stacked on
//! heuristics: in practice it asked four times and the model answered DONE all four times. Here the loop is replaced with rig's: tools are
//! `DynamicTool`s, policy is hooks, and termination is **structural**:
//!
//! **A turn cannot end before at least one tool has been called.** The first request carries `tool_choice: required`,
//! until some tool has run. `no_evidence_needed` is the tool for questions that need no
//! library, such as "hello" or "say it shorter". So "please wait, I will call a tool" is no longer a possible final state:
//! the model either really calls one, or says plainly that the question needs no lookup.
//!
//! The fallback for endpoints that do not accept `required` (on a 400 `RigModel` drops it and resends; or they simply ignore it)
//! is one retry in `on_model_turn_finished`: if the first turn is only text and no tool has run, send it back to call one;
//! if it still does not call one, accept that. It is the endpoint's doing, and it is visible in the logs.

use super::tools::{self, ToolCtx, ToolSink};
use crate::state::AppState;
use rig_agent::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, HookContext, InvalidToolCallAction,
    InvalidToolCallContext, ModelTurnAction, ModelTurnFinished, RequestPatch, RetryRequest,
    ToolCall as ToolCallEvent, ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_agent::tool::{DynamicTool, ToolContext, ToolOutput};
use rig_core::message::{AssistantContent, Message, ToolChoice};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use utopia_core::models::DataSourceView;
use uuid::Uuid;

/// The tool for "this question needs no lookup". It exists so the `required` on the first turn has an honest exit:
/// not every question is about data in the library, and forcing a retrieval for "hello" is another kind of error
pub const NO_EVIDENCE_TOOL: &str = "no_evidence_needed";

const NO_EVIDENCE_DESCRIPTION: &str = "Declare that this question does not need the knowledge \
    base: a greeting, a question about this conversation itself, or a request to reword or \
    shorten a previous answer. Never use it for a question about facts, entities, documents, \
    dates or data; for those, call the tool that gathers the evidence. After calling it, answer \
    directly.";

/// The message sent when an endpoint ignored `required` and the first turn was text only. **There is no DONE exit**:
/// there is exactly one legitimate reason not to look anything up, which is calling `no_evidence_needed`
const MUST_CALL: &str = "(system) Every turn starts with a tool call. Call the tool that gathers \
    the evidence for this question, or call no_evidence_needed if the question is not about the \
    knowledge base at all. Do not describe a plan.";

/// The message re-asked when the model returns an **empty reply** (no body, no tool call) (#631).
///
/// An empty reply is not a conclusion, just silence. It used to become an `error` frame straight away: on the bench it happened 2 times in 48 turns, both after the tools had already finished.
/// The user saw three or four steps of research and then a single "Model returned an
/// empty answer" message, and everything found was wasted. It now re-asks once and only errors if the second attempt is also empty: only once, so an
/// endpoint that never speaks cannot make the loop spin.
///
/// The wording leaves room on both sides: if tools have run, answer from what was found; if no step was taken (the reply was empty on the first turn),
/// evidence is missing, so go and call a tool.
pub(crate) const EMPTY_REPLY_RETRY: &str = "(system) Your previous reply was empty. Reply to \
    the user now: answer from the evidence gathered above, or call a tool if you still need \
    evidence.";

/// The message sent back when the model wrote the tool call into the body text (#937).
///
/// Sometimes an endpoint does not use the protocol's `tool_calls` and instead writes the call marker verbatim as body text: DeepSeek's
/// `<｜tool▁call▁begin｜>function…` (in the bench's chat path, 2 of 35 questions went wrong), or DSML.
/// Nothing was executed, and that text is not an answer. On retry the turn is kept so the model can see what it tried to call;
/// only once: if it writes it again, error out with [`CONTROL_TEXT`]
pub(crate) const MARKUP_RETRY: &str = "(system) Your previous reply wrote a tool call out as \
    text, so nothing ran. Make the call as a tool call, or answer the user in plain text.";

/// Bounded buffering applies only to the tool-free terminal call. This is a byte
/// limit, independent of the provider's token accounting.
pub(crate) const MAX_FINAL_ANSWER_BYTES: usize = 1024 * 1024;
const FINAL_TOOL_CALL: &str = "Model attempted a tool call after the tool budget was exhausted";
pub(crate) const CONTROL_TEXT: &str = "Model returned tool-control text instead of a final answer";

/// Line starts that open a tool call written as text, as [`control_key`] spells
/// them: DSML, and DeepSeek's native markup (#937).
const CONTROL_MARKERS: [&str; 5] = [
    "<DSMLcalls>",
    "<DSMLtool_calls>",
    "<DSMLinvokename=",
    "<tool▁calls▁begin>",
    "<tool▁call▁begin>",
];

/// Accommodate the known ASCII/full-width and doubled-pipe spellings.
fn control_key(candidate: &str) -> String {
    candidate
        .chars()
        .take(80)
        .filter(|c| !c.is_whitespace() && *c != '|' && *c != '｜')
        .collect()
}

fn is_control(candidate: &str) -> bool {
    let key = control_key(candidate);
    CONTROL_MARKERS.iter().any(|marker| key.starts_with(marker))
}

/// A line still being written that could yet turn out to be a marker.
fn may_become_control(candidate: &str) -> bool {
    let key = control_key(candidate);
    CONTROL_MARKERS
        .iter()
        .any(|marker| marker.starts_with(key.as_str()))
}

/// Whether a reply is a tool call written out as text (#845, #937). Explanations
/// and fenced examples are prose, and an explicit request about the encoding may
/// legitimately ask for the raw markup. This is a publication guard: never
/// interpret this text as an executable tool call.
pub(crate) fn tool_call_text(text: &str, question: &str) -> bool {
    let request = question.to_ascii_lowercase();
    let asks_for_encoding = (request.contains("dsml") || request.contains("tool▁call"))
        && ["example", "verbatim", "示例", "原样"]
            .iter()
            .any(|term| request.contains(term))
        && !request.contains("business")
        && !request.contains("业务");
    if asks_for_encoding {
        return false;
    }
    // Inspect the assembled turn, so SSE chunk boundaries do not matter.
    let text = text.trim();
    if is_control(text) {
        return true;
    }
    // A real endpoint prefixed its final control block with "Let me examine
    // it." in the SAME turn. Check bare line starts too, without treating
    // inline mentions, block quotes, or Markdown fenced examples as calls.
    let mut fence: Option<(char, usize)> = None;
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(marker @ ('`' | '~')) = line.chars().next() {
            let len = line.chars().take_while(|c| *c == marker).count();
            if len >= 3 {
                match fence {
                    None => fence = Some((marker, len)),
                    Some((open, size))
                        if marker == open && len >= size && line[len..].trim().is_empty() =>
                    {
                        fence = None;
                    }
                    _ => {}
                }
                continue;
            }
        }
        if fence.is_none() && is_control(line) {
            return true;
        }
    }
    false
}

/// How much of a tool round's text can go out while it is still streaming (#937).
///
/// Text goes out line by line. A line is held while it could still become a
/// tool-call marker; once one has, the rest of the turn is held with it, to be
/// sent back or dropped. `from` is how much has gone out already, and the line
/// it ends in was judged then. Fences are not tracked here: a marker in a fenced
/// example is held until the turn ends, when [`tool_call_text`] releases it.
pub(crate) fn publishable(text: &str, from: usize, holding: &mut bool) -> usize {
    if *holding {
        return from;
    }
    let mut at = if from == 0 || text[..from].ends_with('\n') {
        from
    } else {
        match text[from..].find('\n') {
            Some(i) => from + i + 1,
            None => return text.len(),
        }
    };
    while at < text.len() {
        let rest = &text[at..];
        let (line, finished) = match rest.find('\n') {
            Some(i) => (&rest[..i], true),
            None => (rest, false),
        };
        if is_control(line) {
            *holding = true;
            return at;
        }
        if !finished {
            return if may_become_control(line) {
                at
            } else {
                text.len()
            };
        }
        at += line.len() + 1;
    }
    at
}

/// What keeps a budget-boundary candidate from being published: a structured
/// call, blank text, or a tool call written as text.
pub(crate) fn finalization_error(
    text: &str,
    has_calls: bool,
    question: &str,
) -> Option<&'static str> {
    if has_calls {
        return Some(FINAL_TOOL_CALL);
    }
    if text.trim().is_empty() {
        return Some("Model returned an empty answer");
    }
    tool_call_text(text, question).then_some(CONTROL_TEXT)
}

/// What the tools share within one conversation: the library, permissions, the citation list, and the trail shown to the UI.
///
/// rig runs several tools of the same turn concurrently, and citation numbers are stateful (`[3]` depends on how many were cited before),
/// so `sink` is an async lock: one tool finishes before the next one enters.
pub struct Shared {
    pub state: AppState,
    pub kb_id: Uuid,
    pub workspace_id: Uuid,
    pub mounted_sources: Vec<DataSourceView>,
    pub can_write: bool,
    pub actor: Uuid,
    /// The tool list (the same one shared with MCP); the `check_call` criteria come from here
    pub schema: Value,
    /// Model name written into logs, so calls can be counted per model
    pub model: String,
    /// The question the user asked this turn. Tools use it to rank same-named entities: "Zhang Wei's employer" and "Zhang Wei's papers"
    /// should resolve to different Zhang Weis (the hand-written loop on dev already passed this; the MCP path has no such question, so it passes None)
    pub question: String,
    pub sink: tokio::sync::Mutex<ToolSink>,
    /// The step left for the UI once a tool has run, looked up by rig's internal_call_id;
    /// calls rejected by `check_call` also leave a step here
    steps: Mutex<HashMap<String, (Value, bool)>>,
    /// Once any tool (including `no_evidence_needed`) has run, the `required` gate is passed
    gate_passed: AtomicBool,
    /// The fallback when the endpoint ignores `required` happens only once
    nudged: AtomicBool,
    /// The retry for an empty reply also happens only once (see `EMPTY_REPLY_RETRY`)
    asked_again: AtomicBool,
    /// The retry for tool calls written as body text also happens only once (see `MARKUP_RETRY`)
    markup_retried: AtomicBool,
    /// Set before the final request so the route can withhold unvalidated text.
    finalizing: AtomicBool,
    /// Only a rejected model candidate authorizes the one-shot recovery, not an
    /// authentication, credit, transport, or database error.
    answer_requested: AtomicBool,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: AppState,
        kb_id: Uuid,
        workspace_id: Uuid,
        mounted_sources: Vec<DataSourceView>,
        can_write: bool,
        actor: Uuid,
        schema: Value,
        model: String,
        question: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            kb_id,
            workspace_id,
            mounted_sources,
            can_write,
            actor,
            schema,
            model,
            question,
            sink: tokio::sync::Mutex::new(ToolSink::default()),
            steps: Mutex::new(HashMap::new()),
            gate_passed: AtomicBool::new(false),
            nudged: AtomicBool::new(false),
            asked_again: AtomicBool::new(false),
            markup_retried: AtomicBool::new(false),
            finalizing: AtomicBool::new(false),
            answer_requested: AtomicBool::new(false),
        })
    }

    pub fn finalizing(&self) -> bool {
        self.finalizing.load(Ordering::Relaxed)
    }

    pub fn take_answer_request(&self) -> bool {
        self.answer_requested.swap(false, Ordering::Relaxed)
    }

    fn keep_step(&self, internal_call_id: &str, step: Value, is_error: bool) {
        self.steps
            .lock()
            .expect("steps lock")
            .insert(internal_call_id.to_string(), (step, is_error));
    }

    /// Take the step left for the UI by this call (None = unknown tool, or a gate tool that leaves no trace)
    pub fn take_step(&self, internal_call_id: &str) -> Option<(Value, bool)> {
        self.steps
            .lock()
            .expect("steps lock")
            .remove(internal_call_id)
    }

    fn tool_ctx(&self) -> ToolCtx<'_> {
        ToolCtx {
            state: &self.state,
            kb_id: self.kb_id,
            workspace_id: self.workspace_id,
            mounted_sources: &self.mounted_sources,
            can_write: self.can_write,
            actor: Some(self.actor),
            // The web chat does not go through a token: the speaker is the person themselves
            via_token: None,
            question: Some(&self.question),
        }
    }
}

/// The step left in the rig tool context after a tool has run; `on_tool_result` reads it from there
#[derive(Clone)]
struct Step(Value, bool);

/// The tool list becomes rig's dynamic tools: name, description and parameter schema all come from `tools_schema`,
/// execution is still `tools::dispatch`. **The list is the single source of truth**; no second copy is kept here
pub fn dynamic_tools(shared: &Arc<Shared>) -> Vec<DynamicTool> {
    let mut out = Vec::new();
    for t in shared.schema.as_array().into_iter().flatten() {
        let f = &t["function"];
        let Some(name) = f["name"].as_str() else {
            continue;
        };
        let owned = shared.clone();
        let tool_name = name.to_string();
        out.push(DynamicTool::new(
            name,
            f["description"].as_str().unwrap_or_default(),
            f["parameters"].clone(),
            move |ctx: &mut ToolContext, args: Value| {
                let shared = owned.clone();
                let name = tool_name.clone();
                Box::pin(async move {
                    let tool_ctx = shared.tool_ctx();
                    // dispatch now returns a struct (#601 added structuredContent and
                    // is_error for MCP). The web chat only needs the body and the UI step, the same as the hand-written loop on dev.
                    let tools::ToolResult {
                        text: result,
                        step,
                        is_error,
                        ..
                    } = {
                        let mut sink = shared.sink.lock().await;
                        tools::dispatch(&tool_ctx, &mut sink, &name, &args).await
                    };
                    shared.gate_passed.store(true, Ordering::Relaxed);
                    ctx.insert_result(Step(step, is_error));
                    Ok(ToolOutput::text(result))
                })
            },
        ));
    }
    let owned = shared.clone();
    out.push(DynamicTool::new(
        NO_EVIDENCE_TOOL,
        NO_EVIDENCE_DESCRIPTION,
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "One short phrase: why no evidence is needed."
                }
            },
            "required": ["reason"]
        }),
        move |_ctx: &mut ToolContext, args: Value| {
            let shared = owned.clone();
            Box::pin(async move {
                tracing::info!(
                    model = shared.model,
                    reason = args["reason"].as_str().unwrap_or(""),
                    "模型声明这题不用查"
                );
                shared.gate_passed.store(true, Ordering::Relaxed);
                Ok(ToolOutput::text(
                    "Understood. Answer the user directly now.",
                ))
            })
        },
    ));
    out
}

/// The loop's policy, as rig hooks.
#[derive(Clone)]
pub struct Policy {
    pub shared: Arc<Shared>,
    pub cancellation: crate::live::Cancellation,
    /// The next logical call is handed over before the model service is accessed; tools are no longer executed.
    pub max_rounds: usize,
}

impl AgentHook for Policy {
    fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> impl std::future::Future<Output = CompletionCallAction> + Send {
        let turn = event.turn;
        self.shared
            .finalizing
            .store(turn > self.max_rounds, Ordering::Relaxed);
        let action = if self.cancellation.is_cancelled() {
            CompletionCallAction::Stop("Stopped by the user".into())
        } else if turn > self.max_rounds {
            // Rig 0.42 runs this hook before choosing the model or accessing the service.
            // The router only reads this turn's marker on PromptCancelled.
            self.shared.answer_requested.store(true, Ordering::Relaxed);
            CompletionCallAction::Stop("Evidence gathering complete".into())
        } else if !self.shared.gate_passed.load(Ordering::Relaxed) {
            // no tool has run yet: this turn must call one
            CompletionCallAction::Patch(RequestPatch::new().tool_choice(ToolChoice::Required))
        } else {
            CompletionCallAction::Continue
        };
        async move { action }
    }

    fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> impl std::future::Future<Output = ModelTurnAction> + Send {
        let has_tool_call = event
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        // Only reasoning, or a body that is entirely whitespace: to the user, that is not speaking
        let has_text = event
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if !t.text.trim().is_empty()));
        let written_call = !has_tool_call && {
            let text: String = event
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            tool_call_text(&text, &self.shared.question)
        };
        let turn = event.turn;
        let shared = self.shared.clone();
        let max_rounds = self.max_rounds;
        async move {
            // Re-ask once for an empty reply; if it is still empty, let it end, and `chat` finishes with "Model returned an empty
            // answer". **The retry below is not stacked on top**: an endpoint that never speaks gets only one extra ask
            if !has_tool_call && !has_text {
                if !shared.asked_again.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        model = shared.model,
                        turn,
                        "model returned empty, asking once more"
                    );
                    return ModelTurnAction::Retry(RetryRequest::Feedback(
                        EMPTY_REPLY_RETRY.into(),
                    ));
                }
                return ModelTurnAction::Continue;
            }
            // Tool call written into the body: nothing was executed, and this turn is not an answer either (#937). The same applies to every turn,
            // not just the first. Retry once; if it happens again, let it end, and `chat` holds back the unsent
            // marker and finishes with `CONTROL_TEXT`, without storing it
            if written_call {
                if !shared.markup_retried.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        model = shared.model,
                        turn,
                        "模型把工具调用写成了正文，退回一次"
                    );
                    return ModelTurnAction::Retry(RetryRequest::Feedback(MARKUP_RETRY.into()));
                }
                tracing::warn!(
                    model = shared.model,
                    turn,
                    "退回一次后仍把工具调用写成正文，按错误收尾"
                );
                return ModelTurnAction::Continue;
            }
            if has_tool_call || shared.gate_passed.load(Ordering::Relaxed) || turn > max_rounds {
                return ModelTurnAction::Continue;
            }
            // Text only, gate not passed: the endpoint ignored `required`. Send it back once
            if !shared.nudged.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    model = shared.model,
                    turn,
                    "首轮没有工具调用（端点未执行 tool_choice=required），退回要求调用"
                );
                return ModelTurnAction::Retry(RetryRequest::Feedback(MUST_CALL.into()));
            }
            tracing::warn!(
                model = shared.model,
                turn,
                "退回一次后仍无工具调用，按原文收尾（sources 为空）"
            );
            ModelTurnAction::Continue
        }
    }

    fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        _event: &InvalidToolCallContext,
    ) -> impl std::future::Future<Output = Option<InvalidToolCallAction>> + Send {
        // Rig rejects calls disallowed by ToolChoice::None before on_tool_call.
        // Mark that candidate for the same answer-only recovery; never ask the
        // runner to retry a forbidden tool call.
        let action = if self.shared.finalizing() {
            Some(InvalidToolCallAction::fail())
        } else {
            None
        };
        async move { action }
    }

    fn on_tool_call(
        &self,
        _ctx: &HookContext,
        event: ToolCallEvent<'_>,
    ) -> impl std::future::Future<Output = ToolCallAction> + Send {
        // **A call whose intent cannot be understood is not executed.** Tell the model and let it redo it;
        // the UI still shows it as a call that was not carried out
        let action = if self.cancellation.is_cancelled() {
            ToolCallAction::Stop("Stopped by the user".into())
        } else if self.shared.finalizing() {
            ToolCallAction::Stop(FINAL_TOOL_CALL.into())
        } else {
            match super::chat::check_call(&self.shared.schema, event.tool_name, event.args) {
                Ok(_) => ToolCallAction::Run,
                Err((message, step)) => {
                    self.shared.keep_step(event.internal_call_id, step, true);
                    ToolCallAction::Skip(message)
                }
            }
        };
        async move { action }
    }

    fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> impl std::future::Future<Output = ToolResultAction> + Send {
        if let Some(Step(step, is_error)) = event.tool_context.result::<Step>() {
            self.shared
                .keep_step(event.internal_call_id, step.clone(), *is_error);
        }
        async { ToolResultAction::Keep }
    }
}

/// Turn the stored history (OpenAI protocol JSON) back into rig messages.
///
/// The tool round-trip of the last turn goes before its conclusion, so the order is the real one and the model reads it as
/// "I asked, I looked, I answered". The protocol has no tool name on tool messages; it is recovered from the earlier
/// assistant message with tool_calls by id
#[cfg(test)]
pub fn history_messages(turns: &[(String, String)], last_tool_exchange: &[Value]) -> Vec<Message> {
    history_messages_with_reasoning(turns, last_tool_exchange, &[])
}

pub fn history_messages_with_reasoning(
    turns: &[(String, String)],
    last_tool_exchange: &[Value],
    reasoning: &[Option<String>],
) -> Vec<Message> {
    let last_assistant = turns.iter().rposition(|(role, _)| role == "assistant");
    let mut out = Vec::new();
    for (i, (role, content)) in turns.iter().enumerate() {
        if Some(i) == last_assistant {
            out.extend(exchange_messages(last_tool_exchange));
        }
        match role.as_str() {
            "assistant" => {
                let mut parts = Vec::new();
                if let Some(Some(text)) = reasoning.get(i) {
                    parts.push(AssistantContent::reasoning(text));
                }
                parts.push(AssistantContent::text(content));
                out.push(Message::Assistant {
                    id: None,
                    content: parts,
                });
            }
            _ => out.push(Message::user(content.clone())),
        }
    }
    out
}

fn exchange_messages(exchange: &[Value]) -> Vec<Message> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for m in exchange {
        match m["role"].as_str() {
            Some("assistant") => {
                let mut content = Vec::new();
                if let Some(reasoning) = m["reasoning_content"].as_str() {
                    content.push(AssistantContent::reasoning(reasoning));
                }
                if let Some(text) = m["content"].as_str().filter(|t| !t.is_empty()) {
                    content.push(AssistantContent::text(text));
                }
                for c in m["tool_calls"].as_array().into_iter().flatten() {
                    let id = c["id"].as_str().unwrap_or_default();
                    let name = c["function"]["name"].as_str().unwrap_or_default();
                    names.insert(id.to_string(), name.to_string());
                    content.push(AssistantContent::tool_call(
                        id,
                        name,
                        super::rig_model::args_value(
                            c["function"]["arguments"].as_str().unwrap_or("{}"),
                        ),
                    ));
                }
                if !content.is_empty() {
                    out.push(Message::Assistant { id: None, content });
                }
            }
            Some("tool") => {
                let id = m["tool_call_id"].as_str().unwrap_or_default();
                let name = names.get(id).cloned().unwrap_or_default();
                out.push(Message::tool_result(
                    id,
                    name,
                    m["content"].as_str().unwrap_or_default(),
                ));
            }
            _ => {}
        }
    }
    out
}

/// Entities already accepted in earlier turns are handed back along with their ids (as context documents for this turn; see `rig_model::wire`
/// for where it lands and in what role). Without it the model sees only the previous turn's final answer text and not
/// which ids it was given, so it searches again by name; with same-name ambiguity the two turns may land on different entities
pub fn known_entities_block(entities: &[Value], limit: usize) -> Option<String> {
    if entities.is_empty() {
        return None;
    }
    let lines: Vec<String> = entities
        .iter()
        .take(limit)
        .map(|e| {
            format!(
                "{} | {} | {}",
                e["id"].as_str().unwrap_or("?"),
                e["name"].as_str().unwrap_or("?"),
                e["type"].as_str().unwrap_or("?")
            )
        })
        .collect();
    Some(format!(
        "Entities already identified earlier in this conversation (id | name | type). \
         Call entity_facts with these ids directly; do not look them up by name again:\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finalization_checks_narrated_control_but_preserves_protocol_examples() {
        let control =
            "Let me examine it.\n\n<｜｜DSML｜｜ calls>\n<｜DSML｜ invoke name=\"lookup\">{}";
        assert!(finalization_error(control, false, "What happened?").is_some());
        assert!(finalization_error(control, false, "Return a DSML example verbatim").is_none());
        assert!(finalization_error(
            control,
            false,
            "请解释为什么出现 DSML，但请直接回答业务问题"
        )
        .is_some());
        for explanation in [
            "The encoding includes <｜DSML｜ calls> as a marker.",
            "Example:\n```xml\n<｜DSML｜ calls>\n```\nThis is the encoding.",
            "Example:\n~~~~xml\n```\n<｜DSML｜ calls>\n~~~~",
            "> <｜DSML｜ calls>\nThis quotes the encoding.",
        ] {
            assert!(finalization_error(explanation, false, "Explain the encoding").is_none());
        }
        let after_example = "```xml\n<｜DSML｜ calls>\n```\nLet me check.\n<|DSML|calls>";
        assert!(finalization_error(after_example, false, "What happened?").is_some());
    }

    /// DeepSeek's native markers are also tool calls written as body text (#937): both kinds of pipe are recognised, and so are line starts after narration;
    /// inline mentions, citations, examples inside fences, and questions that explicitly ask for verbatim examples do not count
    #[test]
    fn deepseek_markup_is_a_tool_call_written_as_text() {
        let call =
            "<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>search_chunks\n\
                    ```json\n{\"query\":\"Acme\"}\n```<｜tool▁call▁end｜><｜tool▁calls▁end｜>";
        let question = "What changed at Acme?";
        assert!(tool_call_text(call, question));
        assert!(tool_call_text(&call.replace('｜', "|"), question));
        assert!(tool_call_text(
            &format!("Let me search.\n\n{call}"),
            question
        ));
        assert!(tool_call_text(
            "<｜tool▁call▁begin｜>function<｜tool▁sep｜>search_chunks",
            question
        ));
        for prose in [
            "The reply contained <｜tool▁calls▁begin｜> in the middle of a sentence.",
            "> <｜tool▁calls▁begin｜>\nThat line quotes the markup.",
            "It looks like this:\n~~~\n<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>\n~~~",
            "Acme's revenue rose 5% [1].",
        ] {
            assert!(!tool_call_text(prose, question), "{prose}");
        }
        assert!(!tool_call_text(
            call,
            "Show a <｜tool▁calls▁begin｜> example verbatim"
        ));
        assert_eq!(
            finalization_error(call, false, question),
            Some(CONTROL_TEXT)
        );
    }

    /// Streamed output goes out line by line: a line that might turn into a marker is held back; once it is a marker, it and the rest of the turn are held back too;
    /// everything else goes out as usual, and a line already half sent is no longer judged
    #[test]
    fn a_line_is_held_while_it_may_become_a_tool_call() {
        // (turn body, bytes already sent, where to send up to, whether the whole turn is held back from then on)
        for (text, from, out, held) in [
            ("Let me search.\n<｜to", 0, "Let me search.\n", false),
            (
                "Let me search.\n<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>",
                15,
                "Let me search.\n",
                true,
            ),
            ("One.\n\nTwo.\n  ", 0, "One.\n\nTwo.\n", false),
            ("<", 0, "", false),
            ("<b>Acme</b> grew", 0, "<b>Acme</b> grew", false),
            ("Acme grew\n<b>", 9, "Acme grew\n<b>", false),
            (
                "Acme grew <｜tool▁calls▁begin｜> inline",
                9,
                "Acme grew <｜tool▁calls▁begin｜> inline",
                false,
            ),
            ("Acme grew\n<|tool▁call▁begin|>", 9, "Acme grew\n", true),
            ("<|DSML|calls>\nMore.", 0, "", true),
        ] {
            let mut holding = false;
            let upto = publishable(text, from, &mut holding);
            assert_eq!(&text[..upto], out, "{text:?} from {from}");
            assert_eq!(holding, held, "{text:?} from {from}");
        }
        let mut holding = true;
        assert_eq!(publishable("Held.\nStill held.", 0, &mut holding), 0);
    }

    /// The tool round-trip of the previous turn is inserted before its conclusion; the tool message gets its own tool name back
    #[test]
    fn the_last_exchange_sits_before_its_conclusion() {
        let turns = vec![
            ("user".to_string(), "q1".to_string()),
            ("assistant".to_string(), "a1".to_string()),
            ("user".to_string(), "q2".to_string()),
        ];
        let exchange = vec![
            json!({ "role": "assistant", "content": null, "tool_calls": [
                { "id": "c1", "type": "function",
                  "function": { "name": "search_chunks", "arguments": "{\"query\":\"x\"}" } }
            ]}),
            json!({ "role": "tool", "tool_call_id": "c1", "content": "found" }),
        ];
        let msgs = history_messages(&turns, &exchange);
        assert_eq!(msgs.len(), 5);
        assert!(matches!(&msgs[0], Message::User { .. }));
        match &msgs[1] {
            Message::Assistant { content, .. } => match &content[0] {
                AssistantContent::ToolCall(tc) => {
                    assert_eq!(tc.id.as_str(), "c1");
                    assert_eq!(tc.function.arguments["query"], "x");
                }
                other => panic!("expected the tool call, got {other:?}"),
            },
            other => panic!("expected the exchange first, got {other:?}"),
        }
        match &msgs[2] {
            Message::User { content } => match &content[0] {
                rig_core::message::UserContent::ToolResult(r) => {
                    assert_eq!(r.call.as_str(), "c1");
                    assert_eq!(r.name, "search_chunks");
                }
                other => panic!("expected the tool result, got {other:?}"),
            },
            other => panic!("expected the tool result, got {other:?}"),
        }
        assert!(matches!(&msgs[3], Message::Assistant { .. }));
        assert!(matches!(&msgs[4], Message::User { .. }));
    }

    #[test]
    fn the_entity_block_lists_id_name_type_and_stops_at_the_limit() {
        assert!(known_entities_block(&[], 20).is_none());
        let entities = vec![
            json!({ "id": "e1", "name": "Acme", "type": "Organization" }),
            json!({ "id": "e2", "name": "Bob", "type": "Person" }),
        ];
        let block = known_entities_block(&entities, 1).unwrap();
        assert!(block.contains("e1 | Acme | Organization"));
        assert!(!block.contains("e2"));
        assert!(block.starts_with("Entities already identified"));
    }
}
