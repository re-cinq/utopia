//! utopia-llm: a thin client for the OpenAI-compatible protocol.
//! One codebase adapts to DeepSeek / Qwen (DashScope compatibility mode) / GLM / OpenAI / Ollama / vLLM.

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Tool call under the OpenAI protocol (carried on assistant turns).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON string arguments (passed through verbatim per the protocol)
    pub arguments: String,
    /// Opaque data the endpoint attaches to a call, passed back unchanged. Gemini 3 puts one
    /// `extra_content.google.thought_signature` on each function call; when the results are sent back in the same turn it **must**
    /// be echoed back unchanged, otherwise 400 "Function call is missing a thought_signature"; the one from the previous turn
    /// may be omitted. Contents are not interpreted; anything that is not an object is treated as absent.
    pub extra_content: Option<serde_json::Value>,
}

/// One assistant turn of a tool conversation: at least one of text or tool calls.
#[derive(Debug)]
pub struct AssistantTurn {
    /// Preserve the provider value; absent is not an implicit `stop`.
    pub finish_reason: Option<String>,
    pub content: Option<String>,
    /// Provider reasoning, kept apart from visible text and replayed for tool continuation.
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

impl AssistantTurn {
    /// Converts back to an OpenAI-protocol assistant message (used when replaying conversation history).
    pub fn to_message(&self) -> serde_json::Value {
        let mut msg = json!({ "role": "assistant", "content": self.content });
        if let Some(reasoning) = &self.reasoning_content {
            msg["reasoning_content"] = json!(reasoning);
        }
        if !self.tool_calls.is_empty() {
            msg["tool_calls"] = json!(self
                .tool_calls
                .iter()
                .map(|c| {
                    let mut call = json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    });
                    if let Some(extra) = &c.extra_content {
                        call["extra_content"] = extra.clone();
                    }
                    call
                })
                .collect::<Vec<_>>());
        }
        msg
    }
}

/// Decides which call a tool_call delta belongs to. If it has `index`, that wins; if not, a delta with `id`
/// starts a new call, and a delta with neither is a continuation of the previous call.
///
/// **Endpoints that don't write `index` also don't number parallel calls.** Gemini's OpenAI-compatible endpoint sends one
/// call per chunk, with no `index` but with `id`, and all arguments at once; two parallel calls both merge under 0,
/// and the name becomes `find_entitiessearch_chunks`, so the whole round is voided as "unknown tool". The continuation
/// check is the absence of `id`: OpenAI continuations carry only argument fragments, and only the opening chunk carries `id`.
fn tool_call_slot(delta: &serde_json::Value, seen: usize) -> usize {
    if let Some(index) = delta["index"].as_u64() {
        return index as usize;
    }
    let starts_a_call = delta["id"].as_str().is_some_and(|id| !id.is_empty());
    if starts_a_call || seen == 0 {
        seen
    } else {
        seen - 1
    }
}

/// Tool result message (role=tool).
pub fn tool_result_message(tool_call_id: &str, content: &str) -> serde_json::Value {
    json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content })
}

/// The `extra_content` the endpoint attaches to a tool call: only counts if it is an object; other shapes are not guessed.
fn extra_content_of(call: &serde_json::Value) -> Option<serde_json::Value> {
    call.get("extra_content").filter(|v| v.is_object()).cloned()
}

/// Streaming turn event with tools.
#[derive(Debug)]
pub enum ToolStreamItem {
    /// Incremental text (forwarded to the frontend immediately)
    Delta(String),
    /// Stream end: the complete turn (accumulated text + merged tool calls)
    Turn(AssistantTurn),
}

/// **Could not get a parseable answer from the endpoint.** Two cases: cannot connect (DNS, connection, TLS, timeout),
/// or connected, the status says success, but what came back cannot be parsed as JSON - this is not the API at all.
///
/// Lumping these two together is deliberate: from the user's side they are the same thing, "the address you configured is not a model API",
/// and the fix is the same: check the URL and the proxy. The first version only catches transport-layer failures; the most common
/// failure (wrong URL, a proxy in between returning HTML) produces no alert at all,
/// which is exactly the "silent failure" itself.
///
/// **Does not include** 4xx/5xx returned by the endpoint, even if the error body is not JSON: the server did answer,
/// and the error body is itself the diagnosis - a different problem, with different people to look at it.
///
/// Typed rather than matched on error text: any layer on the call chain that adds a context line changes the text,
/// while the source chain of `anyhow` lets `downcast_ref` see it all the way down.
#[derive(Debug, thiserror::Error)]
#[error("LLM endpoint gave no usable answer: {0}")]
pub struct Unreachable(#[from] pub reqwest::Error);

/// Whether the anyhow error chain contains an [`Unreachable`].
pub fn is_unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<Unreachable>())
}

/// The endpoint is rate limiting. **Made a type just like [`Unreachable`]**, for the same reason: the caller must use it to decide
/// "wait a while and try again" rather than "this chunk is dead", and error text keeps being rewritten by context along the way.
///
/// The difference between rate limiting and other 4xx is that **it clears up by itself**. A wrong key stays wrong no matter how often you retry; an exhausted quota
/// is gone after a minute. Mixing the two would spend the retry budget on the kind that never gets better.
#[derive(Debug, thiserror::Error)]
#[error("LLM endpoint is rate limiting ({status}): {detail}")]
pub struct RateLimited {
    pub status: u16,
    /// **Often `None`.** Most vendors' 429 has no `Retry-After` (measured: SiliconFlow
    /// doesn't send one), so callers must supply their own backoff and treat this field as a "more precise if present" supplement, not as the criterion.
    pub retry_after: Option<Duration>,
    pub detail: String,
}

/// The [`RateLimited`] in the anyhow error chain, passing through the context layers.
pub fn rate_limited(err: &anyhow::Error) -> Option<&RateLimited> {
    err.chain().find_map(|e| e.downcast_ref::<RateLimited>())
}

/// The failure kinds worth retrying, along with how long each says to wait. **The criterion is "it clears up by itself"**:
/// the endpoint is rate limiting, the endpoint is unavailable for the moment (502/503/504), or the request never arrived (connection reset, response cut off).
///
/// **Read timeouts are not in there.** `READ_TIMEOUT` is 300 seconds without a first byte; five retries is 25 minutes;
/// a call that always times out does not get better by waiting longer. It needs smaller chunks or a faster endpoint.
///
/// Measured: a 32-chunk document hit both kinds within one hour: 502 four times, request-not-delivered twice.
pub fn transient(err: &anyhow::Error) -> Option<(&'static str, Option<Duration>)> {
    if let Some(hit) = rate_limited(err) {
        return Some(("LLM endpoint is rate limiting", hit.retry_after));
    }
    if let Some(hit) = unavailable(err) {
        return Some(("LLM endpoint is unavailable", hit.retry_after));
    }
    if err.chain().any(|e| e.is::<Interrupted>()) {
        return Some(("LLM response stream was interrupted", None));
    }
    let sending = err
        .chain()
        .find_map(|e| e.downcast_ref::<Unreachable>())
        .filter(|u| !u.0.is_timeout());
    sending.map(|_| ("LLM request could not be sent", None))
}

/// The endpoint opened and then dropped halfway: the stream was cut off in the middle of a sentence, with neither `[DONE]` nor `finish_reason`.
///
/// **Typed because it looks like success.** A half-finished reply is a legal string; the caller cannot tell
/// that it should have been longer. Extraction would treat it as the model's complete answer, and the missing statements would vanish silently.
#[derive(Debug, thiserror::Error)]
#[error("LLM stream ended in the middle of the answer ({got} chars in)")]
pub struct Interrupted {
    /// How much had been assembled when it dropped. When reporting a fault, it shows the drop was not right from the first byte.
    pub got: usize,
}

/// The endpoint is unavailable for the moment: 502 / 503 / 504, or 408 if it says so itself. **Same class as [`RateLimited`],
/// same reason: it clears up by itself.** Gateway glitches, upstream restarts, and queue timeouts all last seconds to tens of seconds,
/// while 400 (invalid prompt) and 401 (wrong key) stay wrong no matter how often you retry.
///
/// The cost of mixing these into [`Rejected`] was measured: a local proxy returned 502 for an upstream `fetch failed`,
/// and in a 32-chunk document a few random chunks were lost that way, failing the whole extraction and forcing a rerun. A 5% per-call failure rate
/// makes a long document almost always fail the first pass (1 - 0.95^32 is about 81%), while the task only retries three times.
#[derive(Debug, thiserror::Error)]
#[error("LLM endpoint is unavailable ({status}): {detail}")]
pub struct Unavailable {
    pub status: u16,
    /// The endpoint's `Retry-After` (more accurate if present; most don't send one)
    pub retry_after: Option<Duration>,
    pub detail: String,
}

/// The [`Unavailable`] in the anyhow error chain, passing through the context layers.
pub fn unavailable(err: &anyhow::Error) -> Option<&Unavailable> {
    err.chain().find_map(|e| e.downcast_ref::<Unavailable>())
}

/// The account cannot pay for this request: out of credit, or the plan quota used up.
///
/// **Separate from [`RateLimited`], because it does not clear up by itself.** Rate limiting is over after a minute;
/// out of credit stays out of credit until someone pays. Retrying only repeats the same error, and the thing that should happen
/// (someone topping up) does not happen because of retries.
///
/// Measured: in one test run 14 documents failed whole because of it, and it took the same path as ordinary failures.
/// The only way to learn the cause was to look through `graph_error` in the database.
#[derive(Debug, thiserror::Error)]
#[error("LLM account cannot pay for this request ({status}): {detail}")]
pub struct OutOfCredit {
    pub status: u16,
    pub detail: String,
}

/// The [`OutOfCredit`] in the anyhow error chain, passing through the context layers.
pub fn out_of_credit(err: &anyhow::Error) -> Option<&OutOfCredit> {
    err.chain().find_map(|e| e.downcast_ref::<OutOfCredit>())
}

/// The endpoint answered, and the answer was "no": every non-2xx except out-of-credit and rate limiting.
///
/// **Made a type so the status code survives.** The conversation loop must do different things for "this provider does not support tool calling"
/// (400, request shape rejected) versus "the network is down" or "the key is wrong". The former falls back to one-shot
/// RAG that can still answer; the latter cannot be answered even after falling back. Previously this class was only a block of text with the status code buried in the string,
/// so the loop treated **any** first-turn error as "tools unsupported". One network blip fell back to RAG,
/// and then RAG died on the same blip.
#[derive(Debug, thiserror::Error)]
#[error("{kind} request failed ({reason}): {detail}")]
pub struct Rejected {
    /// Which kind of request (`LLM` / `Embedding`); it only goes into the error text
    pub kind: String,
    pub status: u16,
    /// The status code carries its reason phrase, e.g. `400 Bad Request`. The error text has always been written this way
    pub reason: String,
    pub detail: String,
}

/// The [`Rejected`] in the anyhow error chain, passing through the context layers.
pub fn rejected(err: &anyhow::Error) -> Option<&Rejected> {
    err.chain().find_map(|e| e.downcast_ref::<Rejected>())
}

/// The prompt exceeded the model's context window. It is neither "tools unsupported" nor the completion cap (#964).
///
/// The wording stays neutral and keeps the status code: `failure` is shared by extraction, governance and conversation. An oversized chunk
/// is not "the conversation is too long"; the conversation's own sentence lives in the UI copy (error code `context_too_long`)
#[derive(Debug, thiserror::Error)]
#[error("LLM request failed ({reason}): the prompt exceeds the model's context window: {detail}")]
pub struct ContextTooLong {
    pub status: u16,
    /// The status code carries its reason phrase, same as `Rejected`
    pub reason: String,
    pub detail: String,
    /// The window reported by the endpoint (tokens). Only taken when the number directly follows the word "window" and is not less
    /// than `MIN_CONTEXT_WINDOW_TOKENS`.
    pub window: Option<u32>,
}

/// Default character budget for history. DeepSeek once accepted a 250k-token prompt (measured 2026-09-27),
/// so this figure is far below any window. Its job is to keep twenty rounds of tables from being carried back verbatim, not to hug the window.
const DEFAULT_HISTORY_CHARS: usize = 32_000;
/// Lower bound of the history budget. A learned window only goes down, and is used until the settings change or a restart. An error that reports a small number
/// must not starve the history (#973 review)
const MIN_HISTORY_CHARS: usize = 2_000;
/// A "window" smaller than this is not trusted, for the same reason as `stated_completion_ceiling`: no model has a window that small,
/// so the number is most likely something else.
const MIN_CONTEXT_WINDOW_TOKENS: u32 = 1024;

pub fn context_too_long(err: &anyhow::Error) -> Option<&ContextTooLong> {
    err.chain().find_map(|e| e.downcast_ref::<ContextTooLong>())
}

/// The integer-seconds form of `Retry-After`.
///
/// The spec also allows HTTP-date, but this **does not parse it**: pulling in a date library for a header few people send is not worth it.
/// Treating a parse failure as absent is exactly right. Callers need backoff anyway, and one more guessed number would only make things harder to debug.
fn retry_after_of(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Non-2xx responses are shaped here in one place, into three classes: out of credit, rate limiting, other.
///
/// **503 is not rate limiting**: it may be the endpoint really down, or an intermediate proxy. Counting it in would make
/// "wait and retry" apply where waiting never helps. Keep the criterion narrow; falling back to "other" is preferred.
fn failure(
    kind: &str,
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    body: &serde_json::Value,
    raw: &str,
) -> anyhow::Error {
    let detail = err_detail(body, raw);
    // **Out-of-credit must be checked first, and not by status code alone.**
    //
    // 402 is the standard answer (SiliconFlow uses it), but OpenAI's out-of-balance comes back as **429**,
    // and is told apart by `insufficient_quota` in the body. Classifying by status code alone, a broke
    // OpenAI account would be treated as rate limiting, and backed off and retried forever on something that never gets better.
    // The longer the backoff, the more the symptom looks like "the endpoint is slow", and the harder the root cause is to find.
    if status == reqwest::StatusCode::PAYMENT_REQUIRED || says_out_of_credit(body) {
        return anyhow::Error::new(OutOfCredit {
            status: status.as_u16(),
            detail,
        });
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return anyhow::Error::new(RateLimited {
            status: status.as_u16(),
            retry_after,
            detail,
        });
    }
    // The gateway and queueing ones: wait and retry. **500 is not in here**. It can be a bug in the endpoint itself,
    // and retrying only triggers the same crash again
    if matches!(
        status,
        reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    ) {
        return anyhow::Error::new(Unavailable {
            status: status.as_u16(),
            retry_after,
            detail,
        });
    }
    if kind == "LLM"
        && matches!(status.as_u16(), 400 | 413 | 422)
        && says_context_too_long(body, &detail)
    {
        return anyhow::Error::new(ContextTooLong {
            status: status.as_u16(),
            reason: status.to_string(),
            window: stated_context_window(&detail),
            detail,
        });
    }
    anyhow::Error::new(Rejected {
        kind: kind.to_string(),
        status: status.as_u16(),
        reason: status.to_string(),
        detail,
    })
}

/// Turns a non-success status response into an error (#527).
///
/// **Check the status code first, then decide how to read the body.** Previously success and failure both called `resp.json()` first, so a plain-text
/// 400 (LM Studio's "request (105140 tokens) exceeds the available context") could not be parsed as JSON and was classed as `Unreachable`,
/// so reporters had to set up a proxy to see it. Now the body is read as raw text only once:
/// if it is JSON, the message is taken from several common shapes; if not, a chunk of the raw text is quoted.
///
/// Only when the body **cannot be read** (the connection dropped midway) is it still `Unreachable`. That is a transport-layer matter.
async fn response_failure(
    kind: &str,
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    response: reqwest::Response,
) -> anyhow::Result<anyhow::Error> {
    let raw = response.text().await.map_err(Unreachable)?;
    let body = serde_json::from_str(&raw).unwrap_or_default();
    Ok(failure(kind, status, retry_after, &body, &raw))
}

/// Result of a streaming Q&A: the full answer, plus the endpoint's closing reason.
///
/// **Why the reason is carried along:** `finish_reason: length` is the endpoint saying "I was cut off",
/// and a truncated reply looks exactly like a complete one. The parser only sees the JSON missing its tail, not
/// why it is missing. With the reason attached, the drop can say "hit the token limit", not just "could not parse".
#[derive(Debug)]
pub struct Reply {
    pub text: String,
    /// The endpoint's closing reason (`stop` / `length` / ...). If the stream has none, this is `None`:
    /// some implementations only send `[DONE]`, so absence does not mean the answer is complete.
    pub finish_reason: Option<String>,
    /// Usage reported by the endpoint (last frame). If it reports none, this is `None`. No numbers are made up for the books.
    pub usage: Option<Usage>,
    /// The reply was repeating itself and we stopped reading (#1301); this is not the endpoint's closing reason. Recorded separately from `finish_reason`:
    /// if recorded as `length`, the drop line would say "hit the token limit", and whoever reads it would go check that limit
    pub repeating: bool,
}

/// Token usage of a single call, as reported by the endpoint itself
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Reply {
    /// The endpoint says this segment was cut off by the token limit.
    pub fn hit_token_ceiling(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

#[derive(Default)]
struct SseBuffer {
    bytes: Vec<u8>,
    after_cr: bool,
}

impl SseBuffer {
    fn push(&mut self, chunk: &[u8]) {
        // SSE accepts LF, CRLF and bare CR. Remember CR across HTTP chunks so its
        // following LF cannot turn one line ending into a spurious empty line.
        // Keep UTF-8 as bytes until a whole frame has arrived.
        for &byte in chunk {
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            self.bytes.push(if self.after_cr { b'\n' } else { byte });
        }
    }

    fn next_frame(&mut self) -> Option<String> {
        let end = self.bytes.windows(2).position(|w| w == b"\n\n")?;
        let frame = String::from_utf8_lossy(&self.bytes[..end]).into_owned();
        self.bytes.drain(..end + 2);
        Some(frame)
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    pub model: String,
    /// `reasoning_effort` for the OpenAI-compatible endpoint; when set, it goes into every conversation request body
    reasoning_effort: Option<String>,
    /// The completion cap sent in streaming calls. It starts at [`MAX_COMPLETION_TOKENS`]; when the endpoint's 400 states its own lower
    /// cap, it is recorded here, and the same client (and its clones) sends by it from then on, so one process eats one rejection
    /// (#891, #892 review). Only goes down, never up.
    completion_ceiling: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Parameters the endpoint has rejected are recorded on the client and shared with clones; the next chunk does not spend another 400 on the same kind of request (#1376).
    use_max_completion_tokens: std::sync::Arc<std::sync::atomic::AtomicBool>,
    suppress_temperature: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Context window (tokens) reported by the endpoint, starts at `u32::MAX`. The same client (and its clones) from then on
    /// computes the history budget from it, only going down. The conversation client is kept per workspace, so the next turn remembers too (#964).
    context_window: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Set once the endpoint says requests with tools do not accept a reasoning effort (#1353). From then on, requests with tools write
    /// `reasoning_effort: "none"`, and other calls still send the setting as configured. The same client (and clones) takes the rejection only once
    tools_without_effort: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How long to connect before it counts as failed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// **How long without new bytes before the request counts as dead.**
///
/// Cannot use `Client::timeout` (total request duration): `chat_tools_stream` is truly streaming,
/// and a long conversation legitimately runs for minutes. A total cap would cut it off in the middle. `read_timeout`
/// measures **silence**. With streaming, tokens keep arriving and it never trips; in non-streaming, what it guards
/// is exactly "request sent and vanished into silence".
///
/// Why 300 seconds and not 60: in a non-streaming call the first byte waits for the model to finish generating the whole reply,
/// and extraction with a large prompt normally takes 60-120 seconds, more when the server queues. Set too small, it would kill normal requests.
/// On this path a false kill costs far more than finding out five minutes later: it would fail extractions that could have succeeded.
///
/// **The cost of not having it was measured:** `reqwest::Client::new()` sets no timeout by default.
/// With 32 concurrent requests to one account, all requests hung and the 32 worker slots stayed occupied forever,
/// and the pipeline stalled with **no error reported at all**. The orphan reaper for jobs only runs once at process startup,
/// so while the process lives it never reaps. One ingestion of 7459 chunks died at chunk 55.
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// **How many tokens we allow a reply** (#760).
///
/// Without this field, where the answer gets cut is decided by the endpoint's own default. With reasoning off, a dense body chunk's
/// answer measured stopping at **exactly 4,096** completion tokens, with `finish_reason` set to `length`, so the JSON could not be parsed.
/// The cap is ours, so the answer stops where we decide.
///
/// Why 65,536 and not sized to the answer: `max_tokens` means different things across vendors.
/// Anthropic counts thinking within it, and OpenAI's `max_completion_tokens` also counts reasoning tokens.
/// On such endpoints the number is not the answer's ceiling but **the thinking budget**. And
/// 0044's "What a reasoning cap costs" measured that capping thinking at around 15-16 thousand dropped 885 statements to
/// 729, and the table-figure misses rose from 2% to 16%. That is exactly the lever that was rejected, and it must not sneak back in here.
///
/// The largest completion measured on this path was 45,428 tokens (with reasoning on). Going one tier above that
/// keeps it forever as "a guard against the endpoint's 4,096 default", never a reasoning cap. The answer itself is about
/// 1,500 tokens, far from this line.
///
/// **With reasoning on, this endpoint does not read it.** A call capped at 4,096 returned
/// 45,428 completion tokens and still finished normally. Sending it is therefore "useful where it is read, no loss where it isn't",
/// not a guarantee that every endpoint honors.
const MAX_COMPLETION_TOKENS: u32 = 65_536;

/// Reasoning models' thinking process must not reach downstream (#690).
///
/// Reasoning models such as DeepSeek-V4-Flash write their thinking inline as `<think>…</think>` in
/// `message.content`, rather than in a separate `reasoning_content` field. This code only reads
/// `content`, so that field is never touched. The consequences were seen on two fronts: `settings/test` got back
/// `"OK</think>OK"`, and extraction's `json_block` takes "first `{` to last `}`", so a pair of braces
/// inside the thinking can throw off the start and end positions.
///
/// Cut off **everything before the last** `</think>`. It is a protocol marker, not body vocabulary;
/// taking the last one guards against the body quoting it. With no marker it is returned as-is, so non-reasoning models
/// go through here unaffected.
///
/// Applies only to non-streaming `chat`: extraction, adjudication, and connectivity tests all use it. The streaming path emits text to the UI chunk by chunk,
/// and cutting the front half would misalign the text already emitted. That is a separate problem, not handled here.
fn strip_reasoning(reply: &str) -> &str {
    match reply.rfind("</think>") {
        Some(pos) => &reply[pos + "</think>".len()..],
        None => reply,
    }
}

/// Whether the tail of the reply is the same segment repeating (#1301).
///
/// Small models at temperature 0 fall into repetition: the same statement line written over and over until it hits
/// [`MAX_COMPLETION_TOKENS`]. About 3% of extraction calls on local qwen3-8b do this, each holding a slot
/// for over an hour and eating 60% of the generation volume. The cap itself cannot be lowered (on endpoints with reasoning it is the thinking budget),
/// so look at the tail: if the same segment (at least 30 bytes) appears five times in a row, byte for byte, it is not answering the question.
/// Real statement lines differ from one another. Comparing by bytes, it does not matter where multi-byte characters are split.
///
/// Only the **shortest period** is counted, and the tail is still at least 30 bytes. Each statement opens with a quoted original text, and a short-period
/// string over 150 bytes can still split into 30+ byte segments: a leader line of 200 dots in a table of contents, or twenty empty
/// `<td></td>` in a row. Without this rule it could be cut off as repetition (depending on where the per-256-byte check falls), and everything after the quoted
/// text would be lost. Real repetition is a whole line repeating. Lines shorter than 30 bytes are not cut when they repeat here (of 57 truncations one night,
/// 4 were of this kind). It is better to let it run to the cap than to cut a quotation
fn repeats_at_tail(text: &[u8]) -> bool {
    const REPEATS: usize = 5;
    (30..=600.min(text.len() / REPEATS)).any(|span| {
        let tail = &text[text.len() - span * REPEATS..];
        tail.chunks(span).all(|piece| piece == &tail[..span])
            && !(1..30).any(|shift| tail[shift..] == tail[..tail.len() - shift])
    })
}

impl LlmClient {
    pub fn new(base_url: &str, api_key: Option<&str>, model: &str) -> Self {
        Self::with_timeouts(base_url, api_key, model, CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// `reasoning_effort`. Reasoning models think while answering by default. For extraction about 90% of a call's tokens are
    /// thinking; `minimal` zeroes that without changing the answer (bench README, 2026-09-24). Some endpoints cannot turn off `minimal`,
    /// and need `none` (DeepSeek). Empty means no field is sent.
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.filter(|e| !e.trim().is_empty());
        self
    }

    fn with_effort(&self, mut body: serde_json::Value) -> serde_json::Value {
        if let Some(e) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(e);
        }
        body
    }

    /// The timeout is injectable, only so it can be **tested in reasonable time**. Production uses [`LlmClient::new`].
    /// Testing a hang with 300 seconds would take five minutes, and nobody would keep a test like that.
    pub fn with_timeouts(
        base_url: &str,
        api_key: Option<&str>,
        model: &str,
        connect: Duration,
        read: Duration,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(connect)
                .read_timeout(read)
                .build()
                // Only fails when the TLS backend cannot start. In that case falling back to the default client is pointless,
                // but the whole process should not crash here either
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, "failed to build the HTTP client, falling back to the default client without a timeout");
                    reqwest::Client::new()
                }),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(String::from),
            model: model.to_string(),
            reasoning_effort: None,
            context_window: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX)),
            tools_without_effort: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            completion_ceiling: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
                MAX_COMPLETION_TOKENS,
            )),
            use_max_completion_tokens: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            suppress_temperature: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Character budget for history. Not a tokenizer estimate: when the endpoint has reported a window, take **half** of the window as a character count. Chinese is roughly
    /// one character per token, so using the token count as-is overruns on small windows (#973 review). The other half is left for the prompt, tools and answer.
    /// It is then clamped between `MIN_HISTORY_CHARS` and `DEFAULT_HISTORY_CHARS`. An estimate that is off
    /// is caught by the once-per-turn recovery
    pub fn history_char_budget(&self) -> usize {
        let window = self
            .context_window
            .load(std::sync::atomic::Ordering::Relaxed);
        ((window / 2) as usize).clamp(MIN_HISTORY_CHARS, DEFAULT_HISTORY_CHARS)
    }

    /// When the endpoint states its window in a rejection, remember it. It only goes down.
    pub fn remember_context_window(&self, err: &anyhow::Error) {
        if let Some(window) = context_too_long(err).and_then(|e| e.window) {
            self.context_window
                .fetch_min(window, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.post(format!("{}{path}", self.base_url));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req
    }

    /// Non-streaming conversation (lightweight cases such as connectivity tests). No temperature is passed, so the request body is unchanged from before and the endpoint uses its default
    pub async fn chat(&self, messages: &[ChatMessage]) -> anyhow::Result<String> {
        self.chat_at(messages, None).await
    }

    /// Non-streaming conversation with a specified sampling temperature. Extraction, a task of "copying the source verbatim", needs 0. The endpoint default is 1.0, and the same text
    /// asked twice gave 6 statements one time and 19 the other (measured #729), so density was pure luck
    pub async fn chat_at(
        &self,
        messages: &[ChatMessage],
        temperature: Option<f32>,
    ) -> anyhow::Result<String> {
        use std::sync::atomic::Ordering;
        let mut body =
            self.with_effort(json!({ "model": self.model, "messages": messages, "stream": false }));
        let mut dropped_temperature = self.suppress_temperature.load(Ordering::Relaxed);
        if let Some(t) = temperature.filter(|_| !dropped_temperature) {
            body["temperature"] = json!(t);
        }
        let resp = loop {
            let resp = self
                .request("/chat/completions")
                .json(&body)
                .send()
                .await
                .map_err(Unreachable)?;
            let status = resp.status();
            let retry_after = retry_after_of(resp.headers());
            if status.is_success() {
                break resp;
            }
            let raw = resp.text().await.map_err(Unreachable)?;
            let parsed = serde_json::from_str(&raw).unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST
                && !dropped_temperature
                && temperature.is_some()
                && says_temperature_unsupported(&parsed, &raw)
            {
                self.suppress_temperature.store(true, Ordering::Relaxed);
                body.as_object_mut().unwrap().remove("temperature");
                dropped_temperature = true;
                continue;
            }
            return Err(failure("LLM", status, retry_after, &parsed, &raw));
        };
        let body: serde_json::Value = resp.json().await.map_err(Unreachable)?;
        log_usage(&self.model, &call_label(messages), &body);
        body["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| strip_reasoning(s).to_string())
            .ok_or_else(|| anyhow::anyhow!("Unexpected LLM response shape: {body}"))
    }

    /// One Q&A, **streamed internally but returned whole**: the caller gets one string, the same as [`Self::chat_at`].
    /// The only difference is how the bytes arrive.
    ///
    /// **Why the long-prompt paths use this:** [`READ_TIMEOUT`] measures "how long without new bytes", while
    /// a non-streaming call's first byte waits for the model to generate the whole reply, so all of the model's thinking time counts as silence.
    /// With reasoning on, a dense body chunk measured first byte at 227 seconds, occasionally past 300 seconds, and was killed. Under streaming the same chunk got
    /// bytes at 2.6 seconds (the thinking is in the stream), and the total duration was still about 230 seconds. Streaming is not faster. It restores
    /// "silence" to its real meaning, so the timeout only kills requests that are truly stuck.
    ///
    /// The thinking process is not in the return value: only `delta.content` is collected. Reasoning deltas (`reasoning` /
    /// `reasoning_content`) are never read. The kind with `<think>` mixed into content is still cut by
    /// [`strip_reasoning`].
    pub async fn chat_at_streaming(
        &self,
        messages: &[ChatMessage],
        temperature: Option<f32>,
    ) -> anyhow::Result<Reply> {
        use std::sync::atomic::Ordering;
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            // Usage arrives with the last frame. The first thing to compare between two runs is completion tokens,
            // so switching to streaming must not lose that number
            "stream_options": { "include_usage": true },
        });
        let mut body = self.with_effort(body);
        let mut dropped_temperature = self.suppress_temperature.load(Ordering::Relaxed);
        if let Some(t) = temperature.filter(|_| !dropped_temperature) {
            body["temperature"] = json!(t);
        }
        // One retry per call, and only downwards: an endpoint that refuses our ceiling has
        // told us its own, and sending nothing instead would hand the cut-off point back
        // to the endpoint default this field exists to displace (#760). The lowered
        // ceiling is kept on the client, so the next call starts from it instead of
        // paying the 400 again (extraction calls once per chunk).
        let mut ceiling = self.completion_ceiling.load(Ordering::Relaxed);
        let mut lowered = false;
        // Each adjustment is tried only once per call: the gateway may re-add a parameter we already removed (#1232).
        let mut renamed = self.use_max_completion_tokens.load(Ordering::Relaxed);
        let resp = loop {
            let limit_key = if renamed {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            // The cap is ours, not the endpoint default ([`MAX_COMPLETION_TOKENS`]); renaming the field must not drop it.
            body[limit_key] = json!(ceiling);
            let resp = self
                .request("/chat/completions")
                .json(&body)
                .send()
                .await
                .map_err(Unreachable)?;
            let status = resp.status();
            let retry_after = retry_after_of(resp.headers());
            if status.is_success() {
                break resp;
            }
            let raw = resp.text().await.map_err(Unreachable)?;
            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST
                && !renamed
                && parsed["error"]["param"] == "max_tokens"
                && parsed["error"]["code"] == "unsupported_parameter"
            {
                self.use_max_completion_tokens
                    .store(true, Ordering::Relaxed);
                body.as_object_mut().unwrap().remove("max_tokens");
                renamed = true;
                continue;
            }
            if status == reqwest::StatusCode::BAD_REQUEST
                && !dropped_temperature
                && temperature.is_some()
                && says_temperature_unsupported(&parsed, &raw)
            {
                self.suppress_temperature.store(true, Ordering::Relaxed);
                body.as_object_mut().unwrap().remove("temperature");
                dropped_temperature = true;
                continue;
            }
            if status == reqwest::StatusCode::BAD_REQUEST
                && !lowered
                && !says_context_too_long(&parsed, &err_detail(&parsed, &raw))
            {
                if let Some(lower) = stated_completion_ceiling(&err_detail(&parsed, &raw), ceiling)
                {
                    tracing::info!(model = %self.model, sent = ceiling, stated = lower, "endpoint stated its own completion ceiling, retrying with it and remembering it");
                    self.completion_ceiling.fetch_min(lower, Ordering::Relaxed);
                    ceiling = lower;
                    lowered = true;
                    continue;
                }
            }
            return Err(failure("LLM", status, retry_after, &parsed, &raw));
        };
        let mut bytes = resp.bytes_stream();
        let (mut buf, mut answer) = (SseBuffer::default(), String::new());
        let (mut saw_frame, mut ended) = (false, false);
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;
        let (mut checked, mut repeating) = (0, false);
        'read: while let Some(part) = bytes.next().await {
            let part = part.map_err(Unreachable)?;
            // Network chunks can break in the middle of a UTF-8 character, so wait until the full SSE frame has arrived before decoding.
            buf.push(&part);
            // SSE frames are separated by blank lines; the last incomplete frame stays in buf waiting for the next chunk
            while let Some(frame) = buf.next_frame() {
                self.take_frame(
                    &frame,
                    &mut answer,
                    &mut saw_frame,
                    &mut ended,
                    &mut finish_reason,
                    &mut usage,
                );
                // Once it repeats, stop reading. The reply is recorded as `repeating`: extraction treats it as partial, and the complete statements before it are still kept,
                // and it does not wait for it to reach 65,536 tokens. Dropping the response stream closes the connection, so the endpoint stops too. Check by frame,
                // not by network chunk: one chunk may hold a whole repetition. Check once per additional 256 bytes
                if !ended && answer.len() >= checked + 256 {
                    checked = answer.len();
                    if repeats_at_tail(answer.as_bytes()) {
                        tracing::warn!(
                            model = %self.model,
                            call = %call_label(messages),
                            chars = answer.len(),
                            "回复在复读，按截断收下"
                        );
                        repeating = true;
                        ended = true;
                        buf = SseBuffer::default();
                        break 'read;
                    }
                }
            }
        }
        // **The closing frame also counts**: some implementations omit the trailing blank line, and dropping it would drop the tail of the answer
        let rest = String::from_utf8_lossy(&buf.bytes);
        if !rest.trim().is_empty() {
            self.take_frame(
                &rest,
                &mut answer,
                &mut saw_frame,
                &mut ended,
                &mut finish_reason,
                &mut usage,
            );
        }
        if !saw_frame {
            anyhow::bail!("LLM stream carried no frames");
        }
        if let Some(u) = usage {
            tracing::info!(
                model = %self.model,
                prompt = u.prompt_tokens,
                completion = u.completion_tokens,
                call = %call_label(messages),
                "llm usage"
            );
        }
        // The endpoint opened and then dropped halfway. A half-finished reply looks like success, and unless it is made an error it will be taken as
        // the model's complete answer
        if !ended {
            return Err(anyhow::Error::new(Interrupted {
                got: answer.chars().count(),
            }));
        }
        Ok(Reply {
            text: strip_reasoning(&answer).to_string(),
            finish_reason,
            usage,
            repeating,
        })
    }

    /// One SSE frame: take the content delta, recognize the termination signal, and record usage on the way. Reasoning deltas are never read.
    fn take_frame(
        &self,
        frame: &str,
        answer: &mut String,
        saw_frame: &mut bool,
        ended: &mut bool,
        finish_reason: &mut Option<String>,
        usage: &mut Option<Usage>,
    ) {
        for line in frame.lines() {
            let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if data == "[DONE]" {
                *ended = true;
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            *saw_frame = true;
            if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                answer.push_str(delta);
            }
            // The model finished on its own: a normal close (stop) or hitting its output limit (length). Both mean
            // the endpoint completed the reply, unlike "the stream dropped halfway".
            //
            // Which one must be kept: both set `ended` to true, but a `length` reply is partial,
            // and if not carried out it leaves only "could not parse", without saying why it is incomplete (#760)
            if let Some(reason) = v["choices"][0]["finish_reason"].as_str() {
                *ended = true;
                *finish_reason = Some(reason.to_string());
            }
            // Usage appears only in the last frame (choices empty)
            // Some gateways attach cumulative usage to every frame. Here we only record it and log once when the stream ends, otherwise one call
            // becomes hundreds of "usage" lines in the log, and someone summing by line overstates tokens by hundreds of times
            if !v["usage"].is_null() {
                let u = &v["usage"];
                *usage = Some(Usage {
                    prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                    completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                });
            }
        }
    }

    /// Tool conversation (non-streaming): messages are raw OpenAI-protocol JSON
    /// (supports assistant.tool_calls and role=tool turns); tools is an array of function definitions.
    pub async fn chat_tools(
        &self,
        messages: &[serde_json::Value],
        tools: &serde_json::Value,
    ) -> anyhow::Result<AssistantTurn> {
        self.chat_tools_with(messages, Some(tools), None).await
    }

    /// Request body with tools. `tools` None means no tools field is sent, **not an empty array**:
    /// some endpoints return 400 on `"tools": []`. `tool_choice` is passed through as-is per the OpenAI protocol
    /// (`"auto"` / `"none"` / `"required"` / `{"type":"function",...}`),
    /// and is only written when given.
    fn tools_body(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        tool_choice: Option<&serde_json::Value>,
        stream: bool,
    ) -> serde_json::Value {
        let mut body = self.with_effort(json!({
            "model": self.model,
            "messages": messages,
            "stream": stream,
        }));
        if let Some(tools) = tools {
            body["tools"] = tools.clone();
            if let Some(choice) = tool_choice {
                body["tool_choice"] = choice.clone();
            }
            if self
                .tools_without_effort
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                body["reasoning_effort"] = json!("none");
            }
        }
        body
    }

    /// Sends a request with tools. On reasoning deployments an endpoint may reject the whole body that carries tools plus a reasoning effort
    /// (Azure: `Function tools with reasoning_effort are not supported`), and the deployment's own default effort counts too, so not sending the field is rejected as well.
    /// The conversation then falls back to one-step retrieval, so governance's second look cannot run (#1353).
    /// On hitting that 400, record it on the client and retry once with `none`. Only tool-carrying requests are affected; extraction and alignment carry on as before
    async fn post_tools(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        tool_choice: Option<&serde_json::Value>,
        stream: bool,
    ) -> anyhow::Result<reqwest::Response> {
        use std::sync::atomic::Ordering;
        let mut retried = false;
        loop {
            let learned = self.tools_without_effort.load(Ordering::Relaxed);
            let resp = self
                .request("/chat/completions")
                .json(&self.tools_body(messages, tools, tool_choice, stream))
                .send()
                .await
                .map_err(Unreachable)?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            let retry_after = retry_after_of(resp.headers());
            let raw = resp.text().await.map_err(Unreachable)?;
            let parsed = serde_json::from_str(&raw).unwrap_or_default();
            if status == reqwest::StatusCode::BAD_REQUEST
                && tools.is_some()
                && !learned
                && says_tools_refuse_effort(&err_detail(&parsed, &raw))
            {
                tracing::info!(model = %self.model, "endpoint does not accept reasoning effort with tools, sending none for tool requests and remembering it");
                self.tools_without_effort.store(true, Ordering::Relaxed);
                retried = true;
                continue;
            }
            // Retrying with none is also rejected, and the error is still about reasoning effort. This endpoint does not accept even none; remembering it would only make every later question carry a wrong value.
            // Other 400s (context too long, bad tool definitions) have nothing to do with none, so the remembered value stays.
            if retried
                && status == reqwest::StatusCode::BAD_REQUEST
                && err_detail(&parsed, &raw)
                    .to_lowercase()
                    .contains("reasoning_effort")
            {
                self.tools_without_effort.store(false, Ordering::Relaxed);
            }
            return Err(failure("LLM", status, retry_after, &parsed, &raw));
        }
    }

    /// Exact serialized streaming request size, including model and protocol fields.
    /// Used by the bounded answer phase before any network I/O.
    pub fn tool_free_request_bytes(&self, messages: &[serde_json::Value]) -> usize {
        self.tools_body(messages, None, None, true)
            .to_string()
            .len()
    }

    /// Tool conversation (non-streaming), with the tool list and `tool_choice` both optional.
    pub async fn chat_tools_with(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        tool_choice: Option<&serde_json::Value>,
    ) -> anyhow::Result<AssistantTurn> {
        let resp = self.post_tools(messages, tools, tool_choice, false).await?;
        let body: serde_json::Value = resp.json().await.map_err(Unreachable)?;
        let msg = &body["choices"][0]["message"];
        if msg.is_null() {
            anyhow::bail!("Unexpected LLM response shape: {body}");
        }
        let content = msg["content"]
            .as_str()
            .map(String::from)
            .filter(|s| !s.is_empty());
        let tool_calls = msg["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|c| {
                        Some(ToolCall {
                            id: c["id"].as_str()?.to_string(),
                            name: c["function"]["name"].as_str()?.to_string(),
                            arguments: c["function"]["arguments"]
                                .as_str()
                                .unwrap_or("{}")
                                .to_string(),
                            extra_content: extra_content_of(c),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(AssistantTurn {
            content,
            reasoning_content: msg["reasoning_content"].as_str().map(String::from),
            tool_calls,
            finish_reason: body["choices"][0]["finish_reason"]
                .as_str()
                .map(String::from),
        })
    }

    /// Tool conversation (streaming): text deltas are produced immediately, and tool calls are merged by the
    /// index fragments of the OpenAI protocol (id/name arrive in the first frame, arguments continue frame by frame). The complete turn is given at stream end.
    pub async fn chat_tools_stream(
        &self,
        messages: &[serde_json::Value],
        tools: &serde_json::Value,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<ToolStreamItem>> + Send + use<>> {
        self.chat_tools_stream_with(messages, Some(tools), None)
            .await
    }

    /// Tool conversation (streaming), with the tool list and `tool_choice` both optional; see [`Self::chat_tools_stream`].
    pub async fn chat_tools_stream_with(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        tool_choice: Option<&serde_json::Value>,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<ToolStreamItem>> + Send + use<>> {
        let resp = self.post_tools(messages, tools, tool_choice, true).await?;

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            let mut buf = SseBuffer::default();
            let mut content = String::new();
            let mut reasoning_content: Option<String> = None;
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut finish_reason = None;
            let mut done = false;
            'outer: while let Some(part) = bytes.next().await {
                let part = part?;
                // Network chunks can break in the middle of a UTF-8 character, so wait until the full SSE frame has arrived before decoding.
                buf.push(&part);
                while let Some(frame) = buf.next_frame() {
                    for line in frame.lines() {
                        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                            continue;
                        };
                        if data == "[DONE]" {
                            done = true;
                            break 'outer;
                        }
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                            continue;
                        };
                        if let Some(reason) = v["choices"][0]["finish_reason"].as_str() {
                            finish_reason = Some(reason.to_string());
                            done = true;
                        }
                        let delta = &v["choices"][0]["delta"];
                        if let Some(text) = delta["reasoning_content"].as_str() {
                            reasoning_content.get_or_insert_default().push_str(text);
                        }
                        if let Some(text) = delta["content"].as_str() {
                            if !text.is_empty() {
                                content.push_str(text);
                                yield ToolStreamItem::Delta(text.to_string());
                            }
                        }
                        if let Some(tcs) = delta["tool_calls"].as_array() {
                            for tc in tcs {
                                let idx = tool_call_slot(tc, calls.len());
                                while calls.len() <= idx {
                                    calls.push(ToolCall {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: String::new(),
                                        extra_content: None,
                                    });
                                }
                                let slot = &mut calls[idx];
                                if let Some(extra) = extra_content_of(tc) {
                                    slot.extra_content = Some(extra);
                                }
                                if let Some(id) = tc["id"].as_str() {
                                    slot.id.push_str(id);
                                }
                                if let Some(n) = tc["function"]["name"].as_str() {
                                    slot.name.push_str(n);
                                }
                                if let Some(a) = tc["function"]["arguments"].as_str() {
                                    slot.arguments.push_str(a);
                                }
                            }
                        }
                    }
                }
            }
            // Even when HTTP ends normally, only half a model turn may have arrived. Do not hand an unfinished tool call
            // to the agent as a complete turn.
            if !done {
                Err(Interrupted { got: content.chars().count() })?;
            }
            calls.retain(|c| !c.name.is_empty());
            let content = if content.is_empty() { None } else { Some(content) };
            yield ToolStreamItem::Turn(AssistantTurn { content, reasoning_content, tool_calls: calls, finish_reason });
        };
        Ok(stream)
    }

    /// Streaming conversation: yields incremental text fragments.
    pub async fn chat_stream(
        &self,
        messages: &[ChatMessage],
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send + use<>> {
        let messages = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or_default())
            .collect::<Vec<_>>();
        self.chat_stream_raw(&messages).await
    }

    /// Streaming conversation (raw JSON messages, can carry tool-turn context).
    pub async fn chat_stream_raw(
        &self,
        messages: &[serde_json::Value],
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<String>> + Send + use<>> {
        let resp =
            self.request("/chat/completions")
                .json(&self.with_effort(
                    json!({ "model": self.model, "messages": messages, "stream": true }),
                ))
                .send()
                .await
                .map_err(Unreachable)?;
        if !resp.status().is_success() {
            let status = resp.status();
            let retry_after = retry_after_of(resp.headers());
            return Err(response_failure("LLM", status, retry_after, resp).await?);
        }

        let mut bytes = resp.bytes_stream();
        let stream = async_stream::try_stream! {
            let mut buf = SseBuffer::default();
            let mut ended = false;
            let mut got = 0;
            while let Some(part) = bytes.next().await {
                let part = part?;
                // Network chunks can break in the middle of a UTF-8 character, so wait until the full SSE frame has arrived before decoding.
                buf.push(&part);
                // SSE frames are separated by blank lines; take out the parts that have fully arrived, frame by frame
                while let Some(frame) = buf.next_frame() {
                    for line in frame.lines() {
                        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                            continue;
                        };
                        if data == "[DONE]" {
                            return;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                            if v["choices"][0]["finish_reason"].is_string() {
                                ended = true;
                            }
                            if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                                if !delta.is_empty() {
                                    got += delta.chars().count();
                                    yield delta.to_string();
                                }
                            }
                        }
                    }
                }
            }
            if !ended {
                Err(Interrupted { got })?;
            }
        };
        Ok(stream)
    }

    /// Batch embedding.
    pub async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let resp = self
            .request("/embeddings")
            .json(&json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .map_err(Unreachable)?;
        let status = resp.status();
        let retry_after = retry_after_of(resp.headers());
        if !status.is_success() {
            return Err(response_failure("Embedding", status, retry_after, resp).await?);
        }
        let body: serde_json::Value = resp.json().await.map_err(Unreachable)?;
        let data = body["data"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Unexpected embedding response shape"))?;
        let items = ordered_by_index(data, texts.len())?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let v = item["embedding"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("embedding response is missing the vectors"))?
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            out.push(v);
        }
        Ok(out)
    }
}

/// Pairs the returned vectors with the input texts. An entry that writes a numeric `index` is attributed to that input.
/// An entry without a usable number (absent, `null`, string, float, negative) is matched by its arrival position. Gateways that scramble
/// the order are still recognized, and endpoints that omit the index still work.
///
/// **Both shapes can be mixed in one batch.** Gemini's OpenAI-compatible endpoint omits `index` only on the first item.
/// (Zero is its default, and defaults aren't written.) Three inputs come back as `[{embedding}, {index: 1},
/// {index: 2}]`. Mixing is not a contradiction: each item either says who it is, or stands in its own position. A real
/// contradiction is two items claiming the same input, an index outside this batch, or a count that does not match the inputs. That means the endpoint is lying,
/// so reject it rather than guess.
fn ordered_by_index(
    data: &[serde_json::Value],
    expected: usize,
) -> anyhow::Result<Vec<&serde_json::Value>> {
    anyhow::ensure!(
        data.len() == expected,
        "Embedding response has {} items for {expected} inputs",
        data.len()
    );
    let mut ordered = vec![None; expected];
    for (position, item) in data.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(position);
        let slot = ordered
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("Embedding response index {index} is out of range"))?;
        anyhow::ensure!(
            slot.is_none(),
            "Embedding response has a duplicate index {index}"
        );
        *slot = Some(item);
    }
    Ok(ordered.into_iter().flatten().collect())
}

/// Records a call's token overhead. **The cache-hit count is the most important column here**: extraction relies on
/// "the system message is byte-identical across chunks of one document" to get the provider's prefix cache.
/// Putting per-chunk-varying content into the system message silently zeroes it, and only this number makes that visible.
///
/// Field names differ by vendor: OpenAI uses prompt_tokens_details.cached_tokens,
/// while DeepSeek uses prompt_cache_hit_tokens. Both are read, whichever is present.
/// What this call is for: the first sentence of the system message, cut at sixty characters.
///
/// The usage log used to carry only the model and two numbers, so you could not tell which kind of call (extraction, two-vote, rule extraction,
/// reading, time interpretation) a document's 30k tokens went to. System messages are constants hard-coded in each place, and their first sentences
/// differ, so using them as a label needs no change to any caller. Calls without a system message (conversation) are labelled `-`
fn call_label(messages: &[ChatMessage]) -> String {
    let Some(system) = messages.iter().find(|m| m.role == "system") else {
        return "-".into();
    };
    let first = system
        .content
        .split(['.', '。', '\n'])
        .next()
        .unwrap_or("")
        .trim();
    // No spaces or quotes in the label: the log is one `key=value` line, and whoever splits it splits on spaces
    first
        .chars()
        .take(60)
        .map(|c| {
            if c.is_whitespace() || c == '"' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn log_usage(model: &str, call: &str, body: &serde_json::Value) {
    let u = &body["usage"];
    if u.is_null() {
        return;
    }
    let n = |k: &str| u[k].as_u64();
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| n("prompt_cache_hit_tokens"));
    tracing::info!(
        model,
        prompt = n("prompt_tokens"),
        completion = n("completion_tokens"),
        cached,
        // Placed later: the scripts that read the log expect the line in the order `model=… prompt=… completion=…`
        call,
        "llm usage"
    );
}

/// Maximum characters of quoted original text. No diagnostic seen so far has exceeded it, and this text goes into logs,
/// alerts, and the document's error column. A proxy returning an HTML page should not dump the whole page in. Counted in characters, not bytes,
/// so a Chinese error message cut mid-character would become garbage.
const MAX_ERROR_DETAIL_CHARS: usize = 500;

/// Pulls a human-readable sentence from the error body. It tries, in order, OpenAI's `error.message`, SiliconFlow's
/// top-level `message`, and the form where `error` is directly a string. If none match, it quotes the raw body itself.
/// "unknown error" is reserved for when the body is truly empty. It used to be the destination of every unrecognized shape,
/// swallowing the most useful sentence (#527).
fn err_detail(body: &serde_json::Value, raw: &str) -> String {
    body["error"]["message"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .or_else(|| body["error"].as_str())
        .map(String::from)
        .or_else(|| {
            let raw = raw.trim();
            (!raw.is_empty()).then(|| raw.chars().take(MAX_ERROR_DETAIL_CHARS).collect())
        })
        .unwrap_or_else(|| "unknown error".to_string())
}

/// Check parameter names and error codes first; Azure may give only a message. A vague "invalid temperature" must not drop the caller's setting.
fn says_temperature_unsupported(body: &serde_json::Value, raw: &str) -> bool {
    if let Some(parameter) = body["error"]["param"].as_str() {
        if parameter != "temperature" {
            return false;
        }
        if matches!(
            body["error"]["code"].as_str(),
            Some("unsupported_parameter" | "unsupported_value")
        ) {
            return true;
        }
    }
    let detail = err_detail(body, raw).to_lowercase();
    detail.contains("temperature")
        && (detail.contains("not support") || detail.contains("unsupported"))
}

/// Whether the endpoint says "requests with tools do not accept a reasoning effort". Only the wording is recognizable: the code Azure gives is the vague
/// `invalid_request_error`. Both must be mentioned. A 400 that mentions only tools or only reasoning effort is a different fault.
fn says_tools_refuse_effort(detail: &str) -> bool {
    let d = detail.to_lowercase();
    d.contains("reasoning_effort") && (d.contains("tool") || d.contains("function"))
}

/// Whether the response body says this is a balance problem.
///
/// **Used for 429**: OpenAI uses the same status code for "too fast" and "out of money".
/// `insufficient_quota` in `error.code` or `error.type` is the dividing line.
///
/// Only this one identifier is recognized, not free-text matching on the message: wording changes and gets localized,
/// while `code` is part of the interface contract. When it is not recognized, fall back to classifying by status code, which is the safe side
/// (backing off as rate limiting a few times is better than giving up gently as out-of-credit).
fn says_out_of_credit(body: &serde_json::Value) -> bool {
    ["code", "type"]
        .iter()
        .filter_map(|k| body["error"][k].as_str())
        .any(|v| v == "insufficient_quota")
}

/// Whether the response body says the context is exceeded.
///
/// **Check the structured code first, then the wording.** OpenAI's code is `context_length_exceeded`, and the wording changes with
/// versions. DeepSeek's code is only the vague `invalid_request_error`, so it is recognized by the sentence
/// "This model's maximum context length is 1048576 tokens. However, you requested …"
/// (measured 2026-09-27). The code is read in `err_detail` before only the message is kept, or it is lost.
fn says_context_too_long(body: &serde_json::Value, detail: &str) -> bool {
    let says = [
        body.pointer("/error/code"),
        body.pointer("/error/type"),
        body.get("code"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| v.as_str())
    .any(|code| code == "context_length_exceeded")
        || context_wording(detail);
    // The accounting says messages fit and what overflowed is the completion we asked for. That is not a conversation that is too long: the cap has to be taken from
    // the window's remainder (`stated_completion_ceiling`), and a retry gets through (follow-up to #973 review)
    says && completion_overflows(detail).is_none()
}

/// The accounting in the rejection: window W, the messages we sent take N, and the completion asked for M. OpenAI, DeepSeek and
/// vLLM write it this way: "maximum context length is W tokens. However, you requested T tokens
/// (N in the messages, M in the completion)". If it cannot be read in full, it is None, since other wordings cannot tell which part overflowed.
fn overflow_ledger(detail: &str) -> Option<(u32, u32, u32)> {
    let lower = detail.to_ascii_lowercase();
    let number_before = |marker: &str| -> Option<u32> {
        let (before, _) = lower.split_once(marker)?;
        let word = before
            .trim_end()
            .rsplit(|c: char| c.is_whitespace() || c == '(')
            .next()?;
        number_with_separators(word)
    };
    Some((
        stated_context_window(detail)?,
        number_before(" in the messages")?,
        number_before(" in the completion")?,
    ))
}

/// When the completion overflowed: the room the window leaves for the answer. If the messages alone do not fit (the room is under
/// `MIN_CONTEXT_WINDOW_TOKENS`, too little for a reasonable answer), it is None, and that is the conversation being too long.
/// Previously this kind of rejection retried with the window itself as the cap. N + W still overflows, so the retry was bound to hit the same error again.
fn completion_overflows(detail: &str) -> Option<u32> {
    let (window, messages, completion) = overflow_ledger(detail)?;
    let room = window.checked_sub(messages)?;
    (room >= MIN_CONTEXT_WINDOW_TOKENS && room < completion).then_some(room)
}

/// Ways each provider says "too long". Only ones measured or documented are taken: OpenAI and DeepSeek's "maximum context
/// length", Anthropic's "prompt is too long", LM Studio's "exceeds the available context"
fn context_wording(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "maximum context length",
        "context window",
        "too many tokens",
        "prompt is too long",
        "exceeds the available context",
    ]
    .iter()
    .any(|wording| detail.contains(wording))
}

/// How large the window reported in a rejection is (tokens).
///
/// **Only the number directly after "window" is accepted.** The same sentence also has the tokens we sent, the model version number,
/// and the completion cap. Learning the wrong one would cut the history away. The number must also be followed by the word tokens
fn stated_context_window(detail: &str) -> Option<u32> {
    let lower = detail.to_ascii_lowercase();
    for marker in ["maximum context length", "context window"] {
        let Some((_, rest)) = lower.split_once(marker) else {
            continue;
        };
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '='));
        let rest = rest
            .strip_prefix("is ")
            .or_else(|| rest.strip_prefix("of "))
            .unwrap_or(rest);
        let mut words = rest.split_whitespace();
        let Some(number) = words.next().and_then(number_with_separators) else {
            continue;
        };
        if number >= MIN_CONTEXT_WINDOW_TOKENS
            && words
                .next()
                .is_some_and(|s| s.trim_end_matches(['.', ',', ';', ':']) == "tokens")
        {
            return Some(number);
        }
    }
    None
}

/// The ceiling the endpoint says it has, read out of the 400 it refused us with (#891).
///
/// `MAX_COMPLETION_TOKENS` is deliberately above the largest completion this
/// path has ever measured, so that it can never become a reasoning cap. A model
/// whose own completion limit sits below that number rejects the request
/// outright instead of clamping it, and every chunk fails: gpt-4o-mini caps at
/// 16,384 and answers `max_tokens is too large: 65536`.
///
/// Reading the number back is free-text matching, which `says_out_of_credit`
/// deliberately avoids, and the reason it is acceptable here is that there is no
/// structured carrier for the limit and the failure is one-directional: a
/// message that parses to `None` leaves the caller with exactly the error it
/// gets today, and a number can only ever lower a ceiling, never raise one.
///
/// The wording differs per vendor, so the rule is about shape, not phrase: a
/// candidate is a number the message itself calls tokens (`16384 completion
/// tokens`, `32768 tokens`), written with or without thousands separators. Numbers
/// the message does not call tokens are not limits: the `2024` in
/// `gpt-4o-mini-2024-07-18`, a status code, a request id. Of the candidates,
/// those at or above what we sent are the echo of our own request ("whereas you
/// provided 65536"); the largest of the rest is taken, and nothing below 1,024 is,
/// because no completion limit is that small and a stray small number would turn a
/// refusal into a silently cut reply. A context-window refusal belongs to history
/// recovery; treating its window as an output ceiling repeats the oversized prompt.
/// The one exception is a refusal whose own ledger says the messages fit and the
/// completion did not: the ceiling is then what the window leaves for the answer
/// (`completion_overflows`), and the retry at that number goes through.
fn stated_completion_ceiling(detail: &str, sent: u32) -> Option<u32> {
    if let Some(room) = completion_overflows(detail) {
        return (room < sent).then_some(room);
    }
    if context_wording(detail)
        || (!detail.contains("max_tokens") && !detail.contains("max_completion_tokens"))
    {
        return None;
    }
    let words: Vec<&str> = detail
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '\''))
        .filter(|w| !w.is_empty())
        .collect();
    let mut best = None;
    for (i, word) in words.iter().enumerate() {
        let Some(n) = number_with_separators(word) else {
            continue;
        };
        // A modifier (completion / output / new) may sit between the number and tokens
        let says_tokens = words[i + 1..].iter().take(2).any(|w| {
            w.trim_matches(|c: char| !c.is_ascii_alphabetic())
                .eq_ignore_ascii_case("tokens")
                || w.trim_matches(|c: char| !c.is_ascii_alphabetic())
                    .eq_ignore_ascii_case("token")
        });
        if says_tokens && n >= 1024 && n < sent {
            best = Some(best.map_or(n, |b: u32| b.max(n)));
        }
    }
    best
}

/// `16384`, `16,384`, `16384.`: an integer after removing thousands commas and trailing punctuation. Anything else is not a number
fn number_with_separators(word: &str) -> Option<u32> {
    let trimmed = word.trim_end_matches(['.', ',', ';', ':']);
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit() || c == ',') {
        return None;
    }
    if !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    trimmed.replace(',', "").parse().ok()
}

#[cfg(test)]
mod tests {
    /// The accounting says the messages fit and the overflow is the completion. That is not a conversation that is too long: retry with the room the window leaves.
    /// If the messages themselves do not fit (or the room is under 1,024), it is a context rejection
    #[test]
    fn a_completion_overflow_is_a_ceiling_to_lower_and_not_a_context_refusal() {
        let fits = "This model's maximum context length is 8192 tokens. However, you requested 69536 tokens (4000 in the messages, 65536 in the completion). Please reduce the length of the messages or completion.";
        for body in [
            json!({"error":{"message":fits,"type":"invalid_request_error","code":"invalid_request_error"}}),
            json!({"error":{"message":fits,"code":"context_length_exceeded"}}),
        ] {
            let err = failure("LLM", reqwest::StatusCode::BAD_REQUEST, None, &body, "");
            assert!(context_too_long(&err).is_none(), "{err:#}");
            assert!(rejected(&err).is_some(), "{err:#}");
        }
        assert_eq!(
            stated_completion_ceiling(fits, MAX_COMPLETION_TOKENS),
            Some(4_192)
        );
        assert_eq!(
            stated_completion_ceiling(fits, 4_000),
            None,
            "a ceiling is only ever lowered"
        );
        for raw in [
            "This model's maximum context length is 8192 tokens. However, you requested 74536 tokens (9000 in the messages, 65536 in the completion).",
            "This model's maximum context length is 8192 tokens. However, you requested 73036 tokens (7500 in the messages, 65536 in the completion).",
        ] {
            let err = failure("LLM", reqwest::StatusCode::BAD_REQUEST, None, &json!({}), raw);
            let refusal = context_too_long(&err).expect("the messages do not fit");
            assert_eq!(refusal.window, Some(8192));
            assert_eq!(stated_completion_ceiling(raw, MAX_COMPLETION_TOKENS), None);
        }
    }

    /// The whole path: a small-window model rejected our 65,536 max_tokens. The retry asks for what the window leaves, and the
    #[tokio::test]
    async fn a_completion_overflow_is_retried_at_what_the_window_leaves() {
        let refusal = r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 65736 tokens (200 in the messages, 65536 in the completion). Please reduce the length of the messages or completion.","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#;
        let answer = [
            r#"data: {"choices":[{"delta":{"content":"{\"e\":[]}"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n");
        let (addr, server, requests) = two_http_responses(
            ("400 Bad Request", "application/json", refusal),
            ("200 OK", "text/event-stream", &answer),
        )
        .await;
        let reply = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                Some(0.0),
            )
            .await
            .unwrap();
        server.await.unwrap();
        let sent = requests.await.unwrap();
        assert_eq!(sent.len(), 2, "the refusal is retried, not surfaced");
        assert_eq!(sent_max_tokens(&sent[0]), json!(MAX_COMPLETION_TOKENS));
        assert_eq!(
            sent_max_tokens(&sent[1]),
            json!(7_992),
            "8192 minus the 200 the messages take"
        );
        assert_eq!(reply.text, r#"{"e":[]}"#);
    }

    #[test]
    fn context_refusals_keep_the_code_and_only_learn_a_stated_window() {
        // DeepSeek's exact words (measured 2026-09-27, deepseek-chat): code is only invalid_request_error
        let deepseek = json!({"error":{"message":"This model's maximum context length is 1048576 tokens. However, you requested 1200006 tokens (1200005 in the messages, 1 in the completion). Please reduce the length of the messages or completion. (request_id: 3c27dcba)","type":"invalid_request_error","param":null,"code":"invalid_request_error"}});
        for (body, raw, window, budget) in [
            (json!({"error":{"code":"context_length_exceeded","message":"Request rejected"}}), "", None, 32_000),
            (deepseek, "", Some(1_048_576), 32_000),
            (json!({}), "Maximum context length is 16,384 tokens. You requested 24000 tokens for model v2026.", Some(16_384), 8_192),
            (json!({}), "The context window is 8192 tokens", Some(8192), 4_096),
            (json!({}), "The context window is 3000 tokens", Some(3000), 2_000),
            (json!({}), "maximum context length is 512 tokens", None, 32_000),
            (json!({}), "too many tokens: 123456", None, 32_000),
            (json!({}), "Prompt is too long", None, 32_000),
            (json!({}), "request (105140 tokens) exceeds the available context", None, 32_000),
        ] {
            let err = failure("LLM", reqwest::StatusCode::BAD_REQUEST, None, &body, raw).context("wrapped");
            assert_eq!(context_too_long(&err).expect("context refusal").window, window);
            assert!(rejected(&err).is_none());
            assert!(transient(&err).is_none());
            assert!(
                err.root_cause().to_string().starts_with("LLM request failed (400 Bad Request): the prompt exceeds the model's context window: "),
                "{err:#}"
            );
            let client = LlmClient::new("http://unused", None, "m");
            client.remember_context_window(&err);
            assert_eq!(client.clone().history_char_budget(), budget, "{raw}");
        }
        for raw in [
            "tools are unsupported",
            "max_tokens is too large: 16384 completion tokens",
            "Invalid temperature",
        ] {
            let err = failure(
                "LLM",
                reqwest::StatusCode::BAD_REQUEST,
                None,
                &json!({}),
                raw,
            );
            assert!(context_too_long(&err).is_none());
            assert!(rejected(&err).is_some());
        }
    }

    #[test]
    fn a_400_about_tools_and_reasoning_effort_is_recognised_and_others_are_not() {
        assert!(super::says_tools_refuse_effort(
            "Function tools with reasoning_effort are not supported for gpt-x in /v1/chat/completions. Please use /v1/responses instead."
        ));
        assert!(!super::says_tools_refuse_effort(
            "Unsupported value: 'reasoning_effort' does not support 'minimal'"
        ));
        assert!(!super::says_tools_refuse_effort(
            "Invalid 'tools[0].function.name'"
        ));
    }

    #[tokio::test]
    async fn a_tool_request_refused_for_its_reasoning_effort_retries_with_none_and_remembers() {
        use wiremock::matchers::{body_partial_json, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let ok = serde_json::json!({ "choices": [{ "message": { "role": "assistant", "content": "pong" }, "finish_reason": "stop" }] });
        Mock::given(method("POST"))
            .and(body_partial_json(
                serde_json::json!({ "reasoning_effort": "none" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok.clone()))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::body_string_contains("\"tools\""))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": {
                "message": "Function tools with reasoning_effort are not supported for gpt-x in /v1/chat/completions.",
                "type": "invalid_request_error" } })))
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok))
            .with_priority(3)
            .mount(&server)
            .await;
        let client =
            LlmClient::new(&server.uri(), None, "m").with_reasoning_effort(Some("low".into()));
        let tool = serde_json::json!([{ "type": "function", "function": { "name": "ping", "parameters": { "type": "object", "properties": {} } } }]);
        let ask = [serde_json::json!({ "role": "user", "content": "ping" })];
        assert_eq!(
            client
                .chat_tools(&ask, &tool)
                .await
                .unwrap()
                .content
                .as_deref(),
            Some("pong")
        );
        assert_eq!(
            client
                .clone()
                .chat_tools(&ask, &tool)
                .await
                .unwrap()
                .content
                .as_deref(),
            Some("pong")
        );
        let plain = [ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        client.chat(&plain).await.unwrap();
        let sent: Vec<serde_json::Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let efforts: Vec<_> = sent
            .iter()
            .map(|b| (b.get("tools").is_some(), b["reasoning_effort"].clone()))
            .collect();
        assert_eq!(
            efforts,
            [
                (true, serde_json::json!("low")),  // refused once
                (true, serde_json::json!("none")), // retried
                (true, serde_json::json!("none")), // remembered, also by a clone
                (false, serde_json::json!("low")), // a call without tools keeps the setting
            ]
        );
    }

    #[tokio::test]
    async fn a_deployment_that_refuses_none_too_is_not_sent_none_again() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": {
                "message": "Function tools with reasoning_effort are not supported for gpt-x." } })))
            .mount(&server)
            .await;
        let client =
            LlmClient::new(&server.uri(), None, "m").with_reasoning_effort(Some("low".into()));
        let tool = serde_json::json!([{ "type": "function", "function": { "name": "ping" } }]);
        let ask = [serde_json::json!({ "role": "user", "content": "x" })];
        assert!(client.chat_tools(&ask, &tool).await.is_err());
        let sent: Vec<serde_json::Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(
            sent.iter()
                .map(|b| b["reasoning_effort"].clone())
                .collect::<Vec<_>>(),
            ["low", "none"]
        );
        assert_eq!(
            client.tools_body(&ask, Some(&tool), None, false)["reasoning_effort"],
            "low",
            "the flag is cleared"
        );
    }

    #[tokio::test]
    async fn an_unrelated_400_on_the_retry_keeps_none() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": {
                "message": "Function tools with reasoning_effort are not supported for gpt-x." } })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": {
                "message": "This model's maximum context length is 8192 tokens." } })),
            )
            .mount(&server)
            .await;
        let client =
            LlmClient::new(&server.uri(), None, "m").with_reasoning_effort(Some("low".into()));
        let tool = serde_json::json!([{ "type": "function", "function": { "name": "ping" } }]);
        let ask = [serde_json::json!({ "role": "user", "content": "x" })];
        assert!(client.chat_tools(&ask, &tool).await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert_eq!(
            client.tools_body(&ask, Some(&tool), None, false)["reasoning_effort"],
            "none",
            "the flag is kept"
        );
    }

    #[tokio::test]
    async fn another_400_on_a_tool_request_is_not_retried() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({ "error": { "message": "Invalid 'tools[0].function.name'" } }),
            ))
            .mount(&server)
            .await;
        let client = LlmClient::new(&server.uri(), None, "m");
        let tool = serde_json::json!([{ "type": "function", "function": { "name": "bad name" } }]);
        assert!(client
            .chat_tools(
                &[serde_json::json!({ "role": "user", "content": "x" })],
                &tool
            )
            .await
            .is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn reasoning_effort_rides_in_every_chat_body_only_when_set() {
        let plain = LlmClient::new("http://x", None, "m");
        let body = plain.tools_body(&[], None, None, false);
        assert!(body.get("reasoning_effort").is_none());
        let eager =
            LlmClient::new("http://x", None, "m").with_reasoning_effort(Some("minimal".into()));
        let body = eager.tools_body(&[], None, None, true);
        assert_eq!(body["reasoning_effort"], "minimal");
        let blank = LlmClient::new("http://x", None, "m").with_reasoning_effort(Some("  ".into()));
        assert!(blank
            .tools_body(&[], None, None, false)
            .get("reasoning_effort")
            .is_none());
    }

    use super::*;

    #[test]
    fn a_call_is_labelled_by_the_first_sentence_of_its_system_message() {
        let m = |role: &str, content: &str| ChatMessage {
            role: role.into(),
            content: content.into(),
        };
        assert_eq!(
            call_label(&[
                m(
                    "system",
                    "You read one passage of a document and write down what it states, in the passage's own words. Output one JSON object."
                ),
                m("user", "Passage: ..."),
            ]),
            "You_read_one_passage_of_a_document_and_write_down_what_it_st"
        );
        assert_eq!(call_label(&[m("user", "hello")]), "-");
        assert!(!call_label(&[m("system", "Say \"ok\" now")]).contains('"'));
    }
    use tokio::io::AsyncWriteExt;

    fn client_at(addr: std::net::SocketAddr) -> LlmClient {
        LlmClient::new(&format!("http://{addr}"), None, "m")
    }

    fn no_tools() -> serde_json::Value {
        serde_json::json!([])
    }

    /// Read the whole request: headers up to the blank line, body by Content-Length.
    ///
    /// **Must read before answering.** If the connection is closed with data not yet read, Windows sends an RST, and the response the client
    /// already received gets turned into "connection aborted" (os error 10053) along with the error. These tests
    /// are green on Linux and red on Windows for exactly this reason
    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let header_end = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                return String::from_utf8_lossy(&buf).into_owned();
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
        let want: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() - header_end < want {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A service that answers once: it reads the request, writes this response back verbatim, and closes.
    /// It uses a raw socket rather than a mock library, because what it has to fake is exactly "an answer that doesn't look like a model API":
    /// plain text, incomplete, anything at all. The returned handle must be awaited, since only when it finishes is it proven the response actually went out
    async fn an_http_response(
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let content_type = content_type.to_string();
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (addr, server)
    }

    /// Same as above, but also returns the **request** verbatim. What has to be asserted is what we sent
    /// (whether `max_tokens` really got written into the body), not what the endpoint answered.
    async fn an_http_response_capturing(
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let status = status.to_string();
        let content_type = content_type.to_string();
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let _ = tx.send(request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (addr, server, rx)
    }

    // Two answers in order, both requests captured: the retry is only observable
    // as a second request, so one-shot servers cannot see it.
    async fn two_http_responses(
        first: (&str, &str, &str),
        second: (&str, &str, &str),
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<Vec<String>>,
    ) {
        http_responses(&[first, second]).await
    }

    // As many answers in order as given, every request captured
    async fn http_responses(
        answers: &[(&str, &str, &str)],
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<Vec<String>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let answers: Vec<(String, String, String)> = answers
            .iter()
            .map(|(status, content_type, body)| {
                (
                    status.to_string(),
                    content_type.to_string(),
                    body.to_string(),
                )
            })
            .collect();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, content_type, body) in answers {
                let (mut socket, _) = listener.accept().await.unwrap();
                seen.push(read_request(&mut socket).await);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            let _ = tx.send(seen);
        });
        (addr, server, rx)
    }

    fn sent_max_tokens(request: &str) -> serde_json::Value {
        let body: serde_json::Value = serde_json::from_str(
            request
                .split_once("\r\n\r\n")
                .expect("request should have a body")
                .1,
        )
        .expect("request body is JSON");
        body["max_tokens"].clone()
    }

    // HTTP chunks can break in the middle of a UTF-8 character, regardless of SSE frame boundaries.
    async fn bytewise_sse(body: &str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.as_bytes().to_vec();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            for byte in body {
                socket
                    .write_all(&[b'1', b'\r', b'\n', byte, b'\r', b'\n'])
                    .await
                    .unwrap();
            }
            socket.write_all(b"0\r\n\r\n").await.unwrap();
            socket.shutdown().await.unwrap();
        });
        (addr, server)
    }

    fn unicode_sse() -> String {
        format!(
            ": keep-alive\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{"delta": {"content": "你好🦀", "tool_calls": [{
                    "index": 0, "id": "call_1", "function": {
                        "name": "search", "arguments": "{\"city\":\"杭州\"}"
                    }
                }]}}]
            }),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}}),
        )
    }

    fn sse_with_line_endings(body: &str, endings: &[&str]) -> String {
        body.split_inclusive('\n')
            .enumerate()
            .map(|(i, line)| match line.strip_suffix('\n') {
                Some(line) => format!("{line}{}", endings[i % endings.len()]),
                None => line.to_string(),
            })
            .collect()
    }

    const SSE_LINE_ENDINGS: &[&[&str]] = &[&["\n"], &["\r\n"], &["\r"], &["\r\n", "\n", "\r"]];

    #[test]
    fn sse_line_endings_preserve_frames_at_every_chunk_boundary() {
        for endings in SSE_LINE_ENDINGS {
            let body = sse_with_line_endings(": 心跳\ndata: first\n\ndata: second\n\n", endings);
            for split in 0..=body.len() {
                let mut buffer = SseBuffer::default();
                let mut frames = Vec::new();
                for chunk in [&body.as_bytes()[..split], &body.as_bytes()[split..]] {
                    buffer.push(chunk);
                    while let Some(frame) = buffer.next_frame() {
                        frames.push(frame);
                    }
                }
                assert_eq!(
                    frames,
                    [": 心跳\ndata: first", "data: second"],
                    "line endings {endings:?}, split at byte {split}"
                );
            }
        }
    }

    #[tokio::test]
    async fn bytewise_utf8_and_sse_line_endings_survive_collected_streaming() {
        for endings in SSE_LINE_ENDINGS {
            let (addr, server) =
                bytewise_sse(&sse_with_line_endings(&unicode_sse(), endings)).await;
            let answer = client_at(addr)
                .chat_at_streaming(&[], None)
                .await
                .unwrap_or_else(|error| panic!("line endings {endings:?}: {error}"));
            server.await.unwrap();
            assert_eq!(answer.text, "你好🦀", "{endings:?}");
            assert_eq!(answer.finish_reason.as_deref(), Some("tool_calls"));
            assert_eq!(
                answer.usage,
                Some(Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3
                })
            );
        }
    }

    #[tokio::test]
    async fn bytewise_utf8_and_sse_line_endings_survive_raw_streaming() {
        use futures_util::TryStreamExt;
        for endings in SSE_LINE_ENDINGS {
            let (addr, server) =
                bytewise_sse(&sse_with_line_endings(&unicode_sse(), endings)).await;
            let stream = client_at(addr).chat_stream_raw(&[]).await.unwrap();
            let answer: Vec<String> = stream
                .try_collect()
                .await
                .unwrap_or_else(|error| panic!("line endings {endings:?}: {error}"));
            server.await.unwrap();
            assert_eq!(answer.concat(), "你好🦀", "{endings:?}");
        }
    }

    #[tokio::test]
    async fn bytewise_utf8_and_sse_line_endings_survive_tool_streaming() {
        use futures_util::TryStreamExt;
        for endings in SSE_LINE_ENDINGS {
            let (addr, server) =
                bytewise_sse(&sse_with_line_endings(&unicode_sse(), endings)).await;
            let stream = client_at(addr)
                .chat_tools_stream_with(&[], None, None)
                .await
                .unwrap();
            let items: Vec<ToolStreamItem> = stream
                .try_collect()
                .await
                .unwrap_or_else(|error| panic!("line endings {endings:?}: {error}"));
            server.await.unwrap();
            let [ToolStreamItem::Delta(delta), ToolStreamItem::Turn(turn)] = items.as_slice()
            else {
                panic!("expected a delta and completed turn: {items:?}");
            };
            assert_eq!(delta, "你好🦀", "{endings:?}");
            assert_eq!(turn.content.as_deref(), Some("你好🦀"));
            assert_eq!(turn.finish_reason.as_deref(), Some("tool_calls"));
            let [call] = turn.tool_calls.as_slice() else {
                panic!("expected one tool call: {:?}", turn.tool_calls);
            };
            assert_eq!(call.id, "call_1");
            assert_eq!(call.name, "search");
            assert_eq!(call.arguments, r#"{"city":"杭州"}"#);
        }
    }

    #[tokio::test]
    async fn raw_and_tool_streams_deliver_deltas_before_eof_with_cr_line_endings() {
        use futures_util::TryStreamExt;
        for ending in ["\r\n", "\r"] {
            for reader in ["raw", "tools"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let (release, wait) = tokio::sync::oneshot::channel();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    read_request(&mut socket).await;
                    let delta = format!(
                        "data: {}{ending}{ending}",
                        json!({"choices": [{"delta": {"content": "hello"}}]})
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{delta}\r\n",
                        delta.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    // The peer cannot close until the client observes the delta.
                    let _ = wait.await;
                    let done = format!("data: [DONE]{ending}{ending}");
                    let tail = format!("{:x}\r\n{done}\r\n0\r\n\r\n", done.len());
                    socket.write_all(tail.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                });
                let client = client_at(addr);
                match reader {
                    "raw" => {
                        let stream = client.chat_stream_raw(&[]).await.unwrap();
                        futures_util::pin_mut!(stream);
                        let first = tokio::time::timeout(Duration::from_secs(2), stream.try_next())
                            .await
                            .expect("raw delta must arrive before EOF")
                            .unwrap();
                        assert_eq!(first.as_deref(), Some("hello"));
                        release.send(()).unwrap();
                        assert!(stream.try_next().await.unwrap().is_none());
                    }
                    _ => {
                        let stream = client
                            .chat_tools_stream_with(&[], None, None)
                            .await
                            .unwrap();
                        futures_util::pin_mut!(stream);
                        let first = tokio::time::timeout(Duration::from_secs(2), stream.try_next())
                            .await
                            .expect("tool delta must arrive before EOF")
                            .unwrap();
                        assert!(
                            matches!(first, Some(ToolStreamItem::Delta(text)) if text == "hello")
                        );
                        release.send(()).unwrap();
                        assert!(matches!(
                            stream.try_next().await.unwrap(),
                            Some(ToolStreamItem::Turn(_))
                        ));
                        assert!(stream.try_next().await.unwrap().is_none());
                    }
                }
                server.await.unwrap();
            }
        }
    }

    async fn an_http_error(body: &str) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        an_http_response("502 Bad Gateway", "text/plain", body).await
    }

    /// Claims the body is 100 bytes, sends only 1, and hangs up: reading the body fails midway
    async fn an_incomplete_http_error() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx")
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        (addr, server)
    }

    /// The error carries the endpoint's own sentence, and is not classed as Unreachable
    async fn assert_diagnosis<T>(
        result: anyhow::Result<T>,
        diagnosis: &str,
        server: tokio::task::JoinHandle<()>,
    ) {
        server.await.unwrap();
        let error = match result {
            Ok(_) => panic!("the endpoint returned an error response"),
            Err(error) => error,
        };
        assert!(!is_unreachable(&error));
        assert!(error.to_string().contains(diagnosis), "{error:#}");
    }

    #[tokio::test]
    async fn an_interruption_counts_characters_in_all_three_readers() {
        use futures_util::TryStreamExt;
        let sse = format!(
            "data: {}\n\n",
            json!({ "choices": [{"delta": {"content": "你好🦀"}}] })
        );
        for endings in SSE_LINE_ENDINGS {
            let sse = sse_with_line_endings(&sse, endings);
            for reader in ["collected", "raw", "tools"] {
                let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
                let client = client_at(addr);
                let error = match reader {
                    "collected" => client.chat_at_streaming(&[], None).await.unwrap_err(),
                    "raw" => client
                        .chat_stream_raw(&[])
                        .await
                        .unwrap()
                        .try_collect::<Vec<_>>()
                        .await
                        .unwrap_err(),
                    _ => client
                        .chat_tools_stream_with(&[], None, None)
                        .await
                        .unwrap()
                        .try_collect::<Vec<_>>()
                        .await
                        .unwrap_err(),
                };
                server.await.unwrap();
                assert_eq!(
                    error.downcast_ref::<Interrupted>().unwrap().got,
                    3,
                    "{reader}, line endings {endings:?}"
                );
            }
        }
    }

    fn content_frame(text: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"delta": {"content": text}}]})
        )
    }

    async fn streamed_reply(sse: &str) -> Reply {
        let (addr, server) = an_http_response("200 OK", "text/event-stream", sse).await;
        let user = ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        };
        let reply = client_at(addr)
            .chat_at_streaming(&[user], Some(0.0))
            .await
            .unwrap();
        server.await.unwrap();
        reply
    }

    /// A repeating reply is accepted as truncated (#1301). This stream has no closing frame; without a guard it is "the stream dropped halfway"
    #[tokio::test]
    async fn a_reply_that_keeps_repeating_comes_back_cut() {
        let row = r#"[null,"a","is on","b",null,{},null,null],"#;
        let sse =
            content_frame(r#"{"e":[["a","thing",true]],"s":["#) + &content_frame(row).repeat(200);
        let reply = streamed_reply(&sse).await;
        assert!(reply.repeating);
        assert!(!reply.hit_token_ceiling(), "the endpoint never said length");
        assert!(
            reply.text.len() < row.len() * 20,
            "read {} bytes past the loop",
            reply.text.len()
        );
    }

    /// A 200-dot table-of-contents leader in a quote is not a repetition: the period is one dot, not a whole line
    #[tokio::test]
    async fn a_quoted_dot_leader_comes_back_whole() {
        let sse = content_frame(r#"{"e":[["Introduction","section",true]],"s":[["Introduction "#)
            + &content_frame(&".".repeat(200))
            + &content_frame(r#" 3","Introduction","is on page",null,"3",{},null,null]],"n":[]}"#)
            + "data: [DONE]\n\n";
        let reply = streamed_reply(&sse).await;
        assert!(!reply.repeating);
        assert!(reply.text.ends_with(r#""n":[]}"#), "{}", reply.text);
    }

    #[tokio::test]
    async fn a_raw_stream_cut_before_its_finish_is_an_error() {
        use futures_util::TryStreamExt;
        let sse = format!(
            "data: {}\n\n",
            json!({
                "choices": [{"delta": {"content": "partial answer"}}]
            })
        );
        let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
        let stream = client_at(addr).chat_stream_raw(&[]).await.unwrap();
        let error = stream.try_collect::<Vec<_>>().await.unwrap_err();
        server.await.unwrap();
        assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    }

    #[tokio::test]
    async fn a_tool_stream_cut_before_its_finish_is_an_error() {
        use futures_util::TryStreamExt;
        let sse = format!(
            "data: {}\n\n",
            json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "function": {
                        "name": "lookup", "arguments": "{\"name\":"
                    }
                }]}}]
            })
        );
        let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
        let stream = client_at(addr)
            .chat_tools_stream_with(&[], None, None)
            .await
            .unwrap();
        let error = stream.try_collect::<Vec<_>>().await.unwrap_err();
        server.await.unwrap();
        assert!(error.downcast_ref::<Interrupted>().is_some(), "{error:#}");
    }

    #[tokio::test]
    async fn tool_turns_preserve_finish_reasons_in_both_transports() {
        use futures_util::TryStreamExt;
        for reason in [
            None,
            Some("stop"),
            Some("length"),
            Some("tool_calls"),
            Some("content_filter"),
            Some("vendor_specific"),
        ] {
            let body = json!({"choices":[{"message":{"content":"answer"},"finish_reason":reason}]})
                .to_string();
            let (addr, server) = an_http_response("200 OK", "application/json", &body).await;
            let turn = client_at(addr)
                .chat_tools_with(&[], None, None)
                .await
                .unwrap();
            server.await.unwrap();
            assert_eq!(turn.finish_reason.as_deref(), reason);
            let frame = json!({"choices":[{"delta":{"content":"answer"},"finish_reason":reason}]});
            let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
            let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
            let stream = client_at(addr)
                .chat_tools_stream_with(&[], None, None)
                .await
                .unwrap();
            let items: Vec<ToolStreamItem> = stream.try_collect().await.unwrap();
            server.await.unwrap();
            let Some(ToolStreamItem::Turn(turn)) = items.last() else {
                panic!("missing turn")
            };
            assert_eq!(turn.finish_reason.as_deref(), reason);
        }
    }

    #[tokio::test]
    async fn tool_turns_preserve_reasoning_for_continuation_without_showing_it() {
        for reasoning in [Some("先查找🦀"), Some(""), None] {
            let body = json!({"choices":[{"message":{
                "content":"Searching now",
                "reasoning_content":reasoning,
                "tool_calls":[{"id":"call_1","type":"function","function":{"name":"search","arguments":"{}"}}]
            },"finish_reason":"tool_calls"}]}).to_string();
            let (addr, server) = an_http_response("200 OK", "application/json", &body).await;
            let turn = client_at(addr)
                .chat_tools_with(&[], None, None)
                .await
                .unwrap();
            server.await.unwrap();
            let message = turn.to_message();
            assert_eq!(turn.content.as_deref(), Some("Searching now"));
            assert_eq!(message["content"], "Searching now");
            assert_eq!(
                message.get("reasoning_content"),
                reasoning.map(|s| json!(s)).as_ref()
            );
            assert_eq!(message["tool_calls"][0]["id"], "call_1");
        }
    }

    #[tokio::test]
    async fn streamed_tool_turns_join_reasoning_across_frames_and_utf8_chunks() {
        use futures_util::TryStreamExt;
        let frames = [
            json!({"choices":[{"delta":{"reasoning_content":"先查找"}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"🦀"}}]}),
            json!({"choices":[{"delta":{
                "content":"Searching now",
                "tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"search","arguments":"{}"}}]
            },"finish_reason":"tool_calls"}]}),
        ];
        let sse = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let (addr, server) = bytewise_sse(&sse).await;
        let items: Vec<ToolStreamItem> = client_at(addr)
            .chat_tools_stream_with(&[], None, None)
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap();
        server.await.unwrap();
        let [ToolStreamItem::Delta(delta), ToolStreamItem::Turn(turn)] = items.as_slice() else {
            panic!("expected only the visible content delta and completed turn: {items:?}");
        };
        assert_eq!(delta, "Searching now");
        assert_eq!(turn.content.as_deref(), Some("Searching now"));
        let message = turn.to_message();
        assert_eq!(message["reasoning_content"], "先查找🦀");
        assert_eq!(message["tool_calls"][0]["id"], "call_1");
    }

    #[tokio::test]
    async fn either_finish_signal_completes_raw_and_tool_streams() {
        use futures_util::TryStreamExt;
        for ending in [
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ] {
            let sse = format!(
                "data: {}\n\n{ending}",
                json!({
                    "choices": [{"delta": {"content": "whole answer"}}]
                })
            );
            let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
            let stream = client_at(addr).chat_stream_raw(&[]).await.unwrap();
            let chunks: Vec<String> = stream.try_collect().await.unwrap();
            server.await.unwrap();
            assert_eq!(chunks.concat(), "whole answer");
            let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
            let stream = client_at(addr)
                .chat_tools_stream_with(&[], None, None)
                .await
                .unwrap();
            let items: Vec<ToolStreamItem> = stream.try_collect().await.unwrap();
            server.await.unwrap();
            let Some(ToolStreamItem::Turn(turn)) = items.last() else {
                panic!("missing completed turn")
            };
            assert_eq!(turn.content.as_deref(), Some("whole answer"));
        }
    }

    /// Collapses a streaming Q&A into one whole piece: only `delta.content` counts. Reasoning deltas are not in the return value,
    /// the usage frame is not treated as content, and no frame is an error (the endpoint opened the stream but sent nothing)
    #[tokio::test]
    async fn a_streamed_answer_arrives_whole_without_its_reasoning() {
        let sse = [
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking hard\"}}]}",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"e\\\":[\"}}]}",
            "data: {\"choices\":[{\"delta\":{\"content\":\"]}\"}}]}",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":9000}}",
            "data: [DONE]",
            "",
        ]
        .join(
            "

",
        );
        let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
        let answer = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                Some(0.0),
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            answer.text, "{\"e\":[]}",
            "only accumulate content deltas and join them into one whole"
        );

        // Stream opened but no frame was sent: that is not an empty answer, it is no answer
        // The final frame has no trailing blank line: the tail must not be dropped
        let tail = "data: {\"choices\":[{\"delta\":{\"content\":\"head\"}}]}

data: {\"choices\":[{\"delta\":{\"content\":\"tail\"},\"finish_reason\":\"stop\"}]}";
        let (addr, server) = an_http_response("200 OK", "text/event-stream", tail).await;
        let answer = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                None,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(
            answer.text, "headtail",
            "a last frame without a closing blank line must also be counted"
        );
        assert_eq!(
            answer.finish_reason.as_deref(),
            Some("stop"),
            "端点给的收尾原因要带出来"
        );

        // Opened and then dropped halfway: that is not a short answer, it is an unfinished one, worth retrying
        let cut = "data: {\"choices\":[{\"delta\":{\"content\":\"half an ans\"}}]}

";
        let (addr, server) = an_http_response("200 OK", "text/event-stream", cut).await;
        let err = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                None,
            )
            .await
            .expect_err("断在半路该是错误");
        server.await.unwrap();
        assert_eq!(
            crate::transient(&err).map(|(w, _)| w),
            Some("LLM response stream was interrupted"),
            "{err:#}"
        );

        let (addr, server) = an_http_response("200 OK", "text/event-stream", "").await;
        let err = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                None,
            )
            .await
            .expect_err("没有帧该是错误");
        server.await.unwrap();
        assert!(format!("{err:#}").contains("no frames"), "{err:#}");
    }

    /// A model whose ceiling is below ours is retried at its own, not failed (#891).
    ///
    /// The old behaviour was one 400 per chunk and an extraction that produced
    /// nothing, because `MAX_COMPLETION_TOKENS` sits above what gpt-4o-mini will
    /// accept and the endpoint refuses rather than clamps.
    #[tokio::test]
    async fn a_ceiling_the_endpoint_refuses_is_retried_at_the_one_it_states() {
        let refusal = r#"{"error":{"message":"max_tokens is too large: 65536. This model supports at most 16384 completion tokens, whereas you provided 65536.","type":"invalid_request_error","param":"max_tokens"}}"#;
        let answer = [
            r#"data: {"choices":[{"delta":{"content":"{\"e\":[]}"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n");
        let (addr, server, requests) = two_http_responses(
            ("400 Bad Request", "application/json", refusal),
            ("200 OK", "text/event-stream", &answer),
        )
        .await;

        let reply = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                Some(0.0),
            )
            .await
            .unwrap();
        server.await.unwrap();

        let sent = requests.await.unwrap();
        assert_eq!(sent.len(), 2, "the refusal must be retried, not surfaced");
        assert_eq!(
            sent_max_tokens(&sent[0]),
            json!(MAX_COMPLETION_TOKENS),
            "the first attempt still asks for the ceiling that is ours"
        );
        assert_eq!(
            sent_max_tokens(&sent[1]),
            json!(16_384),
            "the retry asks for the ceiling the endpoint stated, not a guess"
        );
        assert_eq!(reply.text, r#"{"e":[]}"#);
    }

    /// The stated ceiling is remembered on the client: the next call, and a clone's
    /// call, start from it, so a process pays the refusal once and not once per chunk.
    #[tokio::test]
    async fn a_stated_ceiling_is_kept_for_the_next_call_and_for_clones() {
        let refusal = r#"{"error":{"message":"max_tokens is too large: 65536. This model supports at most 16,384 completion tokens, whereas you provided 65536.","type":"invalid_request_error","param":"max_tokens"}}"#;
        let answer = [
            r#"data: {"choices":[{"delta":{"content":"ok"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n");
        let (addr, server, requests) = http_responses(&[
            ("400 Bad Request", "application/json", refusal),
            ("200 OK", "text/event-stream", &answer),
            ("200 OK", "text/event-stream", &answer),
            ("200 OK", "text/event-stream", &answer),
        ])
        .await;
        let client = client_at(addr);
        let msgs = [ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        client.chat_at_streaming(&msgs, None).await.unwrap();
        client.chat_at_streaming(&msgs, None).await.unwrap();
        client
            .clone()
            .with_reasoning_effort(Some("low".into()))
            .chat_at_streaming(&msgs, None)
            .await
            .unwrap();
        server.await.unwrap();
        let sent = requests.await.unwrap();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent_max_tokens(&sent[0]), json!(MAX_COMPLETION_TOKENS));
        assert_eq!(
            sent_max_tokens(&sent[1]),
            json!(16_384),
            "retried at the stated ceiling"
        );
        assert_eq!(
            sent_max_tokens(&sent[2]),
            json!(16_384),
            "the next call starts there"
        );
        assert_eq!(sent_max_tokens(&sent[3]), json!(16_384), "so does a clone");
    }

    fn complete_chat_response(streaming: bool) -> wiremock::ResponseTemplate {
        let response = wiremock::ResponseTemplate::new(200);
        if streaming {
            response.set_body_raw(
                "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                "text/event-stream",
            )
        } else {
            response.set_body_json(json!({"choices": [{"message": {"content": "ok"}}]}))
        }
    }

    async fn ask_at_zero(client: &LlmClient, streaming: bool) -> anyhow::Result<String> {
        let messages = [ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }];
        if streaming {
            Ok(client.chat_at_streaming(&messages, Some(0.0)).await?.text)
        } else {
            client.chat_at(&messages, Some(0.0)).await
        }
    }

    #[tokio::test]
    async fn refused_parameters_and_a_lower_ceiling_are_learned_and_shared_with_clones() {
        use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                if body.get("max_tokens").is_some() {
                    ResponseTemplate::new(400).set_body_json(json!({"error": {
                        "param": "max_tokens", "code": "unsupported_parameter",
                        "message": "Use max_completion_tokens instead."
                    }}))
                } else if body["max_completion_tokens"] != 16_384 {
                    ResponseTemplate::new(400).set_body_json(json!({"error": {
                        "message": "max_completion_tokens must be at most 16384 tokens"
                    }}))
                } else if body.get("temperature").is_some() {
                    ResponseTemplate::new(400).set_body_json(json!({"error": {
                        "param": "temperature", "code": "unsupported_value",
                        "message": "Use the default sampling settings."
                    }}))
                } else {
                    complete_chat_response(true)
                }
            })
            .expect(6)
            .mount(&server)
            .await;
        let client = LlmClient::new(&server.uri(), None, "m");
        let cloned = client.clone();
        for caller in [&client, &client, &cloned] {
            assert_eq!(ask_at_zero(caller, true).await.unwrap(), "ok");
        }
        let sent: Vec<serde_json::Value> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.body_json().unwrap())
            .collect();
        assert_eq!(sent[0]["max_tokens"], MAX_COMPLETION_TOKENS);
        assert_eq!(sent[1]["max_completion_tokens"], MAX_COMPLETION_TOKENS);
        for body in &sent[1..] {
            assert!(body.get("max_tokens").is_none());
        }
        for body in &sent[2..] {
            assert_eq!(body["max_completion_tokens"], 16_384);
        }
        assert_eq!(
            sent.iter()
                .map(|b| b.get("temperature").is_some())
                .collect::<Vec<_>>(),
            [true, true, true, false, false, false]
        );
    }

    #[tokio::test]
    async fn a_refused_temperature_is_learned_across_streaming_calls_and_clones() {
        use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};
        for refusal in [
            json!({"error": {"param": "temperature", "code": "unsupported_value",
                "message": "Only the default sampling settings are supported."}}),
            json!({"error": {"param": "temperature", "code": "unsupported_parameter",
                "message": "Use the default sampling settings."}}),
            json!({"message": "Unsupported value: 'temperature' does not support 0 with this model. Only the default (1) value is supported."}),
        ] {
            for streaming in [false, true] {
                let server = MockServer::start().await;
                let refusal = refusal.clone();
                Mock::given(method("POST"))
                    .respond_with(move |request: &Request| {
                        let body: serde_json::Value = request.body_json().unwrap();
                        if body.get("temperature").is_some() {
                            ResponseTemplate::new(400).set_body_json(&refusal)
                        } else {
                            complete_chat_response(body["stream"] == true)
                        }
                    })
                    .expect(4)
                    .mount(&server)
                    .await;
                let client = LlmClient::new(&server.uri(), None, "m");
                let cloned = client.clone();
                assert_eq!(ask_at_zero(&client, streaming).await.unwrap(), "ok");
                assert_eq!(ask_at_zero(&cloned, !streaming).await.unwrap(), "ok");
                assert_eq!(ask_at_zero(&client, streaming).await.unwrap(), "ok");
                let temperatures: Vec<bool> = server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .map(|r| {
                        r.body_json::<serde_json::Value>()
                            .unwrap()
                            .get("temperature")
                            .is_some()
                    })
                    .collect();
                assert_eq!(temperatures, [true, false, false, false]);
            }
        }
    }

    #[tokio::test]
    async fn each_compatibility_adjustment_is_tried_only_once_per_call() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        for (streaming, parameter) in [
            (true, "max_tokens"),
            (true, "temperature"),
            (false, "temperature"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
                    "param": parameter, "code": "unsupported_parameter",
                    "message": "Repeated refusal"
                }})))
                // The third time it gets a 404, so a retry with no attempt limit exposes itself immediately.
                .up_to_n_times(2)
                .expect(2)
                .mount(&server)
                .await;
            let client = LlmClient::new(&server.uri(), None, "m");
            let err = ask_at_zero(&client, streaming).await.unwrap_err();
            let refusal = rejected(&err).unwrap();
            assert_eq!(refusal.status, 400);
            assert_eq!(refusal.detail, "Repeated refusal");
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    /// Only a number the message calls tokens is a ceiling; the rest of the digits in a
    /// refusal are a model name, a status code, or our own request echoed back.
    #[test]
    fn a_stated_ceiling_is_a_number_the_message_calls_tokens() {
        let sent = MAX_COMPLETION_TOKENS;
        assert_eq!(
            stated_completion_ceiling("max_tokens is too large: 65536. This model supports at most 16384 completion tokens, whereas you provided 65536.", sent),
            Some(16_384)
        );
        assert_eq!(
            stated_completion_ceiling("max_tokens is too large: 65,536. This model supports at most 16,384 completion tokens.", sent),
            Some(16_384),
            "thousands separators are part of the number, not a split"
        );
        assert_eq!(
            stated_completion_ceiling(
                "max_tokens is too large for model gpt-4o-mini-2024-07-18",
                sent
            ),
            None,
            "a model name's digits are not a limit"
        );
        assert_eq!(
            stated_completion_ceiling("This model's maximum context length is 32768 tokens. However, you requested 65636 tokens (100 in the messages, 65536 in the completion). Please reduce the length of the messages or max_tokens.", sent),
            Some(32_668),
            "the messages fit, so the ceiling is what the window leaves for the completion"
        );
        assert_eq!(
            stated_completion_ceiling("This model's maximum context length is 32768 tokens. However, you requested 98304 tokens (32768 in the messages, 65536 in the completion).", sent),
            None,
            "the messages alone fill the window: nothing to lower to"
        );
        assert_eq!(
            stated_completion_ceiling(
                "maximum context length is 4096 tokens; request has 12000 tokens",
                sent
            ),
            None,
            "no ledger, so a context window is not a completion ceiling"
        );
        assert_eq!(
            stated_completion_ceiling("max_completion_tokens must be at most 8 tokens", sent),
            None,
            "nothing below 1,024 is a completion limit"
        );
        assert_eq!(
            stated_completion_ceiling("Invalid value for 'temperature': must be <= 2 tokens", sent),
            None,
            "a message that is not about max_tokens is left alone"
        );
    }

    /// A 400 about anything else is still a 400, and is not retried.
    #[tokio::test]
    async fn a_refusal_that_names_no_ceiling_is_not_retried() {
        let refusal = r#"{"error":{"message":"Invalid value for 'temperature': must be <= 2","type":"invalid_request_error","param":"temperature"}}"#;
        let (addr, server, request) =
            an_http_response_capturing("400 Bad Request", "application/json", refusal).await;

        let failed = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                Some(0.0),
            )
            .await;
        server.await.unwrap();
        request.await.unwrap();

        let err = failed.expect_err("an unrelated 400 must reach the caller");
        assert!(err.to_string().contains("temperature"), "{err}");
    }

    /// The cap is ours, so being truncated can be stated (#760).
    ///
    /// The two are tested together because they are two halves of one fault. Without `max_tokens`, where the answer
    /// gets cut is up to the endpoint's default. Sending it but dropping `finish_reason` means nobody knows why it was cut even when it is.
    #[tokio::test]
    async fn a_token_ceiling_is_ours_and_a_cut_reply_says_so() {
        // `length` = the endpoint says "I stopped because I hit the cap". Such a reply looks exactly like a normal close
        let cut = [
            r#"data: {"choices":[{"delta":{"content":"{\"e\":[{\"n\":\"half"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            "data: [DONE]",
            "",
        ]
        .join(
            "

",
        );
        let (addr, server, request) =
            an_http_response_capturing("200 OK", "text/event-stream", &cut).await;
        let reply = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                Some(0.0),
            )
            .await
            .unwrap();
        server.await.unwrap();

        // First, the request really carried a cap, and it was ours
        let sent = request.await.unwrap();
        let body: serde_json::Value = serde_json::from_str(
            sent.split_once("\r\n\r\n")
                .expect("request should have a body")
                .1,
        )
        .expect("request body is JSON");
        assert_eq!(
            body["max_tokens"],
            json!(MAX_COMPLETION_TOKENS),
            "不送这个字段，答案断在哪里就是端点的默认值说了算：{body}"
        );

        // Second, the truncation reaches the caller. The answer is still the complete half: a half reply
        // is still valuable. What is lost is the tail, not everything
        assert!(reply.hit_token_ceiling(), "{:?}", reply.finish_reason);
        assert_eq!(reply.finish_reason.as_deref(), Some("length"));
        assert!(reply.text.starts_with(r#"{"e":["#), "{}", reply.text);

        // A normal close is not a truncation: both end the stream normally, and only the reasons are told apart
        let whole =
            "data: {\"choices\":[{\"delta\":{\"content\":\"{}\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
        let (addr, server) = an_http_response("200 OK", "text/event-stream", whole).await;
        let reply = client_at(addr)
            .chat_at_streaming(
                &[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                None,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert!(!reply.hit_token_ceiling(), "stop is not truncation");
    }

    /// Separates what will clear up by itself from what won't: the gateway ones are [`Unavailable`], a wrong key stays
    /// [`Rejected`], and out-of-credit and rate limiting each go their own way. The caller decides from this whether to "wait a while and try again" or "this chunk is dead"
    #[tokio::test]
    async fn a_gateway_failure_is_transient_and_a_bad_key_is_not() {
        for status in [
            "502 Bad Gateway",
            "503 Service Unavailable",
            "504 Gateway Timeout",
            "408 Request Timeout",
        ] {
            let (addr, server) = an_http_response(status, "text/plain", "upstream hiccup").await;
            let err = client_at(addr)
                .chat(&[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }])
                .await
                .expect_err("非成功状态该是错误");
            server.await.unwrap();
            let hit = crate::unavailable(&err).unwrap_or_else(|| {
                panic!("{status} should be a kind that recovers on its own: {err:#}")
            });
            assert_eq!(hit.status, status[..3].parse::<u16>().unwrap());
            assert!(
                format!("{err:#}").contains("upstream hiccup"),
                "原话要带出来：{err:#}"
            );
        }

        // The request never arrived (could not connect). This is also a class that clears up by itself
        let nowhere = ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        };
        let dead = LlmClient::new("http://127.0.0.1:1", None, "m");
        let err = dead.chat(&[nowhere]).await.expect_err("连不上该是错误");
        assert!(crate::is_unreachable(&err), "{err:#}");
        assert_eq!(
            crate::transient(&err).map(|(w, _)| w),
            Some("LLM request could not be sent"),
            "{err:#}"
        );

        // Wrong key or invalid request: retrying ten thousand times is still wrong, so it must not be mixed in
        for status in [
            "401 Unauthorized",
            "400 Bad Request",
            "500 Internal Server Error",
        ] {
            let (addr, server) = an_http_response(status, "text/plain", "no").await;
            let err = client_at(addr)
                .chat(&[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }])
                .await
                .expect_err("非成功状态该是错误");
            server.await.unwrap();
            assert!(
                crate::unavailable(&err).is_none(),
                "{status} 不该被当成会自己好的：{err:#}"
            );
            assert!(
                crate::rate_limited(&err).is_none(),
                "{status} 不是限流：{err:#}"
            );
        }
    }

    /// The thesis of #527: a plain-text 502, and all five request paths (chat, tool chat, both streaming kinds, embedding)
    /// must carry that sentence through verbatim, instead of reporting "unknown error" or "could not connect".
    #[tokio::test]
    async fn a_body_that_is_not_json_is_quoted_and_not_called_unreachable() {
        let diagnosis = "proxy says the model context is too long";
        let (addr, server) = an_http_error(diagnosis).await;
        let client = client_at(addr);
        assert_diagnosis(
            client
                .chat(&[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }])
                .await,
            diagnosis,
            server,
        )
        .await;

        let (addr, server) = an_http_error(diagnosis).await;
        let client = client_at(addr);
        assert_diagnosis(client.chat_tools(&[], &no_tools()).await, diagnosis, server).await;

        let (addr, server) = an_http_error(diagnosis).await;
        let client = client_at(addr);
        assert_diagnosis(
            client.chat_tools_stream(&[], &no_tools()).await,
            diagnosis,
            server,
        )
        .await;

        let (addr, server) = an_http_error(diagnosis).await;
        let client = client_at(addr);
        assert_diagnosis(
            client
                .chat_stream(&[ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                }])
                .await,
            diagnosis,
            server,
        )
        .await;

        let (addr, server) = an_http_error(diagnosis).await;
        let client = client_at(addr);
        assert_diagnosis(client.embed(&["hi".to_string()]).await, diagnosis, server).await;
    }

    /// The opposite: the body was cut off midway through reading. That is a transport-layer matter and still belongs to Unreachable.
    /// Do not go so far in quoting the original that a real connection failure gets reported as the endpoint's diagnosis.
    #[tokio::test]
    async fn a_body_that_cannot_be_read_stays_unreachable() {
        let (addr, server) = an_incomplete_http_error().await;
        let client = client_at(addr);
        let result = client.chat(&[]).await;
        server.await.unwrap();
        let error = result.expect_err("the body is incomplete");
        assert!(
            is_unreachable(&error),
            "body read error lost its source: {error:#}"
        );
    }

    /// The success path is unchanged: checking the status first does not change how the success response is parsed
    #[tokio::test]
    async fn successful_responses_still_parse() {
        let (addr, server) = an_http_response(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"ok"}}]}"#,
        )
        .await;
        let client = client_at(addr);
        let result = client.chat(&[]).await;
        server.await.unwrap();
        assert_eq!(result.unwrap(), "ok");

        let (addr, server) = an_http_response(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"ok"}}]}"#,
        )
        .await;
        let client = client_at(addr);
        let result = client.chat_tools(&[], &no_tools()).await;
        server.await.unwrap();
        assert_eq!(result.unwrap().content.as_deref(), Some("ok"));

        let (addr, server) = an_http_response(
            "200 OK",
            "application/json",
            r#"{"data":[{"embedding":[1.0]}]}"#,
        )
        .await;
        let client = client_at(addr);
        let result = client.embed(&["hi".to_string()]).await;
        server.await.unwrap();
        assert_eq!(result.unwrap(), vec![vec![1.0]]);
    }

    /// The thesis of #690: a reasoning model writes its thinking inline as `<think>` in the reply body,
    /// and `chat` returns only the half after the marker. Extraction, adjudication and connectivity tests all read that.
    /// A pair of braces is deliberately placed in the thinking: `json_block` takes "first `{` to last `}`".
    /// Without cutting it here, downstream would get stuck in the thinking and pick the wrong start and end.
    #[tokio::test]
    async fn a_reasoning_reply_keeps_only_what_follows_think() {
        let (addr, server) = an_http_response(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"让我想想 {\"a\": 1}。\n</think>{\"facts\": []}"}}]}"#,
        )
        .await;
        let client = client_at(addr);
        let result = client.chat(&[]).await;
        server.await.unwrap();
        assert_eq!(result.unwrap(), r#"{"facts": []}"#);
    }

    /// The opposite: a reply without a marker is returned as-is. Non-reasoning models are unaffected by this path
    #[tokio::test]
    async fn a_reply_without_a_think_marker_is_untouched() {
        let (addr, server) = an_http_response(
            "200 OK",
            "application/json",
            r#"{"choices":[{"message":{"content":"{\"facts\": []}"}}]}"#,
        )
        .await;
        let client = client_at(addr);
        let result = client.chat(&[]).await;
        server.await.unwrap();
        assert_eq!(result.unwrap(), r#"{"facts": []}"#);
    }

    /// Takes the last marker, to guard against the body quoting it
    #[test]
    fn strip_reasoning_cuts_before_the_last_marker() {
        assert_eq!(
            strip_reasoning("a</think>b</think>c"),
            "c",
            "earlier markers are part of the text, not the split point"
        );
    }

    /// The shape measured by `settings/test` (#690): `"OK</think>OK"`
    #[test]
    fn strip_reasoning_fixes_the_settings_test_echo() {
        assert_eq!(strip_reasoning("OK</think>OK"), "OK");
    }

    /// Endpoints that write the error directly as a string, like `{"error": "…"}` (LM Studio is one), were previously not recognized
    #[test]
    fn an_error_given_as_a_string_is_forwarded() {
        let body = serde_json::json!({ "error": "model is unavailable" });
        assert_eq!(
            err_detail(&body, ""),
            "model is unavailable",
            "a valid error field must not become unknown"
        );
    }

    /// OpenAI shape unchanged
    #[test]
    fn a_body_with_error_message_still_reads_it() {
        let body = serde_json::json!({ "error": { "message": "bad request" } });
        assert_eq!(err_detail(&body, ""), "bad request");
    }

    /// Truncation is by characters: a 501-character string is cut to 500, rather than split in the middle of a character's bytes
    #[test]
    fn a_long_body_is_truncated() {
        let raw = "界".repeat(MAX_ERROR_DETAIL_CHARS + 1);
        let detail = err_detail(&serde_json::Value::Null, &raw);
        assert_eq!(detail.chars().count(), MAX_ERROR_DETAIL_CHARS);
    }

    /// Really sends a request that is bound to fail, to get a genuine `reqwest::Error`.
    /// Nothing listens on port 1, and 127.0.0.1 does not go through a proxy.
    async fn a_real_transport_error() -> reqwest::Error {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get("http://127.0.0.1:1/")
            .send()
            .await
            .expect_err("端口 1 不该连得上")
    }

    /// **A server that accepts the connection but never sends a byte.**
    ///
    /// This is the form that really happens in production, and the hardest to spot: TCP connects, TLS completes,
    /// and the request goes out, then nothing. Connection errors report immediately; this one does not. Without a timeout,
    /// `send().await` stays parked there forever, and the worker slot calling it is never released.
    async fn a_server_that_never_answers() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accepts the connection and holds on to it. **Must hold the socket**: a drop sends a FIN,
            // and the test would then be measuring a closed connection, not silence
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        addr
    }

    /// When a request hangs it must **return an error**, not wait forever.
    ///
    /// Without this guard the regression looks like this: one ingestion dies at chunk 55, the 32 worker slots are
    /// permanently occupied, the jobs table is all running, and the logs and UI show nothing at all.
    #[tokio::test]
    async fn a_silent_server_ends_in_an_error_not_a_hang() {
        let addr = a_server_that_never_answers().await;
        let client = LlmClient::with_timeouts(
            &format!("http://{addr}"),
            None,
            "m",
            Duration::from_secs(5),
            Duration::from_millis(300),
        );
        let started = tokio::time::Instant::now();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.chat(&[ChatMessage {
                role: "user".into(),
                content: "hi".into(),
            }]),
        )
        .await;

        // The outer timeout firing means the client did not cut it off itself, which is exactly the bug to fix
        let inner = out.expect("client has no timeout, so the request hangs forever");
        assert!(inner.is_err(), "a silent server must not count as success");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "read_timeout 没生效：等了 {:?}",
            started.elapsed()
        );
    }

    /// **The check must pass through the context layers.**
    ///
    /// This is the link in the chain most likely to break quietly. Every time a caller adds one `.context("extraction failed")`
    /// it rewrites the error text, and a check that matches on text breaks the same day. The symptom is that alerts stop appearing,
    /// no test turns red, and no user comes to report "I didn't get the alert".
    #[tokio::test]
    async fn unreachable_survives_context_layers() {
        let raw = a_real_transport_error().await;
        let err = anyhow::Error::new(Unreachable(raw))
            .context("embedding failed")
            .context("process_document failed");
        assert!(is_unreachable(&err));
        // Also pins down the fact that "the text changes": the outermost layer no longer contains any of the endpoint's wording
        assert!(!err.to_string().contains("endpoint"));
    }

    /// The opposite: an ordinary error should not be recognized as an endpoint problem, or alerts would fire for every failure.
    #[tokio::test]
    async fn an_ordinary_failure_is_not_the_endpoint() {
        let e = anyhow::anyhow!("Embedding 响应缺少向量").context("extraction failed");
        assert!(!is_unreachable(&e));
    }

    /// **Rate limiting must be recognized through the context layers.**
    ///
    /// Same reason as [`unreachable_survives_context_layers`], with heavier consequences. Failing to recognize it falls back to "this chunk is dead",
    /// while rate limiting would clear up a minute later. In one ingestion of 1884 chunks,
    /// 55 of 60 documents failed whole because of this.
    #[tokio::test]
    async fn a_rate_limit_survives_context_layers() {
        let err = anyhow::Error::new(RateLimited {
            status: 429,
            retry_after: None,
            detail: "TPM limit reached".into(),
        })
        .context("extraction failed")
        .context("process_document failed");
        let hit = rate_limited(&err).expect("rate limit was not recognized");
        assert_eq!(hit.status, 429);
        assert!(!err.to_string().contains("rate limiting"));
    }

    /// **No `Retry-After` is the normal case, not an exception.**
    ///
    /// Most vendors' 429 does not carry this header. The check must look only at the type; backoff is supplied by the caller.
    /// If `retry_after.is_some()` were the criterion, none of these vendors would be recognized.
    #[tokio::test]
    async fn a_rate_limit_without_retry_after_is_still_a_rate_limit() {
        let err = anyhow::Error::new(RateLimited {
            status: 429,
            retry_after: None,
            detail: "TPM limit reached".into(),
        });
        assert!(rate_limited(&err).is_some());
        assert!(rate_limited(&err).unwrap().retry_after.is_none());
    }

    /// The opposite: rate limiting should not be treated as an unreachable endpoint. The two are handled completely differently.
    #[tokio::test]
    async fn a_rate_limit_is_not_an_unreachable_endpoint() {
        let err = anyhow::Error::new(RateLimited {
            status: 429,
            retry_after: Some(Duration::from_secs(7)),
            detail: "slow down".into(),
        });
        assert!(!is_unreachable(&err));
        assert_eq!(
            rate_limited(&err).unwrap().retry_after,
            Some(Duration::from_secs(7))
        );
    }

    /// **429 is not necessarily rate limiting.** OpenAI uses the same status code for "too fast" and "out of money".
    /// The dividing line is in `error.code`. Classifying by status code alone, a broke account would be backed off and
    /// retried without end. The longer the retries, the more it looks like "the endpoint is slow", and the harder it is to trace to the root.
    #[test]
    fn a_429_that_says_insufficient_quota_is_a_billing_problem() {
        let body = serde_json::json!({
            "error": { "message": "You exceeded your current quota", "code": "insufficient_quota" }
        });
        let e = failure(
            "LLM",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
            &body,
            "",
        );
        assert!(
            out_of_credit(&e).is_some(),
            "should be classed as out of credit"
        );
        assert!(
            rate_limited(&e).is_none(),
            "should not be classed as a rate limit"
        );
    }

    /// 402 is the standard answer. SiliconFlow uses exactly that.
    #[test]
    fn a_402_is_a_billing_problem() {
        let body = serde_json::json!({ "message": "Sorry, your account balance is insufficient" });
        let e = failure(
            "LLM",
            reqwest::StatusCode::PAYMENT_REQUIRED,
            None,
            &body,
            "",
        );
        assert!(out_of_credit(&e).is_some());
    }

    /// The opposite: a 429 without that identifier is still rate limiting. Do not classify something that clears up by itself as needing a human to act.
    #[test]
    fn a_plain_429_is_still_a_rate_limit() {
        let body = serde_json::json!({ "error": { "message": "TPM limit reached" } });
        let e = failure(
            "LLM",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
            &body,
            "",
        );
        assert!(rate_limited(&e).is_some());
        assert!(out_of_credit(&e).is_none());
    }

    /// The opposite: other 4xx are not rate limiting. A wrong key stays wrong no matter how often you retry.
    #[tokio::test]
    async fn an_auth_failure_is_not_a_rate_limit() {
        let e = anyhow::anyhow!("LLM request failed (401 Unauthorized): bad key");
        assert!(rate_limited(&e).is_none());
    }

    /// A clean 4xx from the endpoint does not count as "not reachable". It means it is a model API,
    /// only the key or model name is wrong, and the people to ask and what to do are different.
    #[tokio::test]
    async fn a_clean_api_error_is_a_different_problem() {
        let e = anyhow::anyhow!("LLM request failed (401 Unauthorized): bad key");
        assert!(!is_unreachable(&e));
    }
    async fn embeddings_from(data: serde_json::Value) -> anyhow::Result<Vec<Vec<f32>>> {
        let body = json!({ "data": data }).to_string();
        let (addr, server) = an_http_response("200 OK", "application/json", &body).await;
        let result = client_at(addr)
            .embed(&["first".into(), "second".into()])
            .await;
        server.await.unwrap();
        result
    }

    /// An entry that writes index but yields no number (`null`, string, float) counts as having no index. These shapes work today,
    /// and a key-based check would send them into the index branch and error out, failing the whole batch
    #[tokio::test]
    async fn an_index_that_is_not_a_number_falls_back_to_position() {
        for data in [
            json!([{"index": null, "embedding": [1.0]}, {"index": null, "embedding": [2.0]}]),
            json!([{"index": "0", "embedding": [1.0]}, {"index": "1", "embedding": [2.0]}]),
            json!([{"index": 0.5, "embedding": [1.0]}, {"index": 1.5, "embedding": [2.0]}]),
        ] {
            let out = embeddings_from(data.clone())
                .await
                .unwrap_or_else(|e| panic!("{data} should pair by position but failed: {e}"));
            assert_eq!(out, vec![vec![1.0], vec![2.0]], "{data}");
        }
    }

    #[tokio::test]
    async fn indexed_embeddings_follow_the_input_order() {
        let out = embeddings_from(json!([
            {"index": 1, "embedding": [2.0, 20.0]},
            {"index": 0, "embedding": [1.0, 10.0]}
        ]))
        .await
        .unwrap();
        assert_eq!(out, vec![vec![1.0, 10.0], vec![2.0, 20.0]]);
    }

    /// Gemini's compatible endpoint omits only the first item's `index` (zero is the default). Three inputs come back as
    /// `[{embedding}, {index: 1}, {index: 2}]`. An entry with no index stands in its own position
    #[tokio::test]
    async fn an_item_without_an_index_takes_its_position_beside_indexed_ones() {
        let body = json!({ "data": [
            {"embedding": [1.0]},
            {"index": 1, "embedding": [2.0]},
            {"index": 2, "embedding": [3.0]}
        ]})
        .to_string();
        let (addr, server) = an_http_response("200 OK", "application/json", &body).await;
        let out = client_at(addr)
            .embed(&["one".into(), "two".into(), "three".into()])
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(out, vec![vec![1.0], vec![2.0], vec![3.0]]);
    }

    /// A negative number cannot become a `u64`, so like `null` and strings it counts by position, even if other items in the same batch do write indexes
    #[tokio::test]
    async fn a_negative_index_falls_back_to_position_beside_indexed_ones() {
        let out = embeddings_from(json!([
            {"index": 0, "embedding": [1.0]}, {"index": -1, "embedding": [2.0]}
        ]))
        .await
        .unwrap();
        assert_eq!(out, vec![vec![1.0], vec![2.0]]);
    }

    #[tokio::test]
    async fn ambiguous_embedding_indices_are_rejected() {
        for data in [
            json!([{"index": 0, "embedding": [1.0]}, {"index": 0, "embedding": [2.0]}]),
            json!([{"index": 0, "embedding": [1.0]}, {"index": 2, "embedding": [2.0]}]),
            json!([{"index": 1, "embedding": [1.0]}, {"embedding": [2.0]}]),
            json!([{"index": 0, "embedding": [1.0]}]),
            json!([{"embedding": [1.0]}, {"embedding": [2.0]}, {"embedding": [3.0]}]),
        ] {
            assert!(
                embeddings_from(data.clone()).await.is_err(),
                "accepted {data}"
            );
        }
    }

    #[tokio::test]
    async fn embeddings_without_indices_keep_positional_compatibility() {
        let out = embeddings_from(json!([
            {"embedding": [1.0, 10.0]}, {"embedding": [2.0, 20.0]}
        ]))
        .await
        .unwrap();
        assert_eq!(out, vec![vec![1.0, 10.0], vec![2.0, 20.0]]);
    }

    /// Gemini 3's shape: the tool_call has no `index`, but has `extra_content.google.thought_signature`, and
    /// finish_reason says `stop`. Both transports must keep `extra_content` and echo it back when replaying
    #[tokio::test]
    async fn a_tool_call_keeps_its_extra_content_in_both_transports() {
        use futures_util::TryStreamExt;
        let extra = json!({"google": {"thought_signature": "Eq0CCqoC"}});
        let call = json!({
            "id": "call_170866", "type": "function",
            "function": {"name": "search_kb", "arguments": "{\"query\":\"lore\"}"},
            "extra_content": extra
        });
        let body = json!({"choices":[{"message":{"role":"assistant","tool_calls":[call]},"finish_reason":"stop"}]})
            .to_string();
        let (addr, server) = an_http_response("200 OK", "application/json", &body).await;
        let turn = client_at(addr)
            .chat_tools_with(&[], None, None)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(turn.tool_calls[0].extra_content, Some(extra.clone()));
        assert_eq!(turn.to_message()["tool_calls"][0]["extra_content"], extra);

        let frame =
            json!({"choices":[{"delta":{"role":"assistant","tool_calls":[call]},"index":0}]});
        let done =
            json!({"choices":[{"delta":{"role":"assistant"},"finish_reason":"stop","index":0}]});
        let sse = format!("data: {frame}\n\ndata: {done}\n\ndata: [DONE]\n\n");
        let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
        let stream = client_at(addr)
            .chat_tools_stream_with(&[], None, None)
            .await
            .unwrap();
        let items: Vec<ToolStreamItem> = stream.try_collect().await.unwrap();
        server.await.unwrap();
        let [ToolStreamItem::Turn(turn)] = items.as_slice() else {
            panic!("expected one completed turn: {items:?}");
        };
        assert_eq!(turn.tool_calls[0].extra_content, Some(extra.clone()));
        assert_eq!(turn.to_message()["tool_calls"][0]["extra_content"], extra);
    }

    /// A call without `extra_content`, or whose value is not an object, does not get this key added back when replayed
    #[tokio::test]
    async fn a_tool_call_without_extra_content_echoes_none() {
        for call in [
            json!({"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}),
            json!({"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}, "extra_content": null}),
            json!({"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}, "extra_content": "sig"}),
        ] {
            let body = json!({"choices":[{"message":{"role":"assistant","tool_calls":[call]},"finish_reason":"tool_calls"}]})
                .to_string();
            let (addr, server) = an_http_response("200 OK", "application/json", &body).await;
            let turn = client_at(addr)
                .chat_tools_with(&[], None, None)
                .await
                .unwrap();
            server.await.unwrap();
            assert_eq!(turn.tool_calls[0].extra_content, None, "{call}");
            assert!(
                turn.to_message()["tool_calls"][0]
                    .get("extra_content")
                    .is_none(),
                "{call}"
            );
        }
    }

    /// Gemini's parallel calls: one per chunk, with no `index` on the chunk, each with its own `id`. Two calls,
    /// with names and arguments each their own
    #[tokio::test]
    async fn parallel_tool_calls_without_an_index_stay_separate() {
        use futures_util::TryStreamExt;
        let first = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{
            "function":{"arguments":"{\"name\":\"lore\"}","name":"find_entities"},
            "id":"call_266956","type":"function"}]},"index":0}]});
        let second = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{
            "function":{"arguments":"{\"query\":\"what's next\"}","name":"search_chunks"},
            "id":"call_266959","type":"function"}]},"index":0}]});
        let done =
            json!({"choices":[{"delta":{"role":"assistant"},"finish_reason":"stop","index":0}]});
        let sse = format!("data: {first}\n\ndata: {second}\n\ndata: {done}\n\ndata: [DONE]\n\n");
        let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
        let stream = client_at(addr)
            .chat_tools_stream_with(&[], None, None)
            .await
            .unwrap();
        let items: Vec<ToolStreamItem> = stream.try_collect().await.unwrap();
        server.await.unwrap();
        let [ToolStreamItem::Turn(turn)] = items.as_slice() else {
            panic!("expected one completed turn: {items:?}");
        };
        let calls: Vec<(&str, &str, &str)> = turn
            .tool_calls
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![
                ("call_266956", "find_entities", "{\"name\":\"lore\"}"),
                (
                    "call_266959",
                    "search_chunks",
                    "{\"query\":\"what's next\"}"
                ),
            ]
        );
    }

    /// A delta with neither `index` nor `id` is a continuation of the previous call: its argument fragment is appended
    #[tokio::test]
    async fn an_unindexed_delta_without_an_id_continues_the_last_call() {
        use futures_util::TryStreamExt;
        let head = json!({"choices":[{"delta":{"tool_calls":[{
            "id":"call_1","function":{"name":"lookup","arguments":"{\"name\":"}}]}}]});
        let tail = json!({"choices":[{"delta":{"tool_calls":[{
            "function":{"arguments":"\"Acme\"}"}}]}}]});
        let done = json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]});
        let sse = format!("data: {head}\n\ndata: {tail}\n\ndata: {done}\n\ndata: [DONE]\n\n");
        let (addr, server) = an_http_response("200 OK", "text/event-stream", &sse).await;
        let stream = client_at(addr)
            .chat_tools_stream_with(&[], None, None)
            .await
            .unwrap();
        let items: Vec<ToolStreamItem> = stream.try_collect().await.unwrap();
        server.await.unwrap();
        let [ToolStreamItem::Turn(turn)] = items.as_slice() else {
            panic!("expected one completed turn: {items:?}");
        };
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].arguments, "{\"name\":\"Acme\"}");
    }
}
