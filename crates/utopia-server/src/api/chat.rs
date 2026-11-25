//! Agentic chat: the model calls tools on its own (document search / entity lookup / temporal facts), gathers evidence, then answers.
//! Event sequence: step* (action trace) | sources (citation list, updated incrementally as retrieval runs) | delta* (incremental text) → done | error.
//! When the model does not support tool calling, it automatically falls back to a one-shot RAG injection.

#[path = "chat_finalization.rs"]
mod finalization;

#[path = "chat_generation.rs"]
mod generation;

use generation::{Answer, ProducerEvent};

use super::agent;
use super::rig_model::{self, RigModel};
use crate::live::Frame;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::{Stream, StreamExt};
use rig_agent::agent::{AgentBuilder, MultiTurnStreamItem, StreamingError};
use rig_agent::completion::PromptError;
use rig_agent::tool::server::ToolServer;
use rig_core::completion::{CompletionError, Document};
use rig_core::message::Message;
use rig_core::streaming::{StreamedAssistantContent, StreamedUserContent};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::convert::Infallible;
use utopia_core::models::{ChunkView, Role};
use utopia_core::AppError;
use utopia_llm::tool_result_message;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::retrieval;
use crate::state::AppState;

/// Replays a few already-recognized entities. The cap exists because long conversations pile up dozens, and pasting them all back would spend
/// the context saved elsewhere; sorted by first appearance, since the ones recognized early are usually the protagonists of the conversation.
const KNOWN_ENTITY_LIMIT: usize = 20;

const MAX_HISTORY: usize = 20;
const MAX_ROUNDS: usize = 6;

/// A generation that failed to produce an answer (0004): `code` lets the UI look up the wording in the `err.*` table, while the English sentence is kept for logs,
/// MCP and clients that do not localize. **The server does not send display text**: the `error` frame and a rejected request use
/// the same envelope `{error, code}`, which the frontend reads with one function
struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Failures on the model side, mapped to a code by the error type from `utopia_llm`; anything unrecognized counts as an answer failure
    fn model(err: &anyhow::Error, message: impl Into<String>) -> Self {
        Self::new(model_failure_code(err).unwrap_or("answer_failed"), message)
    }
}

/// Turns failures of the model endpoint into stable codes. The types were already sorted out by `utopia_llm`: billing, rate limits,
/// temporarily unavailable, request not delivered or cut off midway, and other refusals. Here they are only told to the UI
fn model_failure_code(err: &anyhow::Error) -> Option<&'static str> {
    if utopia_llm::out_of_credit(err).is_some() {
        Some("model_out_of_credit")
    } else if utopia_llm::rate_limited(err).is_some() {
        Some("model_rate_limited")
    } else if utopia_llm::unavailable(err).is_some() {
        Some("model_unavailable")
    } else if utopia_llm::is_unreachable(err)
        || err.chain().any(|e| e.is::<utopia_llm::Interrupted>())
    {
        Some("model_unreachable")
    } else if utopia_llm::context_too_long(err).is_some() {
        Some("context_too_long")
    } else if utopia_llm::rejected(err).is_some() {
        Some("model_rejected")
    } else {
        None
    }
}

/// The few sentences the finalization gate (`agent::finalization_error`) uses when it rejects a candidate answer are mapped to codes here.
/// Those sentences cross the `chat_finalization` boundary as text, so they are matched by their original wording; tests take
/// the output of `finalization_error` directly, so changing the wording fails there, rather than quietly dropping the code
fn unpublishable_code(message: &str) -> Option<&'static str> {
    if message.contains("tool-control text") {
        Some("answer_tool_text")
    } else if message.contains("attempted a tool call")
        || message.contains("Tool calls are not answers")
    {
        Some("answer_tool_call")
    } else if message.contains("empty answer") {
        Some("answer_empty")
    } else if message.contains("size limit") {
        Some("answer_too_long")
    } else {
        None
    }
}

/// `remember` was once disabled for a period (see `docs/decisions/0015`): it turned a sentence directly into
/// a live edge in the graph; in practice "remember that Acme moved HQ to Shenzhen" became an edge with an **empty predicate and
/// 0.9 confidence**, and what the assistant claimed was not what the graph held.
///
/// Now the extraction side is wired to `pending_facts`: facts extracted from memory wait for a person's nod before entering the ledger.
/// This switch is kept so that, the next time a tool is found to change the graph silently, there is a place to pull it immediately:
/// better to have no such tool than one that silently changes the graph.
pub(super) const REMEMBER_ENABLED: bool = true;

#[derive(Deserialize)]
pub struct ChatReq {
    /// Default = new conversation (the first `conversation` SSE event returns the id)
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    /// The body of the new question. On a retry it is not read: what is asked is the sentence already stored in the KB
    pub message: String,
    /// Retries the last still-unanswered question in this conversation (#936): not stored again, it is answered in place
    #[serde(default)]
    pub retry_message_id: Option<Uuid>,
}

/// Once the conversation is occupied, retry eligibility is checked, to keep another round from writing an answer between the check and the registration
/// Accepts only this conversation's last unanswered user message; on rejection, neither the history nor the model is touched
///
/// Why retries exist (#936): before, asking again meant resending, which stored the same question twice in the history,
/// and the copy with no answer was still replayed to the model as an "unanswered turn" in every later question
async fn retry_question(
    state: &AppState,
    conversation_id: Uuid,
    question_id: Uuid,
) -> Result<String, AppError> {
    let target =
        utopia_store::conversations::retry_target(&state.pool, conversation_id, question_id)
            .await?
            .ok_or(AppError::NotFound)?;
    if target.role != "user" {
        return Err(AppError::invalid(
            "retry_not_question",
            "Only a question can be answered again",
        ));
    }
    if !target.last {
        return Err(AppError::CodedConflict {
            code: "retry_answered",
            message: "That question already has an answer, or the conversation went on after it"
                .into(),
        });
    }
    Ok(target.content)
}

/// The tool list. **MCP uses this same one** (`mcp.rs`): names, descriptions and parameter schemas copied into
/// two sets would sooner or later diverge, and they are the contract with the `tools.rs` execution side
pub(super) fn tools_schema(can_write: bool, data_source_names: &[String]) -> serde_json::Value {
    let mut tools = base_tools();
    if !data_source_names.is_empty() {
        if let Some(arr) = tools.as_array_mut() {
            arr.push(json!({
                "type": "function",
                "function": {
                    "name": "query_data",
                    "description": format!(
                        "Run a read-only SQL query against a mounted database. Available sources: \
                         {}. Search the source's schema document first (search_chunks) if unsure \
                         of tables/columns. Only a single SELECT/WITH statement is allowed; a \
                         LIMIT is enforced server-side; results come back as JSON lines. If the \
                         query errors, fix the SQL and retry once.",
                        data_source_names.join(", ")
                    ),
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "data_source": {
                                "type": "string",
                                "description": "Name of the mounted data source to query."
                            },
                            "sql": {
                                "type": "string",
                                "description": "One SELECT/WITH statement in the source's own SQL dialect (PostgreSQL, MySQL, ClickHouse, Trino, Databricks or Snowflake; the schema document names the engine)."
                            },
                            "purpose": {
                                "type": "string",
                                "description": "One short phrase: what this query answers (shown to the user)."
                            }
                        },
                        "required": ["data_source", "sql"]
                    }
                }
            }));
        }
    }
    if can_write && REMEMBER_ENABLED {
        if let Some(arr) = tools.as_array_mut() {
            arr.push(json!({
                "type": "function",
                "function": {
                    "name": "remember",
                    "description": "Record one memory episode into the knowledge base's temporal \
                        memory. Use ONLY when the user explicitly asks to remember/record \
                        something, or clearly states a decision or fact to keep. The sentence \
                        is stored immediately; facts extracted from it are PROPOSED and shown \
                        to the user for confirmation before they enter the knowledge graph. \
                        Never claim a fact was added to the graph. Do not use for casual \
                        conversation.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "text": {
                                "type": "string",
                                "description": "The episode to remember, one self-contained \
                                    statement (who/what, with names spelled out)."
                            },
                            "occurred_at": {
                                "type": "string",
                                "description": "Optional date the stated fact took effect \
                                    (YYYY, YYYY-MM, YYYY-MM-DD or RFC3339). Omit to use today."
                            }
                        },
                        "required": ["text"]
                    }
                }
            }));
        }
    }
    tools
}

/// Before executing, check whether the call stated itself clearly.
///
/// **Both kinds of "not stated clearly" used to quietly become a normal call.**
///
/// One: the arguments do not parse. When the model's output hits the token limit, `arguments` is cut off midway,
/// and the JSON string is incomplete. Before, this was `unwrap_or_else(|_| json!({}))`, an empty object,
/// and then `args["query"].as_str().unwrap_or(&query)` in `search_chunks` fell back to
/// **the user's original sentence**, so a truncated call became "search using the user's original question",
/// while the trace showed an entirely normal `search · 6 sources`.
///
/// Two: a required parameter was simply not given. The same fallback, the same result.
///
/// Neither should be guessed. **A fallback produces a wrong answer that looks fine**, which is far worse than an error:
/// an error makes the model retry, while a guessed answer is one no one will check.
///
/// The criterion is taken directly from the `required` in the tool table: add a required parameter and this follows automatically,
/// with no need to remember to change a second place.
pub(super) fn check_call(
    tools: &serde_json::Value,
    name: &str,
    raw_args: &str,
) -> Result<serde_json::Value, (String, serde_json::Value)> {
    // The UI speaks by field (#942): which parameter, and whether it was missing or malformed; a body that does not parse at all has no `param`
    let step = |detail: &str| super::tools::Step::new("tool", name, detail);
    let refuse = |step: super::tools::Step, message: String| (message, step.json());
    let Ok(args) = serde_json::from_str::<serde_json::Value>(raw_args) else {
        return Err(refuse(
            step("bad arguments").status("invalid"),
            format!(
                "The arguments for {name} were not valid JSON, so the call was not run. \
                 They were probably cut off. Call it again with complete arguments."
            ),
        ));
    };
    let function = tools
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["function"]["name"] == name)
        .map(|t| &t["function"]);
    let required = function.and_then(|f| f["parameters"]["required"].as_array());
    for key in required.into_iter().flatten().filter_map(|k| k.as_str()) {
        // Empty string and null both count as not given: `{"query": ""}` retrieves things unrelated to the question,
        // and it too would show as a normal trace
        let missing = match args.get(key) {
            None | Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.trim().is_empty(),
            Some(_) => false,
        };
        if missing {
            return Err(refuse(
                step(&format!("missing {key}")).missing(key),
                format!(
                    "{name} needs `{key}`, and it was missing or empty, so the call was not \
                     run. Call it again with `{key}` set."
                ),
            ));
        }
        // The criterion is also taken from the tool table: parameters declared with `format: uuid` in the schema are also checked at this gate.
        // An invented id reaching the tool can only answer "no such document in this KB", and the model would read that as
        // "really absent from the KB" and give up, which is yet another wrong answer that looks fine
        let is_uuid =
            function.is_some_and(|f| f["parameters"]["properties"][key]["format"] == "uuid");
        if is_uuid
            && args[key]
                .as_str()
                .is_none_or(|s| s.trim().parse::<Uuid>().is_err())
        {
            return Err(refuse(
                step(&format!("invalid {key}")).invalid(key),
                format!(
                    "{name} needs `{key}` to be a uuid returned by another tool, and it was \
                     not, so the call was not run. Look the id up first, then call it again."
                ),
            ));
        }
        let is_string =
            function.is_some_and(|f| f["parameters"]["properties"][key]["type"] == "string");
        if is_string && !args[key].is_string() {
            return Err(refuse(
                step(&format!("invalid {key}")).invalid(key),
                format!(
                    "{name} needs `{key}`, which must be a string, so the call was not run. \
                     Call it again with `{key}` set to a string."
                ),
            ));
        }
    }
    Ok(args)
}

const MEMORY_PROMPT: &str = "\
    Memory: you can persist knowledge with the remember tool. Use it when the user says \
    \"remember/record this\" or states a decision meant to last. In your reply, say that the \
    sentence was recorded and that the facts extracted from it will be shown for the user's \
    confirmation before entering the graph; never say a fact is already in the graph. \
    Never invent memories, and never call it for small talk.";

/// The tools' JSON schema: **the copy the model sees**.
///
/// It stays in `chat.rs` rather than `tools.rs`: it is part of the prompt and follows the conversation policy;
/// `tools.rs` decides only what to do once the arguments are in hand (see the note at the top of that module).
/// MCP reads it too, so it is open to the same module: two copies describing the same tools would inevitably diverge.
pub(super) fn base_tools() -> serde_json::Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "search_chunks",
                "description": "Full-text + semantic search over the knowledge base documents. \
                    Returns numbered source excerpts you can cite as [n], each with the \
                    document_id it came from. Excerpts are cut short and only the best-matching \
                    sections come back, so when the answer may sit elsewhere in a hit, call \
                    get_document with that document_id to read the whole document.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query, phrased in the corpus language." },
                        "as_of": {
                            "type": "string",
                            "description": "Optional RECORD-time moment (YYYY-MM-DD or RFC3339): search only what the knowledge base held at that moment — earlier versions of documents, documents deleted since. Full-text recall stays current, so hits are correct but may be incomplete. Omit for the current base."
                        }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_document",
                "description": "Read ONE knowledge base document, all sections in order, up to \
                    24,000 body characters per call, by the document_id shown in search_chunks \
                    results. If truncated, call again with the offset in the notice to continue. Use it whenever \
                    a search hit looks like the right document but the excerpt does not contain \
                    the answer — action items, decisions and lists usually sit past the excerpt \
                    or in a section that did not match the query.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "document_id": {
                            "type": "string",
                            "format": "uuid",
                            "description": "Document id (uuid) from a search_chunks result line."
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Optional zero-based Unicode character position (default 0) across trimmed section bodies in order, excluding headers and separators. Use the next offset from a truncation notice; at or past the end returns no remaining text."
                        }
                    },
                    "required": ["document_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_docs",
                "description": "Search the Utopia product manual (the \"Utopia Charter\") — how \
                    the platform itself works (ingestion and sync, missing markers and versions, \
                    the graph and review flow, roles, settings) — and never the user's own \
                    documents, which live in search_chunks.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "What to look up in the manual." }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_entities",
                "description": "Look up entities in the knowledge graph by (partial) name. \
                    Returns id, name, type and a disambiguator when several entities share a name.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Entity name or a fragment of it." }
                    },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "entity_facts",
                "description": "Facts about one entity from the bi-temporal knowledge graph: \
                    relations with validity ranges (from → to; 'now' = still ongoing). \
                    The best tool for who/when/history questions. Use after find_entities. \
                    Pass `at` to see the world as of that date (server-side filter) — \
                    always do this for \"who was X in <year/month>\" questions. \
                    Conclusions from business rules and ontology axioms come back too, \
                    marked `[rule: <name>]`; an axiom names its defining property. \
                    The readings that made them true are included — take \
                    those as given rather than re-deriving them from the readings yourself. \
                    Output is grouped by predicate and cut at `limit`; use `offset` to continue. \
                    Narrow with predicate, object_type, since / until, or use timeline / neighbors / paths_between.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "entity_id": { "type": "string", "description": "Entity id (uuid) from find_entities, or an entity name (the server picks the best match and says which)." },
                        "predicate": { "type": "string", "description": "Optional: only facts whose relation name contains this (e.g. 'founder')." },
                        "object_type": { "type": "string", "description": "Optional: only facts whose other end is of this type (e.g. 'Person')." },
                        "since": { "type": "string", "description": "Optional WORLD-time window start: facts still holding after it." },
                        "until": { "type": "string", "description": "Optional WORLD-time window end: facts that had started by it." },
                        "limit": { "type": "integer", "description": "How many facts to return (default 80, max 300); the reply says when it is cut." },
                        "offset": { "type": "integer", "minimum": 0, "description": "Zero-based position in the filtered facts (default 0). Use the reply's next_offset to continue, keeping the entity, filters and time parameters unchanged." },
                        "at": {
                            "type": "string",
                            "description": "Optional as-of moment on the WORLD axis (YYYY, YYYY-MM, \
                                YYYY-MM-DD, or a zoned time). Only facts valid at that moment are \
                                returned. Omit for the full history."
                        },
                        "as_of": {
                            "type": "string",
                            "description": "Optional RECORD-time moment (YYYY-MM-DD or RFC3339): the facts as the knowledge base held them at that moment, before later corrections, retractions and merges. Use for 'what did we think / know / have on record as of <date>'. Independent of `at`: `at` is when something was true, `as_of` is when we believed it. Omit for today's understanding."
                        },
                        "before": {
                            "type": "string",
                            "description": "Optional RECORD-time instant copied exactly from a changes event: the facts as the knowledge base held them strictly before that change landed. Use it for 'before <correction / memo> arrived' — paste the event's timestamp, do not compute an earlier as_of yourself. Overrides as_of."
                        }
                    },
                    "required": ["entity_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "paths_between",
                "description": "How two entities are connected in the knowledge graph: the chains of facts that join them, up to 3 hops, shortest first. THE tool for 'what is the relation between A and B', 'how is X linked to Y', 'who connects A and B'. Both ends take an entity name or an id; with a name the server picks the best match and says which. Pass `at` to require every edge to hold at that moment (world time), `as_of` to read the base as recorded then. Each edge comes with its validity range.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "One end: entity name, or an id from find_entities." },
                        "to": { "type": "string", "description": "The other end: entity name, or an id." },
                        "max_hops": { "type": "integer", "description": "Longest chain to consider, 1-3 (default 3)." },
                        "at": { "type": "string", "description": "Optional WORLD-time moment (YYYY, YYYY-MM, YYYY-MM-DD): every edge on a path must hold then." },
                        "as_of": { "type": "string", "description": "Optional RECORD-time moment: the paths as the base held them then." }
                    },
                    "required": ["from", "to"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "neighbors",
                "description": "The entities linked to one entity, grouped by predicate, one hop. Use it to see what surrounds an entity before deciding where to look next (each further hop is another call); narrow with `predicate` (e.g. 'founder', 'employee') or `object_type` (e.g. 'Person'). Attribute values are not neighbors; entity_facts has them.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "entity": { "type": "string", "description": "Entity name, or an id from find_entities." },
                        "predicate": { "type": "string", "description": "Optional: only relations whose name contains this." },
                        "object_type": { "type": "string", "description": "Optional: only neighbors of this type (name contains)." },
                        "at": { "type": "string", "description": "Optional WORLD-time moment: only links valid then." },
                        "as_of": { "type": "string", "description": "Optional RECORD-time moment." },
                        "limit": { "type": "integer", "description": "How many to return (default 40, max 300)." }
                    },
                    "required": ["entity"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "timeline",
                "description": "One entity's dated facts in world-time order: THE tool for 'the timeline of X', 'the history of X', 'what happened to X between <year> and <year>'. Only facts with a stated date appear; narrow with `since` / `until` (world time) or `predicate`.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "entity": { "type": "string", "description": "Entity name, or an id from find_entities." },
                        "since": { "type": "string", "description": "Optional start of the window (YYYY, YYYY-MM, YYYY-MM-DD)." },
                        "until": { "type": "string", "description": "Optional end of the window." },
                        "predicate": { "type": "string", "description": "Optional: only relations whose name contains this." },
                        "as_of": { "type": "string", "description": "Optional RECORD-time moment: the timeline as the base held it then." },
                        "limit": { "type": "integer", "description": "How many to return (default 60, max 300)." }
                    },
                    "required": ["entity"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_rules",
                "description": "The business rules and declared ontology axioms of this base. \
                    Business rules show what each one concludes and the exact conditions it \
                    tests, thresholds included. Ontology axioms name the defining properties \
                    and the relations they imply, and say when derivation is off for this base. Read the rule to \
                    explain WHY something was concluded, or to answer \"what counts as X here\". \
                    Do not re-implement a rule's comparison yourself; ask entity_facts or \
                    rule_matches for what it actually concluded. rule_matches takes business \
                    rule ids only.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "rule_matches",
                "description": "Which entities a business rule currently marks, with the \
                    readings that made each one true. Use it for \"which wells are gas-bearing\" \
                    style questions — one call instead of checking every entity. \
                    Get the rule id from list_rules.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "rule_id": { "type": "string", "description": "Rule id (uuid) from list_rules." },
                        "limit": { "type": "integer", "description": "How many to return (default 50, max 200)." }
                    },
                    "required": ["rule_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "changes",
                "description": "What the graph LEARNED or REVISED in a window of record time — \
                    the belief axis. Answers \"what changed since X\", \"what did we get wrong\", \
                    \"what is new this quarter\", and needs no entity, so use it when the \
                    question names a period rather than a subject. \
                    Events: asserted (new claim), corrected (a claim replaced by a revised one), \
                    rejected (a claim withdrawn), merged (folded into another claim) — each with \
                    the document it came from. \
                    NOT the same axis as entity_facts(at): that asks \"what was true on date D\"; \
                    this asks \"what did we change our mind about between D1 and D2\". A fact \
                    about 2019 can be recorded in 2026 — this windows on when we recorded it. \
                    Each event starts with its exact UTC RFC3339 record timestamp, including \
                    fractional seconds. \
                    For 'before a correction arrived', find that event here and pass its \
                    timestamp, exactly as printed, to entity_facts as `before`. Keep at for the \
                    world date asked about. Returns up to 40 events; use `offset` to continue.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "since": {
                            "type": "string",
                            "description": "Start of the window (YYYY, YYYY-MM or YYYY-MM-DD), inclusive — a year or month means its first day."
                        },
                        "until": {
                            "type": "string",
                            "description": "End of the window (YYYY, YYYY-MM or YYYY-MM-DD), \
                                inclusive of that whole day, month or year. Omit for 'up to now'."
                        },
                        "entity_id": {
                            "type": "string",
                            "description": "Optional entity id from find_entities, to narrow the \
                                window to changes touching that one entity."
                        },
                        "kinds": {
                            "type": "array",
                            "items": {
                                "type": "string",
                                "enum": ["asserted", "corrected", "rejected", "merged"]
                            },
                            "description": "Optional filter. A freshly ingested corpus is nearly \
                                all 'asserted'; pass [\"corrected\", \"rejected\"] to isolate \
                                the places we actually changed our mind."
                        },
                        "offset": { "type": "integer", "minimum": 0, "description": "Zero-based position in the matching events (default 0). Use the reply's next_offset to continue, keeping the window and filters unchanged." }
                    },
                    "required": ["since"]
                }
            }
        }
    ])
}

const SYSTEM_PROMPT: &str = "You are the assistant of Utopia, a temporal knowledge platform. \
    You have tools: search_chunks (document search) and get_document (paged text of one \
    document found by search), find_entities, entity_facts, neighbors, timeline, \
    paths_between and changes (a bi-temporal knowledge graph), and search_docs (Utopia's \
    own manual, the Charter).\n\
    The knowledge base holds whatever its owners ingested: documents, and a graph extracted \
    from them. You do not know what is in it until you look; public companies, well-known \
    people and events are as likely to be there as private material. A question you could \
    answer from memory is still answered from the base, and \"general knowledge\" is never a \
    reason to skip the tools. Never say the base lacks something you have not searched for.\n\
    search_chunks returns short excerpts of the best-matching sections only. When a hit is \
    clearly the right document but the excerpt does not carry the answer, read the whole \
    document with get_document before saying the knowledge base does not have it. If a \
    document read is truncated, continue with the offset in its notice; a partial read is \
    not the full document and cannot establish that the document lacks the answer.\n\
    The graph has TWO independent time axes:\n\
    - World time — when something was true. entity_facts(`at` = as of that date).\n\
    - Record time — when we came to believe it, and when we revised it. entity_facts and search_chunks take `as_of` = the base as it stood at that moment, before later corrections, retractions and merges; changes lists what moved in a window.\n\
    \"Who was CTO in 2019\" is world time; \"what did we learn last month\" and \"what did \
    we get wrong\" are record time. The same fact has a position on both.\n\
    Boundary: search_docs answers questions about Utopia itself (features, ingestion, \
    permissions, what fields like 'missing' or validity ranges mean); the other tools answer \
    questions about the knowledge stored in it. Never mix the manual into answers about the \
    base's contents unless they asked about Utopia's behavior.\n\
    \n\
    Method:\n\
    First decide what the message is about. A message about THIS CONVERSATION — translate it, \
    say it shorter, rephrase it, \"what did you just say\", \"why\" — is answered from the \
    transcript above with NO tool calls: the evidence is already in it. Gathering it again is \
    not merely wasted work — with several entities sharing a name the second pass can land on \
    a different one, and the \"translation\" then says something else. Just deliver it — no \
    preamble about what you are or are not looking up. Everything below is for messages about \
    the knowledge base.\n\
    1. For factual questions — questions about the knowledge base, never one about this \
       conversation — ALWAYS gather evidence with tools before answering. For how two \
       things are related, call paths_between (names are fine); for the history of one \
       thing, timeline; to see what is linked to it, neighbors; for its facts, entity_facts, \
       narrowed with predicate / object_type / since / until. search_chunks answers content \
       and detail questions. Combine both when useful.\n\
    2. Facts carry validity ranges (from → to). For \"as of <date>\" questions pass `at` to \
       entity_facts and the server filters to that moment; for history questions omit `at` \
       to see the full timeline. For 'what did we know / have on record / believe as of <date>' or 'before <memo> arrived' pass `as_of` — that is the record axis and the ONLY way to answer such a question; do not narrate a plan, call the tool. The two combine: `at` for the date asked about, `as_of` for when. State dates in the answer. Dates in tool output carry their own precision: \
       `2023` means the year and `2023-06` the month — never turn them into a specific day; \
       `undated` marks a fact with no stated start; \
       `ended, date unknown` marks one the text says is over, date not given.\n\
    2a. For 'before a correction or memo arrived', call changes to find its exact record timestamp, \
       then call entity_facts with `before` set to that timestamp, copied exactly as printed — the \
       server reads the base as it stood strictly before that change. Never compute an earlier \
       `as_of` yourself. Keep `at` for the world date asked about.\n\
    2b. For \"what changed / what is new / what did we get wrong since <date>\", call changes — \
       it needs no entity. Name the document a correction came from in plain prose. changes \
       returns no [n] numbers and no URLs, so never write a bracketed citation or a placeholder \
       like [Link] after one of its lines — the document's name IS the attribution.\n\
    3. Several entities can share one name — check the disambiguator and pick the right one; \
       if genuinely ambiguous, ask the user which one they mean.\n\
    4. Stop calling tools as soon as you have enough evidence. Then answer concisely: cite \
       with [n] at the end of supported sentences, using only numbers a tool printed. Search \
       results carry them, and so does each fact line of entity_facts, neighbors, timeline and \
       paths_between: its [n] opens the passage that fact was read from. A fact line without \
       a number (a derived fact, or one whose documents are gone) is stated without a bracket. \
       An end written `superseded by X [m]` was set by the timeline when that later fact \
       began: cite [m] for that end, never the line's own [n]. An end marked `end derived` came \
       from the timeline too. A line marked `start corrected` or `end corrected` had that date \
       changed by a person, and one marked `corrected` had both (the note after the colon is \
       theirs, not a document's). A derived or changed date is not in the passage the line's [n] \
       opens, so never attribute it to that passage; a date the line does not mark still is. \
       If the evidence is insufficient, say so explicitly — never fabricate.\n\
    5. Always respond in the same language as the user's question.";

pub async fn chat(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<ChatReq>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let kb = utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    // Write tools follow the person: only conversations at editor level and above get remember; viewers are strictly read-only
    // Mounted data sources decide whether query_data is in the list (data questions are reads, so viewers may use them too)
    let mounted_sources = utopia_store::datasources::mounted(&state.pool, kb_id).await?;
    // Semantic layer: metric/dimension -> data asset mappings that a person has confirmed go straight into the system prompt,
    // so data questions prefer confirmed definitions rather than guessing from the schema each time.
    //
    // Before, this pulled facts by `confidence >= 0.75`, but that threshold encoded a binary state with a float
    // (proposed 0.6 / confirmed 1.0). Now it reads `status = confirmed` (0011).
    // Definitions are picked below by question (`mapping_index::relevant`); before, `confirmed(kb, 30)` took
    // the first thirty in lexicographic order, so in a KB with a hundred definitions seventy never reached the prompt (#574)
    let can_write = utopia_store::access::kb_role(&state.pool, &user, &kb)
        .await?
        .is_some_and(|r| r >= Role::Editor);

    const NO_MODEL: &str = "Chat model not configured. Go to Settings → Models.";
    let settings = utopia_store::settings::get(&state.pool, kb.workspace_id)
        .await?
        .ok_or_else(|| AppError::invalid("no_chat_model", NO_MODEL))?;
    let client = state
        .chat_clients
        .get(kb.workspace_id, &settings)
        .ok_or_else(|| AppError::invalid("no_chat_model", NO_MODEL))?;

    let query = req.message.trim().to_string();
    if req.retry_message_id.is_none() && query.is_empty() {
        return Err(AppError::Validation("Missing user message".into()).into());
    }
    let conversation_id = match req.conversation_id {
        Some(id) => {
            utopia_store::conversations::require_owned(&state.pool, kb_id, user.id, id).await?;
            id
        }
        None if req.retry_message_id.is_some() => {
            return Err(AppError::invalid(
                "retry_needs_conversation",
                "A retry names the conversation its question is in",
            )
            .into());
        }
        None => utopia_store::conversations::create(&state.pool, kb_id, user.id, &query).await?,
    };
    // Session persistence: the user's message is stored at once, and the context is assembled server-side from the DB; the frontend sends only the new message
    // The question being retried is already in the DB: answer it in place, without storing it again
    // New questions and retries share one entry point: first occupy the conversation, then check retry eligibility or store the question
    // If preparation fails, Drop releases it automatically; a rejected new question does not slip into the history
    let handle = state.live.begin(conversation_id).await?;
    let generation_id = handle.generation_id();
    let cancellation = handle.cancellation();
    let (query, user_message_id) = match req.retry_message_id {
        Some(question_id) => {
            let question = retry_question(&state, conversation_id, question_id).await?;
            (question.trim().to_string(), question_id)
        }
        None => {
            let question_id = utopia_store::conversations::append_message(
                &state.pool,
                conversation_id,
                "user",
                &query,
                &utopia_store::conversations::TurnRecord::empty(),
            )
            .await?;
            (query, question_id)
        }
    };
    if query.is_empty() {
        return Err(AppError::Validation("Missing user message".into()).into());
    }
    let history = utopia_store::conversations::recent_context(
        &state.pool,
        conversation_id,
        MAX_HISTORY as i64,
    )
    .await?;
    // This question is already stored, so it is in the history; the runner also sends it once as the prompt. Both are sent,
    // so each request has two identical user messages, and the entity system message is also wedged between the two copies
    // (since #548). Remove by stored id, not by text: another concurrent question in the same conversation may be queued after it.
    // The wrap-up step uses the same position to remove the current question from the background
    let current = history
        .turn_ids
        .iter()
        .position(|id| *id == user_message_id);
    let workspace_id = kb.workspace_id;
    // Data description (written by exploration from the schema) and conventions (written by people) go into the system prompt too.
    // **Always present, not left to luck in retrieval**: a conventions page relying on retrieval alone reached 14/18, but only because
    // these questions were all about metrics, so each hit its page (#520)
    let data_description = kb.data_description.clone().filter(|s| !s.trim().is_empty());
    let data_conventions = kb.data_conventions.clone().filter(|s| !s.trim().is_empty());

    // The registry is taken out before the generator: the `async_stream!` below moves `state` away entirely
    let live = state.live.clone();
    let save_pool = state.pool.clone();
    let previous_answer = history
        .turns
        .iter()
        .rev()
        .find(|(role, _)| role == "assistant")
        .map(|(_, content)| content.clone())
        .unwrap_or_default();
    let previous_sources = history.last_sources.clone();

    // Generation is not tied to this connection.
    //
    // **Switching away once loses an answer**, and loses it more thoroughly than it looks: the whole generation lives inside the generator below,
    // and the assistant message is stored only when it finishes; when the browser navigates away, axum drops the response body and
    // the generator future is dropped, so the LLM call is cancelled on the spot, and the `append_message` never runs. Measured: when the connection was cut,
    // 1219 bytes of body had already arrived, but twenty seconds later the DB held
    // only the user's row. **That answer was not hidden, it was never finished generating.**
    //
    // So the generator is handed to a separate task to drive, and this connection is downgraded to one subscriber.
    // The task does not disappear with the connection: the answer is written in full and stored as usual, and is there when the person comes back.
    //
    // The cost, stated plainly: **it keeps spending money when no one is watching**. This is deliberate: losing an answer costs more than one extra round,
    // and `MAX_ROUNDS` already caps it. A failed send (the receiver is gone) does not stop it, and that is the point.
    let producer = async_stream::stream! {
        // Send the identity before the preparatory work that depends on the model, so an early Stop can still point at this round
        // The conversation id goes out first (a new conversation is announced to the frontend this way); the id stored for this question goes out with it:
        // if an answer fails midway, the UI retries with it (#936)
        yield ProducerEvent::Progress(Frame::new("conversation", json!({
            "id": conversation_id, "message_id": user_message_id, "generation_id": generation_id,
        }).to_string()));
        // Semantic layer: the confirmed definitions related to this question go into the system prompt; data questions prefer confirmed definitions
        // rather than guessing from the schema each time. Pick by question rather than stuffing them all: the 17/18 upper bound for twenty-seven entries was measured
        // under the thirty-entry cap, and it does not hold for a hundred definitions truncated by lexicographic order (#574)
        // Picking definitions calls the embedding model, so it is placed inside the generator: Stop drops the generator, and this request is cancelled with it (0063)
        // The cost is that it fails later: before, this was a 500 that wrote nothing; now the question is already stored, and the failure is an error frame
        // This round's embedding cache exists from the moment definitions are picked: the question is embedded once here, and then chunk retrieval and entity lookup by name
        // all use the same vector (follow-up to #971). The sink is built below, and the cache is handed over to it
        let mut embeddings = crate::llm_util::EmbedCache::default();
        let mappings = if mounted_sources.is_empty() {
            Vec::new()
        } else {
            match crate::mapping_index::relevant(
                &state, kb_id, workspace_id, &query, crate::mapping_index::DEFINITIONS_IN_PROMPT,
                Some(&mut embeddings),
            ).await {
                Ok(mappings) => mappings,
                Err(error) => {
                    tracing::warn!(%error, "Could not retrieve conversation mappings");
                    yield ProducerEvent::Outcome(Err(Failure::new("mapping_search_failed", "Could not retrieve relevant mappings.")));
                    return;
                }
            }
        };
        let ds_names: Vec<String> = mounted_sources.iter().map(|d| d.name.clone()).collect();
        let tools = tools_schema(can_write, &ds_names);
        let mut system_prompt = if can_write && REMEMBER_ENABLED {
            format!("{SYSTEM_PROMPT}\n{MEMORY_PROMPT}")
        } else {
            SYSTEM_PROMPT.to_string()
        };
        if !ds_names.is_empty() {
            system_prompt.push_str(&format!(
                "\nData: query_data runs read-only SQL (in each source's own dialect) against: {}. \
                 For questions about numbers/metrics, search for the source's schema document \
                 first, then query. State units and the time range you used in the answer.",
                ds_names.join(", ")
            ));
            // Descriptions state what the schema has (granularity, units, code values, the time axis); conventions state what the schema does not have
            // (which rows count, which column is the number). The latter is the half that took data questions from 2/18 to 14/18
            if let Some(d) = &data_description {
                system_prompt.push_str(&format!(
                    "\nAbout the data (written from the schema; states only what the schema says):\n{d}"
                ));
            }
            if let Some(c) = &data_conventions {
                system_prompt.push_str(&format!(
                    "\nConventions stated by the owner of this base — apply them in every query \
                     and every answer (filters, units, which column is the figure):\n{c}"
                ));
            }
            if !mappings.is_empty() {
                system_prompt.push_str(
                    "\nSemantic layer (confirmed definitions — use these instead of guessing from schema):",
                );
                // Before, this dumped the whole JSON in. Now the fields are columns, and only the ones data questions need are spread out:
                // `sql` and `expr` are "how it is computed", `unit` is the
                // unit the answer must carry, and `summary` is a plain sentence for the model
                for m in &mappings {
                    let how = m
                        .sql
                        .as_deref()
                        .or(m.expr.as_deref())
                        .or(m.table_name.as_deref())
                        .unwrap_or("-");
                    let unit = m
                        .unit
                        .as_deref()
                        .map(|u| format!(" [{u}]"))
                        .unwrap_or_default();
                    let note = m
                        .summary
                        .as_deref()
                        .map(|s| format!(" — {s}"))
                        .unwrap_or_default();
                    system_prompt.push_str(&format!(
                        "\n- {} ({}){unit}: {how}{note}",
                        m.concept_name, m.source
                    ));
                }
            }
        }

        // The loop belongs to rig (#546): tools, policy hooks, history, and the entity list are all handed to it;
        // here we only translate its events into frames the frontend recognizes, and store to the DB at the end
        let shared = agent::Shared::new(
            state.clone(),
            kb_id,
            workspace_id,
            mounted_sources.clone(),
            can_write,
            user.id,
            tools,
            settings.chat_model.clone().unwrap_or_default(),
            query.clone(),
        );
        shared.sink.lock().await.embeddings = embeddings;
        let policy = agent::Policy {
            shared: shared.clone(),
            max_rounds: MAX_ROUNDS,
            cancellation,
        };
        let tool_server = ToolServer::new()
            .dynamic_tools(agent::dynamic_tools(&shared))
            .run();
        let prior: Vec<(String, String)> = history
            .turns
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != current)
            .map(|(_, turn)| turn.clone())
            .collect();
        let prior_reasoning: Vec<_> = history.reasoning.iter().enumerate()
            .filter(|(i, _)| Some(*i) != current)
            .map(|(_, text)| text.clone()).collect();
        let context = super::chat_context::Context::with_reasoning(
            prior.clone(), history.last_tool_exchange.clone(),
            prior_reasoning.clone(), client.history_char_budget(),
        );
        let rig_agent = AgentBuilder::new(RigModel::with_context(client.clone(), context.clone()))
            .preamble(&system_prompt)
            // Tool rounds + the final answering round; the (MAX_ROUNDS+1)th request is handed by the hook to the pure answering phase before any I/O
            .default_max_turns(MAX_ROUNDS + 1)
            .add_hook(policy)
            .tool_server_handle(tool_server)
            .build();
        let mut runner = rig_agent
            .runner(Message::user(query.clone()))
            .history(agent::history_messages_with_reasoning(
                &prior, &history.last_tool_exchange, &prior_reasoning,
            ));
        // Placed after the history and before the current question: the position is the obedience, for the same reason as known_block in extraction
        // Placed right next to the body, for the same reason (role and position are set by `rig_model::wire`)
        if let Some(block) = agent::known_entities_block(&history.entities, KNOWN_ENTITY_LIMIT) {
            runner = runner.document(Document {
                id: "known_entities".into(),
                text: block,
                additional_props: Default::default(),
            });
        }
        let mut run = runner.stream().await;

        // Accumulates for storage: the assistant's full text and the action trace (for history replay)
        let mut answer_acc = String::new();
        let mut steps_acc: Vec<serde_json::Value> = Vec::new();
        // This round's tool round trip, kept as the protocol has it and stored: the next round replays it, so the model knows what it has done
        let mut exchange_acc: Vec<serde_json::Value> = Vec::new();
        // What the model said in this turn and the calls it sent; once the turn's results arrive, they are gathered into one assistant message
        let mut turn_text = String::new();
        let mut turn_reasoning: Option<String> = None;
        let mut turn_calls: Vec<serde_json::Value> = Vec::new();
        // How much of this turn's body has been sent; the rest is held back, since it may be a tool call written as body text (#937)
        let mut turn_published = 0usize;
        let mut turn_holding = false;
        let mut finished = false;
        let mut published_sources: Vec<serde_json::Value> = Vec::new();
        let mut answer_requested = false;

        while let Some(item) = run.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning {
                    reasoning, ..
                })) => {
                    for block in reasoning.content {
                        if let rig_core::message::ReasoningContent::Text { text, .. } = block {
                            turn_reasoning.get_or_insert_default().push_str(&text);
                        }
                    }
                }
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(t))) => {
                    // Tool-round narration stays live, line by line; a line that is or
                    // may become a tool call written as text is held (#937). Withhold the
                    // final call whole: validation after streaming cannot retract
                    // protocol garbage.
                    if shared.finalizing() {
                        if turn_text.len().saturating_add(t.text.len()) > agent::MAX_FINAL_ANSWER_BYTES {
                            yield ProducerEvent::Outcome(Err(Failure::new("answer_too_long", "Model final answer exceeded the size limit")));
                            return;
                        }
                        turn_text.push_str(&t.text);
                    } else {
                        turn_text.push_str(&t.text);
                        let upto = agent::publishable(&turn_text, turn_published, &mut turn_holding);
                        if upto > turn_published {
                            let fresh = turn_text[turn_published..upto].to_string();
                            turn_published = upto;
                            answer_acc.push_str(&fresh);
                            yield ProducerEvent::Progress(delta_event(&fresh));
                        }
                    }
                }
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                    tool_call, ..
                })) => {
                    turn_calls.push(json!({
                        "id": tool_call.id.as_str(),
                        "type": "function",
                        "function": {
                            "name": tool_call.function.name,
                            "arguments": rig_model::args_string(&tool_call.function.arguments),
                        }
                    }));
                }
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                })) => {
                    if !turn_calls.is_empty() {
                        // This turn really called tools. If held back and the marker is dropped, it is also not replayed to the model;
                        // if not (e.g. an example inside a fence), it is sent after, before this step, so `at` lines up
                        if turn_published < turn_text.len() {
                            if agent::tool_call_text(&turn_text, &query) {
                                turn_text.truncate(turn_published);
                            } else {
                                let rest = turn_text[turn_published..].to_string();
                                answer_acc.push_str(&rest);
                                yield ProducerEvent::Progress(delta_event(&rest));
                            }
                        }
                        // The tool round carried narration text: a paragraph break is inserted before the body of the following round
                        if !turn_text.is_empty() {
                            answer_acc.push_str("\n\n");
                            yield ProducerEvent::Progress(delta_event("\n\n"));
                        }
                        let mut recorded = json!({
                            "role": "assistant",
                            "content": if turn_text.is_empty() {
                                serde_json::Value::Null
                            } else {
                                serde_json::Value::String(turn_text.clone())
                            },
                            "tool_calls": std::mem::take(&mut turn_calls),
                        });
                        if let Some(reasoning) = turn_reasoning.take() {
                            recorded["reasoning_content"] = json!(reasoning);
                        }
                        exchange_acc.push(recorded);
                        turn_text.clear();
                        turn_published = 0;
                        turn_holding = false;
                    }
                    let text = rig_model::tool_result_text(&tool_result.content);
                    // Gate tools leave no trace: an entry "claims no lookup needed" under "hello" is noise
                    let mut is_error = None;
                    let mut published_step = None;
                    if tool_result.name != agent::NO_EVIDENCE_TOOL {
                        let mut step = match shared.take_step(&internal_call_id) {
                            Some((step, failed)) => { is_error = Some(failed); step }
                            None => super::tools::Step::new("tool", tool_result.name.as_str(), "unknown").status("not_found").json(),
                        };
                        // **Where in the body this step happened.**
                        //
                        // The model speaks and calls in turns: say a sentence, look something up, say another. On SSE, `delta` and
                        // `step` are already sent alternately, so the order need not be recorded separately; but **history replay does not have
                        // that timeline**: what is stored is only the assembled body as a whole and a flat steps array,
                        // so reopening a conversation stacks all the calls at the very front of the body, reading as if
                        // it had looked up seven things and then said everything at once. Recording the offsets lets replay break the text up again.
                        //
                        // The unit is **UTF-16 code units**, because the split happens in the browser, and JS's
                        // `String.prototype.length` counts exactly those. Byte counts or `chars()`
                        // would cut in the wrong place on Chinese text and emoji
                        if let Some(obj) = step.as_object_mut() {
                            obj.insert("at".into(), json!(answer_acc.encode_utf16().count()));
                        }
                        steps_acc.push(step.clone());
                        published_step = Some(step);
                    }
                    // cite() appends, and a graph citation can add a quote to an entry a search
                    // registered (#968's follow-up): publish when the list changed at all, not
                    // only when it grew. Document reads can add citations too, regardless of the
                    // UI step kind. Release the sink before yielding to subscribers.
                    let sources = {
                        let sink = shared.sink.lock().await;
                        if sink.sources != published_sources {
                            published_sources = sink.sources.clone();
                            Some(sink.sources.clone())
                        } else {
                            None
                        }
                    };
                    let mut recorded = tool_result_message(tool_result.call.as_str(), &text);
                    if let Some(failed) = is_error { recorded["is_error"] = json!(failed); }
                    exchange_acc.push(recorded);
                    // Keep the history of completed tools first, then send the steps to a client that may press Stop right away.
                    // When saving a stopped answer, the generation driver removes parallel calls that have not finished.
                    let resolved = shared.sink.lock().await.resolved.clone();
                    yield ProducerEvent::Context {
                        resolved,
                        tool_exchange: exchange_acc.clone(),
                        gathered: !steps_acc.is_empty(),
                    };
                    if let Some(step) = published_step {
                        yield ProducerEvent::Progress(Frame::new("step", step.to_string()));
                    }
                    if let Some(sources) = sources {
                        yield ProducerEvent::Progress(Frame::new("sources", serde_json::to_string(&sources).unwrap_or_else(|_| "[]".into())));
                    }
                }
                // A hook pushed this turn back. Text already streamed to the user cannot be taken back, so the following body
                // starts a new paragraph; anything still held back (tool calls written as body text) is dropped together with the turn
                Ok(MultiTurnStreamItem::ModelTurnRetried { .. }) => {
                    if turn_published > 0 {
                        answer_acc.push_str("\n\n");
                        yield ProducerEvent::Progress(delta_event("\n\n"));
                    }
                    turn_text.clear();
                    turn_reasoning = None;
                    turn_calls.clear();
                    turn_published = 0;
                    turn_holding = false;
                }
                Ok(MultiTurnStreamItem::FinalResponse(_)) => finished = true,
                Ok(_) => {}
                Err(e) => {
                    let (failure, rejected) = describe(&e);
                    if matches!(&e, StreamingError::Prompt(pe) if matches!(pe.as_ref(), PromptError::PromptCancelled { .. }))
                        && shared.take_answer_request() {
                        answer_requested = true;
                        break;
                    }
                    // **Only "the endpoint rejected a request with tools" degrades** to one-shot RAG. Before, any error in the first
                    // went down this path: a network blip to SiliconFlow was recorded as
                    // "tool-calling unavailable", and then RAG died on the same blip
                    if rejected && answer_acc.is_empty() && steps_acc.is_empty() {
                        tracing::warn!(error = %failure.message, "endpoint rejected tool calls, falling back to one-shot RAG");
                        let mut legacy = std::pin::pin!(legacy_rag(
                            state.clone(),
                            kb_id,
                            workspace_id,
                            query.clone(),
                            context.clone(),
                            client.clone(),
                        ));
                        while let Some(frame) = legacy.next().await {
                            yield frame;
                        }
                        return;
                    }
                    yield ProducerEvent::Outcome(Err(failure));
                    return;
                }
            }
        }
        // Drop the cancelled runner before the reserved, tool-free answer call.
        drop(run);
        if answer_requested {
            let (sources, resolved) = {
                let sink = shared.sink.lock().await;
                (sink.sources.clone(), sink.resolved.clone())
            };
            let (bounded_history, bounded_exchange) = context.snapshot();
            let input = finalization::AnswerContext {
                question: &query, history: &bounded_history, current: None,
                prior_exchange: &bounded_exchange,
                exchange: &exchange_acc, sources: &sources, resolved: &resolved,
            };
            match finalization::answer_with_context(&client, input, &context).await {
                Ok(answer) => {
                    turn_text = answer.content.unwrap_or_default();
                    turn_reasoning = answer.reasoning_content;
                    turn_calls.clear();
                    finished = true;
                }
                Err(e) => {
                    let message = format!("Model could not produce a final answer: {e}");
                    let code = model_failure_code(&e)
                        .or_else(|| unpublishable_code(&message))
                        .unwrap_or("answer_failed");
                    yield ProducerEvent::Outcome(Err(Failure::new(code, message)));
                    return;
                }
            }
        }
        if !finished {
            yield ProducerEvent::Outcome(Err(Failure::new("answer_failed", "LLM stream ended unexpectedly")));
            return;
        }
        // Check the terminal candidate, not earlier narration. The hook is the
        // policy boundary; this is the last guard before publication and storage.
        if shared.finalizing() {
            if let Some(reason) = agent::finalization_error(&turn_text, !turn_calls.is_empty(), &query) {
                yield ProducerEvent::Outcome(Err(Failure::new(unpublishable_code(reason).unwrap_or("answer_failed"), reason)));
                return;
            }
            answer_acc.push_str(&turn_text);
        } else {
            // The final plain-text round still holds something back. It is a tool call written as body text, and a hook has already pushed back
            // once; this is the second, so error out and do not store (#937). If not (e.g. an example inside a fence), send it
            if turn_published < turn_text.len() {
                if agent::tool_call_text(&turn_text, &query) {
                    yield ProducerEvent::Outcome(Err(Failure::new("answer_tool_text", agent::CONTROL_TEXT)));
                    return;
                }
                let rest = turn_text[turn_published..].to_string();
                answer_acc.push_str(&rest);
                yield ProducerEvent::Progress(delta_event(&rest));
            }
            if turn_text.trim().is_empty() {
                yield ProducerEvent::Outcome(Err(Failure::new("answer_empty", "Model returned an empty answer")));
                return;
            }
        }
        let (mut sources, resolved) = {
            let sink = shared.sink.lock().await;
            (sink.sources.clone(), sink.resolved.clone())
        };
        // A round with no lookups (translate, shorten it) copies the [n] of the previous answer. Keep those numbers
        // with their sources from the previous answer, so the superscripts can be opened (#943); a round that looked things up accepts only what it found itself
        if !shared.finalizing() && steps_acc.is_empty() && sources.is_empty() {
            let previous = history
                .turns
                .iter()
                .rev()
                .find(|(role, _)| role == "assistant")
                .map(|(_, content)| content.as_str())
                .unwrap_or_default();
            sources = carried_sources(&answer_acc, previous, &history.last_sources);
        }
        exchange_acc.extend(terminal_reasoning(turn_reasoning));
        yield ProducerEvent::Outcome(Ok(Answer {
            content: answer_acc,
            record: utopia_store::conversations::TurnRecord {
                stopped: false,
                steps: serde_json::Value::Array(steps_acc),
                sources: serde_json::Value::Array(sources),
                resolved: serde_json::Value::Array(resolved),
                tool_exchange: serde_json::Value::Array(exchange_acc),
            },
            final_text: shared.finalizing().then_some(turn_text),
        }));
    };

    // The generation is registered, then **this connection too only goes to "attach" to it** — the same code path as the reconnect after
    // a refresh. If the two paths were written separately, sooner or later only one of them would be right
    handle.start().await;
    let attached = live.attach(conversation_id).await;
    tokio::spawn(generation::run(
        save_pool,
        conversation_id,
        handle,
        producer,
        previous_answer,
        previous_sources,
    ));

    Ok(sse_from(attached))
}

/// Turns an rig error into a failure (code and English sentence), plus "did the endpoint reject tool calls".
/// Our own error chain (rate limits, billing, rejections) is recovered from `rig_model`; the text is the same as before
fn describe(err: &StreamingError) -> (Failure, bool) {
    fn completion(ce: &CompletionError) -> (Failure, bool) {
        match rig_model::llm_failure(ce) {
            Some(ours) => (
                Failure::model(ours, ours.to_string()),
                rig_model::tool_calling_rejected(ce),
            ),
            None => (Failure::new("answer_failed", ce.to_string()), false),
        }
    }
    match err {
        StreamingError::Completion(ce) => completion(ce),
        StreamingError::Prompt(pe) => match pe.as_ref() {
            PromptError::CompletionError(ce) => completion(ce),
            PromptError::PromptCancelled { reason, .. } => {
                (Failure::new("answer_failed", reason.clone()), false)
            }
            other => (Failure::new("answer_failed", other.to_string()), false),
        },
    }
}

// DeepSeek also needs a completed answer's reasoning on later tool requests.
// Keep it in the internal exchange metadata, apart from the published answer.
fn terminal_reasoning(reasoning: Option<String>) -> Vec<serde_json::Value> {
    reasoning
        .into_iter()
        .map(|text| json!({"role":"assistant", "reasoning_content":text}))
        .collect()
}

/// Degraded path: one-shot RAG injection when the endpoint does not support tool calls.
/// Retrieves once, puts the sources into the system prompt, answers with streaming, and stores
fn legacy_rag(
    state: AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: String,
    context: super::chat_context::Context,
    client: utopia_llm::LlmClient,
) -> impl Stream<Item = ProducerEvent> {
    async_stream::stream! {
        // This fallback path runs only once per round, and the question embeds itself: it has no sink for this round (the #971 cache lives on that)
        let chunks = match retrieval::hybrid(&state, kb_id, workspace_id, &query, 8, None, None).await {
            Ok(chunks) => chunks,
            Err(error) => {
                tracing::warn!(%error, "fallback document retrieval failed");
                yield ProducerEvent::Outcome(Err(Failure::new("search_failed", "Could not search the documents.")));
                return;
            }
        };
        let legacy_sources: Vec<serde_json::Value> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| source_json(i + 1, c))
            .collect();
        yield ProducerEvent::Progress(Frame::new(
            "sources",
            serde_json::to_string(&legacy_sources).unwrap_or_else(|_| "[]".into()),
        ));
        let mut answer_acc = String::new();
        let mut reasoning = None;
        let deltas = loop {
            let (turns, _) = context.snapshot();
            let mut lmsgs = vec![json!({ "role": "system", "content": legacy_system_prompt(&chunks) })];
            for (role, content) in &turns {
                lmsgs.push(json!({ "role": role, "content": content }));
            }
            lmsgs.push(json!({"role":"user", "content":query}));
            match client.chat_tools_stream_with(&lmsgs, None, None).await {
                Err(e) if context.recover(&client, &e) => continue,
                result => break result,
            }
        };
        match deltas {
            Ok(deltas) => {
                let mut deltas = std::pin::pin!(deltas);
                while let Some(item) = deltas.next().await {
                    match item {
                        Ok(utopia_llm::ToolStreamItem::Delta(text)) => { answer_acc.push_str(&text); yield ProducerEvent::Progress(delta_event(&text)); }
                        Ok(utopia_llm::ToolStreamItem::Turn(turn)) => reasoning = turn.reasoning_content,
                        Err(e) => { yield ProducerEvent::Outcome(Err(Failure::model(&e, e.to_string()))); return; }
                    }
                }
                if answer_acc.trim().is_empty() {
                    yield ProducerEvent::Outcome(Err(Failure::new("answer_empty", "Model returned an empty answer")));
                    return;
                }
                yield ProducerEvent::Outcome(Ok(Answer {
                    content: answer_acc,
                    record: utopia_store::conversations::TurnRecord {
                        sources: serde_json::Value::Array(legacy_sources),
                        tool_exchange: json!(terminal_reasoning(reasoning)),
                        ..utopia_store::conversations::TurnRecord::empty()
                    },
                    final_text: None,
                }));
            }
            Err(e) => yield ProducerEvent::Outcome(Err(Failure::model(&e, e.to_string()))),
        }
    }
}

/// Turns one "attach" into SSE: first sends a snapshot, then carries on with the increments as usual.
///
/// `None` = this conversation has no generation running. Replies with `idle` rather than 404: **a client
/// asks every time it opens a conversation**, and "nothing running" is the most common answer, not an error
fn sse_from(
    attached: Option<(
        crate::live::Snapshot,
        tokio::sync::broadcast::Receiver<Frame>,
    )>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let Some((snapshot, mut rx)) = attached else {
            yield to_event(&Frame::new("idle", "{}".into()));
            return;
        };
        yield to_event(&snapshot.to_frame());
        if let Some(terminal) = snapshot.terminal() {
            yield to_event(&terminal);
            return;
        }
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let done = frame.event == "done" || frame.event == "error";
                    yield to_event(&frame);
                    if done { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    yield to_event(&error_event("stream_ended", "Answer stream ended unexpectedly"));
                    return;
                }
                // This client read too slowly and was dropped by the broadcast buffer. **Say so**:
                // carrying on silently would leave it missing a middle stretch without knowing
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    yield to_event(&error_event(
                        "stream_lagged",
                        &format!("Fell behind the stream by {n} messages; reopen the conversation"),
                    ));
                    return;
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn to_event(frame: &Frame) -> Result<Event, Infallible> {
    Ok(Event::default().event(frame.event).data(&frame.data))
}

/// Reattach to a generation that is running (this path is used after a page refresh).
pub async fn reattach(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, conversation_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    // **Ownership must be checked.** Conversation ids can be guessed, and this stream would read another person's answer back word for word
    utopia_store::conversations::require_owned(&state.pool, kb_id, user.id, conversation_id)
        .await?;
    Ok(sse_from(state.live.attach(conversation_id).await))
}

#[derive(Deserialize)]
pub struct StopReq {
    pub generation_id: Uuid,
}

/// Notifies only the generation's holder, which saves the partial answer and sends the terminal state.
pub async fn stop(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, conversation_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<StopReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    utopia_store::conversations::require_owned(&state.pool, kb_id, user.id, conversation_id)
        .await?;
    state.live.stop(conversation_id, req.generation_id).await;
    Ok(Json(json!({ "ok": true })))
}

/// The generator yields `Frame`, not `axum`'s `Event`.
/// **Both the broadcast and the snapshot must read the event's content back**, and `Event` cannot be read back (see `live`)
fn delta_event(text: &str) -> Frame {
    Frame::new(
        "delta",
        serde_json::to_string(&json!({ "text": text })).unwrap_or_default(),
    )
}

/// Same envelope as a rejected request: `error` is the English sentence, `code` lets the UI look up the wording (0004)
fn error_event(code: &str, message: &str) -> Frame {
    Frame::new(
        "error",
        json!({ "error": message, "code": code }).to_string(),
    )
}

/// Citation numbers in the body, in the same shape the UI's `citeRe` uses to draw superscripts: `[1]`, `[1][2]`, `[1, 2]`, and the same with a full-width comma
/// inside the brackets only digits and separators appear, whitespace is allowed on either side of a separator, and 0 is not a number
fn cited_numbers(text: &str) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for (at, _) in text.match_indices('[') {
        let rest = &text[at + 1..];
        let Some(close) = rest.find(']') else {
            continue;
        };
        let parts: Vec<&str> = rest[..close].split([',', '，']).collect();
        let mut nums = Vec::new();
        for (k, part) in parts.iter().enumerate() {
            let mut digits = *part;
            if k > 0 {
                digits = digits.trim_start();
            }
            if k + 1 < parts.len() {
                digits = digits.trim_end();
            }
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                nums.clear();
                break;
            }
            match digits.parse::<u64>() {
                Ok(n) => nums.push(n),
                Err(_) => {
                    nums.clear();
                    break;
                }
            }
        }
        out.extend(nums.into_iter().filter(|n| *n > 0));
    }
    out
}

/// A round with no lookups carries over the sources of the previous answer (#943). Only given when **every** number cited in this round
/// was cited in the previous answer: then it takes the cited ones from the sources stored with the previous answer, numbers unchanged; otherwise none are given,
/// and it does not guess which round those numbers originally belonged to
fn carried_sources(
    answer: &str,
    previous_answer: &str,
    previous_sources: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let cited = cited_numbers(answer);
    if cited.is_empty() || !cited.is_subset(&cited_numbers(previous_answer)) {
        return Vec::new();
    }
    previous_sources
        .iter()
        .filter(|s| s["n"].as_u64().is_some_and(|n| cited.contains(&n)))
        .cloned()
        .collect()
}

fn source_json(n: usize, c: &ChunkView) -> serde_json::Value {
    json!({
        "n": n,
        "chunk_id": c.id,
        "document_id": c.document_id,
        "filename": c.filename,
        "excerpt": truncate(&c.text, 160),
    })
}

/// System prompt for the degraded path (one-shot injection when tool calling is unavailable).
fn legacy_system_prompt(chunks: &[ChunkView]) -> String {
    if chunks.is_empty() {
        return "You are an enterprise knowledge base assistant. No relevant sources were \
                retrieved for this question. Tell the user the knowledge base lacks material \
                on this topic, answer cautiously from general knowledge, and clearly separate \
                sourced statements from speculation. Always respond in the same language as \
                the user's question."
            .to_string();
    }
    let mut prompt = String::from(
        "You are an enterprise knowledge base assistant. Answer strictly based on the \
         numbered sources below. When a source supports a statement, append its citation \
         number, e.g. [1] or [2], at the end of the sentence. If the sources are \
         insufficient, say so explicitly — never fabricate. Always respond in the same \
         language as the user's question.\n\n### Sources\n",
    );
    for (i, c) in chunks.iter().enumerate() {
        prompt.push_str(&format!(
            "\n[{}] \"{}\" section {}:\n{}\n",
            i + 1,
            c.filename,
            c.seq + 1,
            c.text
        ));
    }
    prompt
}

fn truncate(text: &str, max_chars: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max_chars {
        t.to_string()
    } else {
        let cut: String = t.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// Conversation management (list / replay / delete)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct ConversationsQuery {
    /// Searches two places, the title and the message body: people tend to remember the sentence they asked, not the title
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

pub async fn list_conversations(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<ConversationsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    let (conversations, total) = utopia_store::conversations::list(
        &state.pool,
        kb_id,
        user.id,
        q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        q.limit.unwrap_or(30).clamp(1, 100),
        q.offset.unwrap_or(0).max(0),
    )
    .await?;
    Ok(Json(
        json!({ "conversations": conversations, "total": total }),
    ))
}

#[derive(serde::Deserialize)]
pub struct RenameConversationReq {
    pub title: String,
}

/// Rename a conversation.
///
/// **Titles are automatically taken from the first sentence**, and a conversation drifting off course is normal; renaming lets people
/// find it again in the way they remember it.
pub async fn rename_conversation(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, conversation_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<RenameConversationReq>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    utopia_store::conversations::rename(&state.pool, kb_id, user.id, conversation_id, &req.title)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

/// History replay: messages include the stored action trace (steps) and citations (sources).
pub async fn conversation_detail(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, conversation_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    utopia_store::conversations::require_owned(&state.pool, kb_id, user.id, conversation_id)
        .await?;
    let messages = utopia_store::conversations::messages(&state.pool, conversation_id).await?;
    Ok(Json(json!({ "messages": messages })))
}

pub async fn delete_conversation(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path((kb_id, conversation_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    utopia_store::conversations::delete(&state.pool, kb_id, user.id, conversation_id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- check_call ---------------------------------------------------------

    #[test]
    fn required_strings_reject_other_json_types_without_coercion() {
        let tools = tools_schema(true, &[]);
        for (name, key) in [("search_chunks", "query"), ("remember", "text")] {
            for value in [
                json!(123),
                json!(false),
                json!(["pressure"]),
                json!({"text":"pressure"}),
            ] {
                let args = json!({key: value});
                let err = check_call(&tools, name, &args.to_string())
                    .expect_err("a required string must be a string");
                assert_eq!(err.1["detail"], format!("invalid {key}"));
                assert!(err.0.contains("must be a string"));
            }
        }
        for text in ["pressure", "  压力  "] {
            let args = json!({"query":text,"limit":5});
            assert_eq!(
                check_call(&tools, "search_chunks", &args.to_string()).unwrap(),
                args
            );
        }
        // Keep the existing missing/UUID error precedence, even for a wrong type.
        assert!(check_call(&tools, "get_document", r#"{"document_id":123}"#)
            .unwrap_err()
            .0
            .contains("uuid"));
        assert_eq!(
            check_call(&tools, "search_chunks", r#"{"query":null}"#)
                .unwrap_err()
                .1["detail"],
            "missing query"
        );
        let custom = json!([{"function":{"name":"typed_control","parameters":{
            "required":["count","items"],"properties":{"count":{"type":"integer"},"items":{"type":"array"},"optional":{"type":"string"}}
        }}}]);
        let args = json!({"count":3,"items":["a"],"optional":false});
        assert_eq!(
            check_call(&custom, "typed_control", &args.to_string()).unwrap(),
            args
        );
        assert_eq!(
            check_call(&tools, "unknown_tool", &args.to_string()).unwrap(),
            args
        );
    }

    /// Arguments cut off midway: this is what it looks like when the model hits its token limit.
    ///
    /// **Before, this fell back to an empty object, and then `search_chunks` searched with the user's sentence.**
    /// The result was a `search · 6 sources` that looked entirely normal, and an answer based on the wrong input.
    /// No one checks an answer that looks normal, so this case must be a refusal
    #[test]
    fn arguments_cut_off_mid_json_do_not_become_a_search() {
        let tools = tools_schema(false, &[]);
        let err = check_call(&tools, "search_chunks", "{\"query\": \"Acme reven")
            .expect_err("残缺 JSON 必须拒绝，而不是回落");
        assert_eq!(
            err.1["kind"], "tool",
            "the trace must show it as a call that did not go through"
        );
        assert_eq!(err.1["detail"], "bad arguments");
        assert!(
            err.0.contains("not valid JSON") && err.0.contains("again"),
            "回给模型的话要说清没执行、并让它重来：{}",
            err.0
        );
    }

    /// An empty string and a missing field are the same thing: `{"query": ""}` retrieves things unrelated to the question,
    /// and it too would show as a normal trace
    #[test]
    fn an_empty_required_argument_counts_as_missing() {
        let tools = tools_schema(false, &[]);
        for raw in [
            "{}",
            "{\"query\": \"\"}",
            "{\"query\": \"   \"}",
            "{\"query\": null}",
        ] {
            let Err(err) = check_call(&tools, "search_chunks", raw) else {
                panic!("{raw} should be rejected");
            };
            assert_eq!(err.1["detail"], "missing query", "{raw}");
        }
    }

    /// The refused step is also reported by field (#942): which parameter, and whether it was missing or malformed. The parameter name is data,
    /// and the UI writes it as-is; a body that does not parse at all has no `param`. The stored English detail is not changed by a single character
    #[test]
    fn a_refused_call_names_its_parameter_for_the_interface() {
        let tools = tools_schema(false, &[]);
        let missing = check_call(&tools, "search_chunks", "{}").unwrap_err().1;
        assert_eq!(missing["status"], "invalid", "{missing}");
        assert_eq!(missing["param"], "query", "{missing}");
        assert_eq!(missing["missing"], true, "{missing}");
        assert_eq!(missing["detail"], "missing query");
        let wrong = check_call(&tools, "get_document", "{\"document_id\": \"notes.txt\"}")
            .unwrap_err()
            .1;
        assert_eq!(wrong["status"], "invalid", "{wrong}");
        assert_eq!(wrong["param"], "document_id", "{wrong}");
        assert!(wrong.get("missing").is_none(), "{wrong}");
        let cut = check_call(&tools, "search_chunks", "{\"query\": \"Acme")
            .unwrap_err()
            .1;
        assert_eq!(cut["status"], "invalid", "{cut}");
        assert!(cut.get("param").is_none(), "{cut}");
        assert_eq!(cut["detail"], "bad arguments");
    }

    /// **The criterion is taken from the tool table itself.** This guards against "a required parameter was added but the validation was forgotten" —
    /// query_data appears in the table only when a data source is mounted, and its two required parameters
    /// have never been written out separately anywhere else
    #[test]
    fn the_schema_is_the_only_place_required_is_written_down() {
        let tools = tools_schema(false, &["warehouse".into()]);
        let err = check_call(&tools, "query_data", "{\"data_source\": \"warehouse\"}")
            .expect_err("缺 sql 必须拒绝");
        assert_eq!(err.1["detail"], "missing sql");
        check_call(
            &tools,
            "query_data",
            "{\"data_source\": \"warehouse\", \"sql\": \"SELECT 1\"}",
        )
        .expect("both given should pass");
    }

    /// A compliant call passes through unchanged, **with not one optional parameter dropped**.
    ///
    /// This gate only judges whether the call "stated itself clearly", and does no filtering: if the args were reassembled,
    /// adding an optional parameter would mean remembering to add it here as well, and forgetting it would silently disable it
    #[test]
    fn a_well_formed_call_passes_through_untouched() {
        let tools = tools_schema(false, &[]);
        let args = check_call(
            &tools,
            "entity_facts",
            "{\"entity_id\": \"1f8ac10b-58cc-4372-a567-0e02b2c3d479\", \"at\": \"2026-03-15\"}",
        )
        .expect("required value given should pass");
        assert_eq!(
            args["at"], "2026-03-15",
            "optional parameter must not be swallowed at this check"
        );
    }

    /// Full text can only be read by id, and the id is something the model copied from the retrieval results: a wrong copy or a missed one
    /// must be stopped here
    #[test]
    fn get_document_without_an_id_does_not_run() {
        let tools = tools_schema(false, &[]);
        let err = check_call(&tools, "get_document", "{}").expect_err("缺 document_id 必须拒绝");
        assert_eq!(err.1["detail"], "missing document_id");
    }

    /// A non-uuid id reaching the tool can only answer "no such document in this KB" — the model would read that as
    /// "really absent from the KB" and stop, so a mistyped id becomes a negative answer
    #[test]
    fn a_document_id_that_is_not_a_uuid_does_not_run() {
        let tools = tools_schema(false, &[]);
        for raw in [
            "{\"document_id\": \"Notes- 'Scrum' 3 Sept 2026.txt\"}",
            "{\"document_id\": \"1f8ac10b-58cc-4372\"}",
        ] {
            let Err(err) = check_call(&tools, "get_document", raw) else {
                panic!("{raw} should be rejected");
            };
            assert_eq!(err.1["detail"], "invalid document_id", "{raw}");
        }
        check_call(
            &tools,
            "get_document",
            "{\"document_id\": \"1f8ac10b-58cc-4372-a567-0e02b2c3d479\"}",
        )
        .expect("a real uuid should pass");
    }

    /// `changes` already refused a missing `since` in its own branch — **it is the only one that got it right**.
    /// This pins two things: the new unified gate agrees with it, and its own check on the date format
    /// (such as `2026-13-45`) must still stay, because check_call only checks presence, not correctness
    #[test]
    fn the_one_tool_that_already_refused_still_refuses() {
        let tools = tools_schema(false, &[]);
        assert!(check_call(&tools, "changes", "{}").is_err());
        check_call(&tools, "changes", "{\"since\": \"2026-13-45\"}")
            .expect("a malformed date is not this check's job, leave it to changes_window");
    }

    /// Descriptions are written for the model to read; they are sent with every round, and MCP clients print the tool list too:
    /// there must be no trailing spaces. Before, the descriptions of `list_rules`, `rule_matches` and `changes` each had
    /// twenty-odd spaces at the line-wrap points: the `\` wrap was joined into one line, and the next line's indentation stayed in the string.
    /// This covers everything the conversation and MCP see: including `remember` and `query_data`
    #[test]
    fn every_tool_description_is_written_with_single_spaces() {
        fn spaced(v: &serde_json::Value, at: &str, found: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (key, v) in map {
                        if key == "description" && v.as_str().is_some_and(|t| t.contains("  ")) {
                            found.push(format!("{at}: {v}"));
                        }
                        spaced(v, &format!("{at}/{key}"), found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, v) in items.iter().enumerate() {
                        let name = v["function"]["name"].as_str().unwrap_or("");
                        spaced(v, &format!("{at}[{i}]{name}"), found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        spaced(&tools_schema(true, &["warehouse".into()]), "", &mut found);
        assert!(found.is_empty(), "{found:#?}");
    }
}

#[cfg(test)]
#[path = "chat_empty_reply_tests.rs"]
mod chat_empty_reply_tests;

#[cfg(test)]
#[path = "chat_terminal_tests.rs"]
mod chat_terminal_tests;

#[cfg(test)]
#[path = "chat_stream_tests.rs"]
mod stream_tests;
