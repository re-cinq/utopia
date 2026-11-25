//! **Execution** of the seven tools, regardless of who calls them.
//!
//! These used to be seven branches of one `match` in `chat.rs`, each 20–70 lines, with closures capturing
//! the local variables of the streaming loop. That is fine with only one caller, a conversation—**and MCP is the second one**.
//! After extraction, both sides share one implementation, so "entity_facts in conversation" and "entity_facts in MCP"
//! cannot become different things.
//!
//! Tool definitions (the JSON schema the model sees) are still in `chat.rs`: they are part of the prompt
//! and follow the conversation strategy; here we only handle what happens once the arguments arrive.

use serde_json::json;
use utopia_core::models::{ChunkView, DataSourceView, EntityFact, GraphChange};
use uuid::Uuid;

use crate::retrieval;
use crate::state::AppState;

const SEARCH_TOP_K: usize = 6;
const TOOL_CHUNK_CHARS: usize = 800;
/// The body budget per page of `get_document`; anything beyond it is read on with a character offset, so the context is not stuffed.
const DOCUMENT_CHARS: usize = 24_000;
/// Maximum number of entries returned by one changes call. In a library just ingested, asserted entries run into the hundreds; sending them all
/// only fills the context without adding information—the informative events are corrected/rejected, and those kinds
/// are rare to begin with. When truncated, the body's end gives the next page so the model can read on; the "40+" the UI shows at that step
/// is for people only, the model does not read it
const CHANGES_LIMIT: i64 = 40;

/// The part of the world one tool call can see. **Read-only**: tools cannot change it.
pub struct ToolCtx<'a> {
    pub state: &'a AppState,
    pub kb_id: Uuid,
    pub workspace_id: Uuid,
    /// Sources mounted on this library. **The security boundary of `query_data` is exactly this list**:
    /// credentials never leave the server, and the model can only pick by name
    pub mounted_sources: &'a [DataSourceView],
    /// Only editor and above have `remember`
    pub can_write: bool,
    /// The person speaking. `remember` records it as "who said it", carried all the way to the pending-confirmation queue (0015).
    /// Always set in conversation; MCP has it too, since the token acts as the person (0014)
    pub actor: Option<Uuid>,
    /// Via MCP, which token is speaking. **Besides the person, this is recorded too**: one person can attach
    /// three agents at once, and a review card that shows only the person's name cannot tell which one recorded it (0026). None in conversation
    pub via_token: Option<Uuid>,
    /// The user's words in this turn. The graph tools use them to disambiguate: same-name candidates are ranked by "whose context profile is closest to this question".
    /// MCP does not have it (the agent's intent is not in the request), so there candidates are ranked by fact count
    pub question: Option<&'a str>,
}

impl ToolCtx<'_> {
    /// Whether there is a source list this turn: conversation has one, MCP does not (MCP calls always carry a token).
    /// The `[n]` on graph fact lines is printed only when there is a list (#935): the number points at an entry in the list,
    /// which means nothing to a caller who has no list; such a caller takes its provenance from `document_ids` in `structuredContent`
    pub fn has_source_list(&self) -> bool {
        self.via_token.is_none()
    }

    /// The vector of a piece of text, using this workspace's embedding model. None if none is configured or the call fails:
    /// the graph tools then match by substring as usual, and vectors are just a second stage
    pub async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let settings = utopia_store::settings::get(&self.state.pool, self.workspace_id)
            .await
            .ok()
            .flatten()?;
        let client = crate::llm_util::embed_client(&settings)?;
        match client.embed(&[text.to_string()]).await {
            Ok(mut v) if !v.is_empty() => Some(v.remove(0)),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "embedding failed, graph tool falls back to substring matching");
                None
            }
        }
    }

    /// [`Self::embed`]; within one turn the same text is embedded only once (see [`ToolSink::embeddings`])
    pub async fn embed_once(&self, sink: &mut ToolSink, text: &str) -> Option<Vec<f32>> {
        if let Some(vec) = sink.embeddings.get(text) {
            return Some(vec.clone());
        }
        let vec = self.embed(text).await?;
        sink.embeddings.insert(text.to_string(), vec.clone());
        Some(vec)
    }
}

/// What accumulates outward during tool execution.
///
/// **Citation numbers are stateful**: the 3 in `[3]` depends on how many have already been cited earlier in this turn,
/// so each tool cannot number on its own and merge afterwards—that would give the same chunk two numbers.
#[derive(Default)]
pub struct ToolSink {
    /// Dedup key (chunk uuid, or `charter:{slug}#{anchor}`); index + 1 is the citation number
    pub source_ids: Vec<String>,
    /// The source list sent to the frontend, in the same order as `source_ids`
    pub sources: Vec<serde_json::Value>,
    /// Entities recognized in this turn, which go into the session for replay in the next turn
    pub resolved: Vec<serde_json::Value>,
    /// Text embedded in this turn and its vector, not stored in the database. Each name-based lookup of a same-name entity compares profiles
    /// against the same question; when predicates do not match, the same word is embedded again. Within a turn the text and the model are fixed, so embedding once is enough:
    /// one sink per conversation turn, one per MCP call. Only successes are recorded; a failure is retried next time as usual.
    /// Picking the scope (`mapping_index::relevant`) and retrieving chunks (`search_chunks`) also take the question's
    /// vector from here and record it here (follow-up to #971)
    pub embeddings: crate::llm_util::EmbedCache,
}

/// A step shown on the UI (#942).
///
/// `kind` and `label` (data: query, entity name, source name) stay as they are; `detail` is English, for stored messages and for
/// readers without localization. The UI speaks to the reader in their language from the structured fields (0004): `status`, `count` /
/// `total`, moments in time, and a few fields specific to individual steps. **A successful step also writes `status: ok`**: steps with
/// a status are described by their fields, and those without one (messages stored before this) fall back to detail
pub(crate) struct Step(serde_json::Map<String, serde_json::Value>);

impl Step {
    pub(crate) fn new(
        kind: &str,
        label: impl Into<serde_json::Value>,
        detail: impl Into<String>,
    ) -> Self {
        let mut step = serde_json::Map::new();
        step.insert("kind".into(), json!(kind));
        step.insert("label".into(), label.into());
        step.insert("detail".into(), json!(detail.into()));
        step.insert("status".into(), json!("ok"));
        Self(step)
    }

    /// `ok | failed | not_found | invalid`
    pub(crate) fn status(self, status: &'static str) -> Self {
        self.field("status", status)
    }

    pub(crate) fn count(self, n: usize) -> Self {
        self.field("count", n)
    }

    /// Fewer listed than exist: `count` of `total`
    pub(crate) fn total(self, n: usize) -> Self {
        self.field("total", n)
    }

    /// Bad arguments: this call did nothing. `param` is which one; the parameter name is data, and the UI prints it as is
    pub(crate) fn invalid(self, param: &str) -> Self {
        self.status("invalid").field("param", param)
    }

    /// Argument missing, or given as empty
    pub(crate) fn missing(self, param: &str) -> Self {
        self.invalid(param).field("missing", true)
    }

    /// Cut off at the limit, total unknown ("40+")
    pub(crate) fn more(self, more: bool) -> Self {
        if more {
            self.field("more", true)
        } else {
            self
        }
    }

    /// A moment on the world axis is written as `valid_at`, and on the recording axis as `before` or `as_of`. If before is given, only
    /// before is written: that is the moment the person asked about, and as_of is computed by subtracting one microsecond from it.
    ///
    /// **The world-axis key cannot be called `at`**: every step the conversation stores carries `at`—how long the body was when this step happened
    /// (UTF-16 code units), which replay uses to thread the trace back into the body; writing it here would overwrite that
    pub(crate) fn moments(
        mut self,
        at: Option<chrono::DateTime<chrono::Utc>>,
        as_of: Option<chrono::DateTime<chrono::Utc>>,
        before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        use crate::time_text::instant;
        if let Some(t) = at {
            self = self.field("valid_at", instant(t));
        }
        match (before, as_of) {
            (Some(b), _) => self.field("before", instant(b)),
            (None, Some(r)) => self.field("as_of", instant(r)),
            (None, None) => self,
        }
    }

    pub(crate) fn field(mut self, key: &str, value: impl serde::Serialize) -> Self {
        self.0.insert(
            key.into(),
            serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        );
        self
    }

    pub(crate) fn json(self) -> serde_json::Value {
        serde_json::Value::Object(self.0)
    }
}

/// One call's text, UI steps, and optional machine-readable result; not put into the ToolSink that accumulates across calls.
pub struct ToolResult {
    pub text: String,
    pub step: serde_json::Value,
    pub structured_content: Option<serde_json::Value>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn new(text: String, step: serde_json::Value) -> Self {
        Self {
            text,
            step,
            structured_content: None,
            is_error: false,
        }
    }

    pub fn structured(mut self, content: serde_json::Value) -> Self {
        self.structured_content = Some(content);
        self
    }

    pub fn error(mut self) -> Self {
        self.is_error = true;
        self
    }
}

/// Dispatch by name. **An unknown tool is not an error**: the model occasionally invents a name,
/// and telling it that no such tool exists means it picks another one next turn, which is better than interrupting the whole conversation.
pub async fn dispatch(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    name: &str,
    args: &serde_json::Value,
) -> ToolResult {
    match name {
        "search_chunks" => search_chunks(ctx, sink, args).await,
        "get_document" => get_document(ctx, sink, args).await,
        "search_docs" => search_docs(ctx, sink, args).await,
        "find_entities" => super::tools_graph::find_entities(ctx, sink, args).await,
        "entity_facts" => super::tools_graph::entity_facts(ctx, sink, args).await,
        "neighbors" => super::tools_graph::neighbors(ctx, sink, args).await,
        "timeline" => super::tools_graph::timeline(ctx, sink, args).await,
        "paths_between" => super::tools_graph::paths_between(ctx, sink, args).await,
        "changes" => changes(ctx, args).await,
        // Business rules are read-only (0021): their criteria must be visible, but **writing rules is not opened to the model**:
        // "reasoning criteria are written by people" is the line shared by 0002 and 0021, and a single tool call
        // cannot tell "a person dictated it, an agent typed it for them" from "the model made up a rule"
        "list_rules" => list_rules(ctx).await,
        "rule_matches" => rule_matches(ctx, args).await,
        "query_data" if !ctx.mounted_sources.is_empty() => query_data(ctx, args).await,
        "remember" if ctx.can_write => remember(ctx, args).await,
        other => ToolResult::new(
            format!("Unknown tool: {other}"),
            Step::new("tool", other, "unknown")
                .status("not_found")
                .json(),
        ),
    }
}

/// Something already cited returns its original number; one not yet cited gets a new number. **The same chunk may have only one number
/// in a conversation turn**, otherwise the model cites `[2]` and the UI shows two `[2]`s.
pub(super) fn cite(
    sink: &mut ToolSink,
    key: String,
    make: impl FnOnce(usize) -> serde_json::Value,
) -> usize {
    match sink.source_ids.iter().position(|id| *id == key) {
        Some(i) => i + 1,
        None => {
            sink.source_ids.push(key);
            sink.sources.push(make(sink.source_ids.len()));
            sink.source_ids.len()
        }
    }
}

const QUOTES_PER_SOURCE: usize = 8;
const QUOTE_CHARS: usize = 400;

/// Adds a sentence the source said to the n-th source (follow-up to #968): the number opens that chunk, and `quotes` are the sentences
/// in the chunk that were cited, which the UI marks in the original text. One number per chunk; as many sentences as are cited in the chunk are recorded, deduplicated,
/// in citation order. A chunk that retrieval registered first lacks this cell; it is filled in when the graph cites it later
///
/// Two caps: the list is re-sent in full on each tool call, and the closing request carries it too, so a chunk yielding dozens of facts
/// must not let it grow. A sentence that is too long is not recorded in full rather than truncated: the UI finds the sentence in the original text, and a truncated one cannot be found
pub(super) fn add_quote(sink: &mut ToolSink, n: usize, quote: &str) {
    if quote.chars().count() > QUOTE_CHARS {
        return;
    }
    let Some(entry) = n.checked_sub(1).and_then(|i| sink.sources.get_mut(i)) else {
        return;
    };
    let Some(map) = entry.as_object_mut() else {
        return;
    };
    let quotes = map
        .entry("quotes")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let Some(list) = quotes.as_array_mut() {
        if list.len() < QUOTES_PER_SOURCE && !list.iter().any(|q| q.as_str() == Some(quote)) {
            list.push(serde_json::Value::String(quote.to_string()));
        }
    }
}

pub async fn search_chunks(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    args: &serde_json::Value,
) -> ToolResult {
    // Required parameters are blocked by `chat::check_call` before dispatch, so there is no fallback here
    // to the user's original words: the fallback would produce a wrong answer that looks fine
    let q = args["query"].as_str().unwrap_or_default().to_string();
    // Recording axis (0019 / #347): search only what the store held at that moment. The full-text path is still "now",
    // so hits are not wrong but may be missing: retrieval.rs's header says so
    let as_of = args["as_of"].as_str().and_then(parse_when);
    let chunks = match retrieval::hybrid(
        ctx.state,
        ctx.kb_id,
        ctx.workspace_id,
        &q,
        SEARCH_TOP_K,
        as_of,
        Some(&mut sink.embeddings),
    )
    .await
    {
        Ok(chunks) => chunks,
        Err(e) => {
            tracing::warn!(error = %e, "MCP chunk search failed");
            return ToolResult::new(
                "Could not search the documents.".into(),
                Step::new("search", q, "failed").status("failed").json(),
            )
            .error();
        }
    };
    retrieval::record_retrievals(ctx.state, chunks.iter().map(|c| c.document_id).collect());
    let mut lines = Vec::new();
    for c in &chunks {
        let n = cite(sink, c.id.to_string(), |n| source_json(n, c));
        // Rows carry document_id: when a hit is cut at 800 characters, the model needs a way to fetch the whole document
        lines.push(format!(
            "[{n}] \"{}\" section {} (document_id: {}):\n{}",
            c.filename,
            c.seq + 1,
            c.document_id,
            truncate(&c.text, TOOL_CHUNK_CHARS)
        ));
    }
    let text = if lines.is_empty() {
        "No results.".to_string()
    } else {
        lines.join("\n\n")
    };
    ToolResult::new(
        text,
        Step::new("search", q.as_str(), format!("{} sources", chunks.len()))
            .count(chunks.len())
            .json(),
    )
    .structured(json!({
        "kb_id": ctx.kb_id, "as_of": as_of,
        "limit": SEARCH_TOP_K, "limit_reached": chunks.len() == SEARCH_TOP_K,
        "chunks": chunks.iter().map(|c| json!({
            "chunk_id": c.id, "document_id": c.document_id,
            "seq": c.seq, "filename": c.filename,
            "text": truncate(&c.text, TOOL_CHUNK_CHARS),
            "truncated": c.text.trim().chars().count() > TOOL_CHUNK_CHARS,
        })).collect::<Vec<_>>()
    }))
}

/// The full text of one document. **Everything search_chunks cannot reach is here**: it returns only the top six hits,
/// each cut at 800 characters; when the answer falls at character 801 or in a chunk that did not make the list, retrieval itself is not wrong,
/// the fault is the lack of a second step.
pub async fn get_document(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    args: &serde_json::Value,
) -> ToolResult {
    let refuse = |step: Step| {
        ToolResult::new(
            "No document with that id in this knowledge base.".to_string(),
            step.json(),
        )
    };
    let Some(id) = args["document_id"]
        .as_str()
        .and_then(|s| s.trim().parse::<Uuid>().ok())
    else {
        return refuse(Step::new("document", "?", "invalid id").invalid("document_id"));
    };
    let offset = match args.get("offset") {
        None => 0,
        Some(value) => {
            let Some(offset) = value.as_u64().and_then(|n| usize::try_from(n).ok()) else {
                return ToolResult::new(
                    "get_document needs `offset` to be a non-negative integer character position."
                        .into(),
                    Step::new("document", "?", "invalid offset")
                        .invalid("offset")
                        .json(),
                )
                .error();
            };
            offset
        }
    };
    // Ids outside this library are all treated as nonexistent: being unable to tell "none" from "not allowed to see" is the correct behaviour
    let doc = match utopia_store::documents::find_in_kb(&ctx.state.pool, ctx.kb_id, id).await {
        Ok(Some(doc)) => doc,
        Ok(None) => return refuse(Step::new("document", "?", "not found").status("not_found")),
        Err(e) => {
            tracing::warn!(error = %e, "MCP document lookup failed");
            return ToolResult::new(
                "Could not read the document.".into(),
                Step::new("document", "?", "failed").status("failed").json(),
            )
            .error();
        }
    };
    let chunks =
        match utopia_store::documents::chunks_in_document(&ctx.state.pool, ctx.kb_id, id).await {
            Ok(chunks) => chunks,
            Err(e) => {
                tracing::warn!(error = %e, "MCP document chunk read failed");
                return ToolResult::new(
                    "Could not read the document.".into(),
                    Step::new("document", doc.filename, "failed")
                        .status("failed")
                        .json(),
                )
                .error();
            }
        };

    let mut lines = Vec::new();
    let mut skip = offset;
    let mut used = 0usize;
    let mut omitted = 0usize;
    for c in &chunks {
        let body = c.text.trim();
        let len = body.chars().count();
        // The offset counts only the body as concatenated in order; it does not count headings, citations, or layout between chunks.
        if offset > 0 && skip >= len {
            skip -= len;
            continue;
        }
        let remaining = len - skip;
        let room = DOCUMENT_CHARS.saturating_sub(used);
        // A chunk that does not fit is not sent whole, nor cited: sending a citation number whose body is absent
        // would show up in the UI as a citation pointing at nothing
        if room == 0 {
            omitted += remaining;
            continue;
        }
        let read = remaining.min(room);
        let body: String = if skip > 0 || remaining > room {
            body.chars().skip(skip).take(read).collect()
        } else {
            body.to_string()
        };
        skip = 0;
        omitted += remaining - read;
        used += read;
        let n = cite(sink, c.id.to_string(), |n| source_json(n, c));
        lines.push(format!(
            "[{n}] \"{}\" section {}:\n{body}",
            c.filename,
            c.seq + 1
        ));
    }
    if omitted > 0 {
        lines.push(format!(
            "… truncated, {omitted} chars omitted; call get_document with document_id: {id}, offset: {} to continue",
            offset + used
        ));
    }

    let when = doc
        .doc_time
        .map(crate::time_text::instant)
        .unwrap_or_else(|| "no date".to_string());
    let header = format!(
        "\"{}\" ({when}) — {} section(s):",
        doc.filename,
        chunks.len()
    );
    let text = if lines.is_empty() && offset > 0 {
        format!("{header}\n(no text remaining at offset {offset})")
    } else if lines.is_empty() {
        format!("{header}\n(no text)")
    } else {
        format!("{header}\n\n{}", lines.join("\n\n"))
    };
    ToolResult::new(
        text,
        Step::new(
            "document",
            doc.filename.as_str(),
            format!("{} sections", chunks.len()),
        )
        .count(chunks.len())
        .json(),
    )
}

pub async fn search_docs(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    args: &serde_json::Value,
) -> ToolResult {
    // Required parameters are blocked by `chat::check_call` before dispatch, so there is no fallback here
    // to the user's original words: the fallback would produce a wrong answer that looks fine
    let q = args["query"].as_str().unwrap_or_default().to_string();
    // Tantivy is synchronous, so put it on the blocking thread pool rather than tying up a runtime thread (#515, same reasoning as retrieval.rs)
    let hits = {
        let docs = ctx.state.docs.clone();
        let q = q.clone();
        tokio::task::spawn_blocking(move || docs.search(&q, 4))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    for h in &hits {
        let key = format!("charter:{}#{}", h.slug, h.anchor);
        let n = cite(sink, key, |n| charter_source_json(n, h));
        lines.push(format!(
            "[{n}] Utopia Charter — {} › {}:\n{}",
            h.title,
            h.heading,
            truncate(&h.body, 1600)
        ));
    }
    let text = if lines.is_empty() {
        "No matching manual sections.".to_string()
    } else {
        lines.join("\n\n")
    };
    ToolResult::new(
        text,
        Step::new("docs", q.as_str(), format!("{} sections", hits.len()))
            .count(hits.len())
            .json(),
    )
}

/// This library's criteria. **Give the thresholds exactly as they are**: the model must be able to explain "why is this counted as a gas well",
/// rather than guessing a plausible-sounding threshold.
pub async fn list_rules(ctx: &ToolCtx<'_>) -> ToolResult {
    let Ok(rules) = utopia_store::business_rules::list(&ctx.state.pool, ctx.kb_id).await else {
        return ToolResult::new(
            "Could not read the rules.".to_string(),
            Step::new("tool", "list_rules", "failed")
                .status("failed")
                .json(),
        )
        .error();
    };
    let Ok(axioms) = utopia_store::reasoning::active_axiom_rules(&ctx.state.pool, ctx.kb_id).await
    else {
        return ToolResult::new(
            "Could not read the rules.".to_string(),
            Step::new("tool", "list_rules", "failed")
                .status("failed")
                .json(),
        )
        .error();
    };
    if rules.is_empty() && axioms.is_empty() {
        return ToolResult::new(
            "This base has no business rules or active ontology axioms.".to_string(),
            Step::new("tool", "list_rules", "none").count(0).json(),
        );
    }
    let expressions: Vec<_> = rules
        .iter()
        .filter(|r| r["conclusion"] == "computed")
        .map(|r| &r["conclude_expr"])
        .collect();
    let Ok(descriptions) = utopia_store::business_rules::describe_expressions(
        &ctx.state.pool,
        ctx.kb_id,
        &expressions,
    )
    .await
    else {
        return ToolResult::new(
            "Could not read the rules.".to_string(),
            Step::new("tool", "list_rules", "failed")
                .status("failed")
                .json(),
        )
        .error();
    };
    let mut descriptions = descriptions.into_iter();
    let mut text = rules
        .iter()
        .map(|r| {
            let conditions = r["conditions"]
                .as_array()
                .map(|cs| {
                    // The store orders conditions by group and sequence. Keep that
                    // order while showing the same OR-of-ANDs the evaluator uses.
                    let mut groups: Vec<(i64, Vec<String>)> = Vec::new();
                    for c in cs {
                        let group = c["group"].as_i64().unwrap_or(0);
                        // Legacy conditions have no side and mean the subject;
                        // keep their familiar unprefixed text while disambiguating Y.
                        let side = if c["side"] == "y" { "Y." } else { "" };
                        let condition = format!(
                            "{side}{} {} {}",
                            c["predicate_label"].as_str().unwrap_or("?"),
                            c["op"].as_str().unwrap_or("?"),
                            c["operand"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| c["operand"].to_string()),
                        );
                        if let Some((_, conditions)) =
                            groups.last_mut().filter(|(g, _)| *g == group)
                        {
                            conditions.push(condition);
                        } else {
                            groups.push((group, vec![condition]));
                        }
                    }
                    let alternatives = groups.len() > 1;
                    groups
                        .into_iter()
                        .map(|(_, conditions)| {
                            let text = conditions.join(" AND ");
                            if alternatives && conditions.len() > 1 {
                                format!("({text})")
                            } else {
                                text
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" OR ")
                })
                .unwrap_or_default();
            let concludes = if r["conclusion"] == "typing" {
                r["conclude_type_label"].as_str().unwrap_or("?").to_string()
            } else if r["conclusion"] == "computed" {
                format!(
                    "{} = {}",
                    r["conclude_predicate_label"].as_str().unwrap_or("?"),
                    descriptions
                        .next()
                        .flatten()
                        .unwrap_or_else(|| "(expression unavailable)".to_string()),
                )
            } else if r["conclusion"] == "relation" {
                format!(
                    "{} from X to the Y reached by {}",
                    r["conclude_predicate_label"].as_str().unwrap_or("?"),
                    r["join_predicate_label"].as_str().unwrap_or("?"),
                )
            } else {
                format!(
                    "{} = {}",
                    r["conclude_predicate_label"].as_str().unwrap_or("?"),
                    r["conclude_value"]
                )
            };
            format!(
                "{} [{}] — applies to {} where {} ⇒ {} · marks {} now · id {}",
                r["name"].as_str().unwrap_or("?"),
                if r["enabled"] == true { "on" } else { "off" },
                r["subject_label"].as_str().unwrap_or("?"),
                conditions,
                concludes,
                r["derived_count"],
                r["id"].as_str().unwrap_or("?"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if rules.is_empty() {
        text = "This base has no business rules.".to_string();
    }
    if !axioms.is_empty() {
        // A declared rule is not the same as this library actually deriving from it: with the switch off, the definitions are still listed, but it must say nothing was run.
        let Ok(kb) = utopia_store::kbs::get(&ctx.state.pool, ctx.kb_id).await else {
            return ToolResult::new(
                "Could not read the rules.".to_string(),
                Step::new("tool", "list_rules", "failed")
                    .status("failed")
                    .json(),
            )
            .error();
        };
        text.push_str(if kb.materialize_inferences {
            "\n\nOntology axioms (from property declarations):\n"
        } else {
            "\n\nOntology axioms (derivation is off for this base; nothing is derived from these):\n"
        });
        let lines: Vec<_> = axioms
            .iter()
            .map(|a| {
                let (p, q) = (&a.predicate, &a.target);
                let conclusion = match a.kind {
                    "transitive" => format!("X {p} Y AND Y {p} Z ⇒ X {p} Z"),
                    "symmetric" | "inverse" => format!("X {p} Y ⇒ Y {q} X"),
                    _ => format!("X {p} Y ⇒ X {q} Y"),
                };
                format!("{} of {p}: {conclusion}", a.kind)
            })
            .collect();
        text.push_str(&lines.join("\n"));
    }
    let n = rules.len() + axioms.len();
    ToolResult::new(
        text,
        Step::new("tool", "list_rules", format!("{n} rules"))
            .count(n)
            .json(),
    )
}

/// A rule's current tagging: who. **The premises are given too**: a conclusion without its premises is no different from an unsupported assertion.
pub async fn rule_matches(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> ToolResult {
    let Some(rule_id) = args["rule_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return ToolResult::new(
            "Invalid rule_id (expected the uuid returned by list_rules).".to_string(),
            Step::new("tool", "rule_matches", "invalid id")
                .invalid("rule_id")
                .json(),
        );
    };
    let limit = args["limit"].as_i64().unwrap_or(50).clamp(1, 200);
    let Ok((rows, total)) =
        utopia_store::business_rules::matches(&ctx.state.pool, ctx.kb_id, rule_id, limit, 0).await
    else {
        return ToolResult::new(
            "Could not read what that rule marks.".to_string(),
            Step::new("tool", "rule_matches", "failed")
                .status("failed")
                .json(),
        )
        .error();
    };
    if rows.is_empty() {
        return ToolResult::new(
            "That rule marks nothing right now.".to_string(),
            Step::new("tool", "rule_matches", "0").count(0).json(),
        );
    }
    let text = rows
        .iter()
        .map(|m| {
            let premises = m["premises"]
                .as_array()
                .map(|p| {
                    p.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            // This query includes historical conclusions. A missing bound does
            // not establish that the conclusion holds now.
            let bound = |key: &str, precision: &str, unknown: &str| {
                m[key]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .map(|t| crate::time_text::world(t, m[precision].as_str()))
                    .unwrap_or_else(|| unknown.to_string())
            };
            let from = bound("valid_from", "valid_from_precision", "unknown start");
            let to = bound("valid_to", "valid_to_precision", "unknown end");
            let concluded = if m["object_entity"].is_null() {
                m["concluded"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| m["concluded"].to_string())
            } else {
                format!(
                    "{} {}",
                    m["relation_predicate"]
                        .as_str()
                        .unwrap_or_else(|| m["concluded"].as_str().unwrap_or("?")),
                    m["object_entity"].as_str().unwrap_or("?"),
                )
            };
            format!(
                "{} ⇒ {concluded} (because {}) [validity: {from} → {to}] [{}]",
                m["entity"].as_str().unwrap_or("?"),
                premises,
                m["entity_id"].as_str().unwrap_or("?"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Truncation must be said: the model seeing 50 entries will take them as all of them, while the library may have two hundred
    let text = if total > rows.len() as i64 {
        format!("{text}\n(showing {} of {total} matches)", rows.len())
    } else {
        text
    };
    ToolResult::new(
        text,
        Step::new("tool", "rule_matches", format!("{total} matches"))
            .field("count", total)
            .json(),
    )
}

/// The two row-paged tools share the bounds: the database OFFSET takes i64, in-memory paging takes usize.
pub(super) fn page_offset(args: &serde_json::Value, kind: &str) -> Result<usize, ToolResult> {
    match args.get("offset") {
        None => Ok(0),
        Some(value) => value
            .as_i64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| {
                ToolResult::new(
                    "Expected `offset` to be a non-negative integer row position.".into(),
                    Step::new(kind, "?", "invalid offset")
                        .invalid("offset")
                        .json(),
                )
                .error()
            }),
    }
}

pub async fn changes(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> ToolResult {
    let offset = match page_offset(args, "changes") {
        Ok(offset) => offset,
        Err(result) => return result,
    };
    // Both ends use the same notation as `at`: YYYY / YYYY-MM / YYYY-MM-DD. `since` takes the first day of that span,
    // `until` takes the last day. "What changed in 2023" should not force the model to invent a December 31
    let since = args["since"]
        .as_str()
        .and_then(utopia_extract::parse_time)
        .map(|(t, _)| t.date_naive());
    let until = args["until"]
        .as_str()
        .and_then(utopia_extract::parse_time)
        .map(|(t, p)| period_last_day(t.date_naive(), p));
    // Both ends of the window go to the UI with the day that was asked about: the "now" in the label is English
    let (since_day, until_day) = (since, until);
    let Some((since, until, window)) = changes_window(since, until, chrono::Utc::now()) else {
        return ToolResult::new(
            "Invalid or missing `since` (expected YYYY-MM-DD).".to_string(),
            Step::new("changes", "?", "invalid since")
                .invalid("since")
                .json(),
        )
        .error();
    };
    let entity = args["entity_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok());
    let kinds: Option<Vec<String>> = args["kinds"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });
    let kinds = kinds.filter(|k: &Vec<String>| !k.is_empty());
    let mut rows = match utopia_store::graph::graph_changes(
        &ctx.state.pool,
        ctx.kb_id,
        since,
        until,
        entity,
        kinds.as_deref(),
        CHANGES_LIMIT + 1,
        offset as i64,
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "Graph changes lookup failed");
            return ToolResult::new(
                "Could not read the graph changes.".into(),
                Step::new("changes", window, "failed")
                    .status("failed")
                    .json(),
            )
            .error();
        }
    };
    // A full page does not mean there is a next page; the one extra row read is only used to give the position for reading on.
    let more = rows.len() as i64 > CHANGES_LIMIT;
    rows.truncate(CHANGES_LIMIT as usize);
    let next_offset = more.then(|| offset + rows.len());
    let text = if rows.is_empty() && offset > 0 {
        format!("No recorded changes remaining in {window} at offset {offset}.")
    } else if rows.is_empty() {
        format!("No recorded changes in {window}.")
    } else {
        let mut lines: Vec<String> = rows.iter().map(change_line).collect();
        // It takes the **most recent** forty entries; earlier ones are cut off. Without saying so, the model would treat these forty as the window's
        // entirety, when the correction it is looking for may be earlier
        if let Some(next) = next_offset {
            let page = if offset == 0 {
                format!("Only the {CHANGES_LIMIT} most recent changes in this window are listed")
            } else {
                format!("{CHANGES_LIMIT} changes at offset {offset} are listed")
            };
            lines.push(format!(
                "({page}; call changes with offset={next} and the same window and filters to continue.)"
            ));
        }
        lines.join("\n")
    };
    let detail = if more {
        format!("{CHANGES_LIMIT}+ changes")
    } else {
        format!("{} changes", rows.len())
    };
    let mut step = Step::new("changes", window.as_str(), detail)
        .count(rows.len())
        .more(more);
    if let Some(day) = since_day {
        step = step.field("since", day.to_string());
    }
    if let Some(day) = until_day {
        step = step.field("until", day.to_string());
    }
    ToolResult::new(text, step.json()).structured(json!({
        "kb_id": ctx.kb_id, "since": since, "until": until,
        "limit": CHANGES_LIMIT, "limit_reached": rows.len() as i64 == CHANGES_LIMIT,
        "offset": offset, "next_offset": next_offset,
        "changes": rows.iter().map(|r| json!({
            "fact_id": r.fact_id, "at": r.at, "kind": r.kind,
            "subject_id": r.subject_id, "subject_name": r.subject_name,
            "predicate_label": r.predicate_label, "object_name": r.object_name,
            "object_value": r.object_value, "confidence": r.confidence,
            "previous_object_name": r.previous_object_name,
            "previous_object_value": r.previous_object_value,
            "valid_from": r.valid_from, "valid_to": r.valid_to,
            "valid_from_precision": r.valid_from_precision,
            "valid_to_precision": r.valid_to_precision,
            "document_id": r.document_id, "filename": r.filename, "quote": r.quote,
            "quote_origin": r.quote_origin,
        })).collect::<Vec<_>>()
    }))
}

pub async fn query_data(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> ToolResult {
    let ds_name = args["data_source"].as_str().map(str::trim).unwrap_or("");
    let sql = args["sql"].as_str().map(str::trim).unwrap_or("");
    let purpose = args["purpose"].as_str().map(str::trim).unwrap_or("");
    // Security boundary: only sources mounted on this KB are allowed (credentials never leave the server)
    let found = ctx
        .mounted_sources
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(ds_name));
    let detail = if purpose.is_empty() {
        sql.chars().take(60).collect::<String>()
    } else {
        purpose.to_string()
    };
    // The step on the UI carries the SQL that was written (#936): a reader can expand it and see how the number was computed;
    // a query error and a missing source are each a result of their own, no longer looking the same as "found it"
    let step = Step::new("query", ds_name, detail).field("sql", sql);
    let (text, step) = match found {
        None => (
            format!(
                "Unknown data source '{ds_name}'. Mounted sources: {}",
                ctx.mounted_sources
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            step.status("not_found"),
        ),
        Some(ds) => match execute_query(ctx.state, ds.id, sql).await {
            Ok(result) => (
                query_text(&result),
                step.count(result.rows.len()).more(result.truncated),
            ),
            // Errors pass through: the model can correct the SQL and retry on that basis
            Err(e) => (format!("Query failed: {e}"), step.status("failed")),
        },
    };
    ToolResult::new(text, step.json())
}

pub async fn remember(ctx: &ToolCtx<'_>, args: &serde_json::Value) -> ToolResult {
    // NUL is stripped at the entry (#665): `append_episode` strips it too, but the reply assembled here and the card details
    // use the same text. In conversation they are stored in the session record, and Postgres TEXT, like JSONB,
    // does not accept 0x00, so a turn could fail to be stored just because it echoed a few bytes
    let text = utopia_core::without_nul(args["text"].as_str().unwrap_or(""));
    let text = text.trim();
    // Same notation as `at`: YYYY / YYYY-MM / YYYY-MM-DD or RFC3339. A date lands at the noon of the span's first day, which is farthest from both day boundaries;
    // a moment is given as is. The echo is written at the precision given, so "2023" is not presented as 1 January
    let (occurred_at, occurred_text) = match args["occurred_at"].as_str().map(str::trim) {
        Some(s) if !s.is_empty() => match utopia_extract::parse_time(s) {
            Some((d, precision)) => (
                if matches!(precision, "year" | "month" | "day") {
                    d + chrono::Duration::hours(12)
                } else {
                    d
                },
                crate::time_text::world(d, Some(precision)),
            ),
            None => match parse_when(s) {
                Some(at) => (at, crate::time_text::instant(at)),
                None => {
                    let now = chrono::Utc::now();
                    (now, crate::time_text::instant(now))
                }
            },
        },
        _ => {
            let now = chrono::Utc::now();
            (now, crate::time_text::instant(now))
        }
    };
    if text.is_empty() {
        return ToolResult::new(
            "remember requires non-empty text.".to_string(),
            Step::new("tool", "remember", "empty")
                .missing("text")
                .json(),
        )
        .error();
    }
    match utopia_store::memory::append_episode(&ctx.state.pool, ctx.kb_id, text, occurred_at).await
    {
        Ok((doc_id, chunk_id)) => {
            // Ingestion (embedding / indexing / incremental extraction) runs asynchronously through the queue and does not block the conversation.
            // Extracted facts **wait for a person's nod first** (0015), so here it can only honestly say "this sentence was recorded",
            // and cannot say how many were extracted: extraction has not yet run. The card grows into the conversation when the task completes
            let _ = utopia_store::jobs::enqueue(
                &ctx.state.pool,
                "memory_ingest",
                json!({
                    "document_id": doc_id,
                    "proposed_by": ctx.actor,
                    "proposed_token": ctx.via_token,
                }),
            )
            .await;
            ctx.state.emit_document(ctx.kb_id, doc_id);
            ToolResult::new(
                format!(
                    "Recorded the sentence (effective {}): {text}\n\
                     Facts extracted from it will be shown to the user for confirmation \
                     before entering the graph. Tell the user exactly that: the sentence is \
                     recorded, and the extracted facts await their confirmation. Do not claim \
                     any fact has been added to the knowledge graph.",
                    occurred_text
                ),
                // The confirmation card in the conversation takes its pending item by chunk_id; replay also redraws it from that
                Step::new(
                    "tool",
                    "remember",
                    text.chars().take(60).collect::<String>(),
                )
                .field("chunk_id", chunk_id)
                .json(),
            )
        }
        Err(e) => ToolResult::new(
            format!("Failed to record: {e}"),
            Step::new("tool", "remember", "failed")
                .status("failed")
                .json(),
        )
        .error(),
    }
}

// ---------------------------------------------------------------------------
// Helpers for layout and execution. **The conversation's degraded path uses them too**, which is why they are pub(super) rather than private
// ---------------------------------------------------------------------------

pub(super) fn source_json(n: usize, c: &ChunkView) -> serde_json::Value {
    json!({
        "n": n,
        "chunk_id": c.id,
        "document_id": c.document_id,
        "filename": c.filename,
        "excerpt": truncate(&c.text, 160),
    })
}

/// Charter reference: the frontend renders it as a manual line, linking to /docs/{slug}#{anchor}.
pub(super) fn charter_source_json(n: usize, h: &utopia_search::DocsSection) -> serde_json::Value {
    json!({
        "n": n,
        "kind": "charter",
        "slug": h.slug,
        "anchor": h.anchor,
        "heading": h.heading,
        "filename": h.title,
        "excerpt": truncate(&h.body, 160),
    })
}

/// Data-query execution: safety gate (parse allowlist) → engine execution (read-only session + forced LIMIT + timeout) → JSON rows.
pub(crate) async fn run_query(state: &AppState, ds_id: Uuid, sql: &str) -> anyhow::Result<String> {
    Ok(query_text(&execute_query(state, ds_id, sql).await?))
}

/// The same as `run_query`, but hands back the rows and the truncation flag themselves: the step in conversation needs the row count and truncation (#936)
async fn execute_query(
    state: &AppState,
    ds_id: Uuid,
    sql: &str,
) -> anyhow::Result<crate::query_engine::QueryResult> {
    let (engine, conn) = utopia_store::datasources::engine_and_conn(&state.pool, ds_id).await?;
    // The gate picks a dialect per engine: Databricks' backticks and Snowflake's :: casts must both pass the parser first
    let guarded = crate::query_engine::guard_sql_for(&engine, sql)?;
    crate::query_engine::engine_for(&engine, &conn)?
        .execute(&guarded)
        .await
}

/// The shape of the result as the model reads it: one JSON object per row, with the row count and truncation note at the end
fn query_text(result: &crate::query_engine::QueryResult) -> String {
    let mut out = String::new();
    if result.rows.is_empty() {
        out.push_str("(no rows)");
    } else {
        out.push_str(&result.rows.join("\n"));
        out.push_str(&format!("\n({} rows", result.rows.len()));
        if result.truncated {
            out.push_str(&format!(
                ", truncated at {} — aggregate in SQL for totals",
                crate::query_engine::ROW_CAP
            ));
        }
        out.push(')');
    }
    out
}

/// How the end that succeeds a line is written when the timeline closes that line (#970 step 2): the successor's name, or the new object;
/// an attribute fact's new value is written the same way as the literal on a fact line (`literal_text`)
pub(super) fn successor_text(
    name: Option<&str>,
    value: Option<&serde_json::Value>,
) -> Option<String> {
    name.map(str::to_string)
        .or_else(|| value.and_then(literal_text))
}

/// A fact line: "works at → Nebula Tech (2023-08 → now) [90%]", with the in direction shown as ←. `closer_mark` is the number
/// of the evidence line that closed it (` [n]` in conversation; empty in MCP and when there is no evidence)
pub(super) fn fact_line(f: &EntityFact, closer_mark: &str) -> String {
    // An attribute fact has no counterpart entity; the value lives in `object_value` (0004). Previously this looked only at `other_name`,
    // so salary and title reached the model as `salary → ?`: the interval and confidence were there, but the value was not, and
    // the model could only say "no salary information" (#348). The rendering rule matches the client's `fmtObjectValue`
    let literal = f
        .object_value
        .as_ref()
        .and_then(literal_text)
        .filter(|_| f.other_name.is_none());
    let other = f
        .other_name
        .as_deref()
        .or(literal.as_deref())
        .unwrap_or("?");
    // Attributes on an edge (0037) follow the counterpart: `invested_in → Kestrel [amount: 4000000000 $]`.
    // The most common question when reading a fact line is "how much was invested"; if the figure is not on the line, the model answers "no amount information"
    let quals: Vec<String> = f
        .qualifiers
        .iter()
        .map(|q| {
            let v = q
                .value
                .as_ref()
                .and_then(literal_text)
                .or_else(|| q.entity_name.clone())
                .unwrap_or_else(|| "?".to_string());
            format!("{}: {v}", q.key)
        })
        .collect();
    let other = if quals.is_empty() {
        other.to_string()
    } else {
        format!("{other} [{}]", quals.join(", "))
    };
    // When the ontology did not catch it and the original wording was not kept, use "?", the same convention as other
    // Do not invent a "related": that is exactly what removing related_to was meant to eliminate
    let pred = f.predicate_label.as_deref().unwrap_or("?");
    let core = if f.direction == "out" {
        format!("{pred} → {other}")
    } else {
        format!("{pred} ← {other}")
    };
    // Each end is written at its own precision; a start that is missing runs from the evidence, and an end of unknown date runs up to the document that states it
    //(time_text, 0022). It used to be %Y-%m-%d throughout, which printed a year-precision date as 1 January and an unknown end as now
    let successor = successor_text(f.closed_by.as_deref(), f.closed_by_value.as_ref());
    let range = crate::time_text::span(crate::time_text::Span {
        valid_from: f.valid_from,
        from_precision: f.valid_from_precision.as_deref(),
        valid_to: f.valid_to,
        to_precision: f.valid_to_precision.as_deref(),
        holds_from: f.holds_from,
        holds_to: f.holds_to,
        end_derived: f.end_derived,
        corrected: f.corrected_ends.as_deref(),
        closed_by: successor.as_deref().map(|who| (who, closer_mark)),
        correction_note: f.correction_note.as_deref(),
    });
    let range = if range.is_empty() {
        range
    } else {
        format!(" ({range})")
    };
    format!("{core}{range} [{}%]", (f.confidence * 100.0).round() as i32)
}

// Two changes on the recording axis can happen within the same second; the output must be able to go back into as_of unchanged, so it cannot be truncated to day or second.
fn record_stamp(time: chrono::DateTime<chrono::Utc>) -> String {
    crate::time_text::instant(time)
}

/// Strictly before T, at the ledger's resolution (timestamptz is microseconds): no later than T − 1µs.
/// `recorded_at <= T−1µs` is exactly `recorded_at < T`, and `invalidated_at > T−1µs` is exactly
/// `invalidated_at >= T`: 0019's held_at does not change a word
pub(super) fn just_before(t: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    t - chrono::Duration::microseconds(1)
}

pub(super) fn entity_facts_detail(
    count: usize,
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
) -> String {
    // The recording-axis clause: if before was given, say "before T", the moment the person asked about, not the one minus one microsecond
    let record = match (before, as_of) {
        (Some(b), _) => Some(format!("as recorded before {}", record_stamp(b))),
        (None, Some(r)) => Some(format!("as recorded by {}", record_stamp(r))),
        (None, None) => None,
    };
    match (at, record) {
        (Some(t), Some(r)) => format!("{count} facts at {}, {r}", record_stamp(t)),
        (Some(t), None) => format!("{count} facts as of {}", record_stamp(t)),
        (None, Some(r)) => format!("{count} facts {r}"),
        (None, None) => format!("{count} facts"),
    }
}

/// The moment parameter: `YYYY-MM-DD` or RFC3339. `at` and `as_of` share one parse; a moment on the recording axis
/// is often a timestamp with a time ("the moment the first wave finished ingesting"), which day precision cannot hold
pub(super) fn parse_when(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = raw.trim();
    // A full RFC3339 moment is accepted as given, with fractional seconds kept: within one second on the recording axis, an entry can be made before it is corrected
    // (#351), and cutting one digit here would fold the two beliefs back together. Date forms (YYYY / YYYY-MM / YYYY-MM-DD,
    // or an abbreviated time with a zone) are handed to the same parser the extraction side uses, taking the start of that span
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&chrono::Utc));
    }
    utopia_extract::parse_time(s).map(|(d, _)| d)
}

/// How a literal object is shown to the model: `{value, unit}` → "28000 CNY", boolean → ✓/✗,
/// and mapping-type `{summary}` → the summary itself. The same rule as `web/src/pages/Graph.tsx::fmtObjectValue`;
/// if the two sides diverge, what people see and what the model sees are no longer the same value
pub(super) fn literal_text(v: &serde_json::Value) -> Option<String> {
    // A bare scalar (old data in `changes` looks like this): a string is read as itself, without quotes
    match v {
        serde_json::Value::Null => return None,
        serde_json::Value::String(s) => return Some(s.clone()),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => return Some(v.to_string()),
        _ => {}
    }
    if let Some(val) = v.get("value") {
        let text = match val {
            serde_json::Value::Bool(true) => "✓".to_string(),
            serde_json::Value::Bool(false) => "✗".to_string(),
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => return None,
            other => other.to_string(),
        };
        return Some(
            match v
                .get("unit")
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty())
            {
                Some(unit) => format!("{text} {unit}"),
                None => text,
            },
        );
    }
    if let Some(summary) = v.get("summary").and_then(|s| s.as_str()) {
        return Some(summary.to_string());
    }
    Some(v.to_string())
}

/// The time window for changes: turns two optional dates into (the half-open interval for SQL, the window string for display).
///
/// **Extracted as a pure function because this once went wrong.** `until` gets a day added before it goes into SQL (a person saying "up to March 31"
/// wants the 31st included, while the SQL side is `< $3`). The first version also printed the value with the day added into
/// the display string, and the model therefore answered "up to August 30", when the question was about the 29th. The two values must be computed together
/// and tested together; written in two places they would diverge again sooner or later
///
/// `now` is passed in from outside rather than read inside, purely so this function can be tested
/// The last day of a span (year / month / day): `until=2023` means up to December 31, including that whole day.
fn period_last_day(first: chrono::NaiveDate, precision: &str) -> chrono::NaiveDate {
    use chrono::Datelike;
    match precision {
        "year" => chrono::NaiveDate::from_ymd_opt(first.year(), 12, 31).unwrap_or(first),
        "month" => {
            let (y, m) = if first.month() == 12 {
                (first.year() + 1, 1)
            } else {
                (first.year(), first.month() + 1)
            };
            chrono::NaiveDate::from_ymd_opt(y, m, 1)
                .and_then(|d| d.pred_opt())
                .unwrap_or(first)
        }
        _ => first,
    }
}

fn changes_window(
    since: Option<chrono::NaiveDate>,
    until: Option<chrono::NaiveDate>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
    String,
)> {
    let from = since?;
    let start = from.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let end = until
        .and_then(|d| d.succ_opt())
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
        .unwrap_or(now);
    // The display uses **the day asked about**; if nothing was asked, it writes now: never the end
    let label = format!(
        "{} → {}",
        from.format("%Y-%m-%d"),
        match until {
            Some(d) => d.format("%Y-%m-%d").to_string(),
            None => "now".to_string(),
        }
    );
    Some((start, end, label))
}

/// One line on the knowledge axis.
///
/// Layout writes the two axes **separately**: `at` is preceded by the event type, and the world-axis interval sits in brackets after the assertion.
/// If they were arranged as a string of dates, the model would read "recorded in 2026" as "happened in 2026", which is exactly
/// the misreading this tool exists to prevent.
fn change_line(c: &GraphChange) -> String {
    // A literal object follows the same rule as `fact_line` (`literal_text`): the two tools the model sees must not
    // use two different notations, or `{value, unit}` would print here as a block of JSON
    let object_text = |name: &Option<String>, value: &Option<serde_json::Value>| match (name, value)
    {
        (Some(name), _) => name.clone(),
        (None, Some(v)) => literal_text(v).unwrap_or_else(|| "?".to_string()),
        (None, None) => "?".to_string(),
    };
    let object = object_text(&c.object_name, &c.object_value);
    let object = if c.kind == "corrected" {
        format!(
            "old: {} → new: {object}",
            object_text(&c.previous_object_name, &c.previous_object_value)
        )
    } else {
        object
    };
    // Both ends of the world axis are written at their own precision (time_text); an event line has no anchor, so an unknown end can only be written as words
    let range = crate::time_text::span(crate::time_text::Span {
        valid_from: c.valid_from,
        from_precision: c.valid_from_precision.as_deref(),
        valid_to: c.valid_to,
        to_precision: c.valid_to_precision.as_deref(),
        holds_from: None,
        holds_to: None,
        // One change on the recording axis, not a fact line: it says for itself what was changed
        end_derived: false,
        corrected: None,
        closed_by: None,
        correction_note: None,
    });
    let range = if range.is_empty() {
        range
    } else {
        format!(" [valid {range}]")
    };
    // The file name carries no [n]: citation numbers belong to chunks, and here there is only a document, so sending a number out
    // would show up in the UI as a citation pointing at nothing
    let src = match (&c.filename, &c.quote) {
        (Some(f), Some(q)) => format!(" — from \"{f}\": \"{}\"", truncate(q, 160)),
        (Some(f), None) => format!(" — from \"{f}\""),
        _ => String::new(),
    };
    format!(
        "{} {}: {} {} {}{}{}",
        record_stamp(c.at),
        c.kind,
        c.subject_name,
        c.predicate_label.as_deref().unwrap_or("?"),
        object,
        range,
        src
    )
}

pub(super) fn truncate(text: &str, max_chars: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max_chars {
        t.to_string()
    } else {
        let cut: String = t.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
    fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().unwrap()
    }

    // --- parse_when -------------------------------------------------------------

    /// `at` and `as_of` share one parse: a date becomes the start of that day, RFC3339 is kept as is; anything else is None,
    /// and nothing is guessed—a guessed moment would quietly answer the question for another day
    #[test]
    fn a_moment_is_a_date_or_an_rfc3339_stamp_and_nothing_else() {
        assert_eq!(parse_when("2024-08-01"), Some(t("2024-08-01T00:00:00Z")));
        assert_eq!(
            parse_when(" 2026-09-05T02:43:24.197Z "),
            Some(t("2026-09-05T02:43:24.197Z"))
        );
        assert_eq!(parse_when("August 2024"), None);
        assert_eq!(parse_when(""), None);
    }

    // --- changes_window -----------------------------------------------------

    /// Someone saying "up to March 31" wants **the 31st included**. The SQL side is `< end`,
    /// so end must fall at midnight on April 1; without this day, asking about the 31st would quietly drop the 31st
    #[test]
    fn until_names_a_day_the_window_must_contain() {
        let (start, end, label) = changes_window(
            Some(d("2026-03-01")),
            Some(d("2026-03-31")),
            t("2026-06-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(start, t("2026-03-01T00:00:00Z"));
        assert_eq!(end, t("2026-04-01T00:00:00Z"));
        // But **April 1 must never appear in the display string**: that is an internal detail of the half-open interval;
        // printing it would tell the model the window is a day wider than requested, and the model would answer on that basis (this really happened)
        assert_eq!(label, "2026-03-01 → 2026-03-31");
    }

    /// When until is not given, the display string says now. Formatting end would mean treating the server clock
    /// as the boundary the user asked for—it looks like a precise answer, but is really just "what time it is now"
    #[test]
    fn an_open_window_says_now_rather_than_the_clock() {
        let now = t("2026-06-01T13:45:00Z");
        let (_, end, label) = changes_window(Some(d("2026-03-01")), None, now).unwrap();
        assert_eq!(end, now);
        assert_eq!(label, "2026-03-01 → now");
    }

    #[test]
    fn without_since_there_is_no_window() {
        assert!(changes_window(None, Some(d("2026-03-31")), t("2026-06-01T00:00:00Z")).is_none());
    }

    // --- change_line --------------------------------------------------------

    #[test]
    fn a_correction_labels_the_old_value_before_the_new_value() {
        let mut c = change("corrected");
        c.subject_name = "acme-api".into();
        c.predicate_label = Some("owned by".into());
        c.previous_object_name = Some("team-a".into());
        c.object_name = Some("team-b".into());
        assert_eq!(
            change_line(&c),
            "2026-08-28T10:00:00Z corrected: acme-api owned by old: team-a → new: team-b"
        );

        c.previous_object_name = None;
        c.object_name = None;
        c.previous_object_value = Some(serde_json::json!({"value": 7, "unit": "kg"}));
        c.object_value = Some(serde_json::json!({"value": 8, "unit": "kg"}));
        assert!(change_line(&c).ends_with("old: 7 kg → new: 8 kg"));
    }

    /// A start at year precision is written as the year, not printed as 1 January; an unknown end is never written as now
    #[test]
    fn a_change_line_shows_each_end_at_its_precision() {
        let mut c = change("asserted");
        c.valid_from = Some(t("2019-01-01T00:00:00Z"));
        c.valid_from_precision = Some("year".into());
        assert!(
            change_line(&c).contains("[valid 2019 → now]"),
            "{}",
            change_line(&c)
        );
        c.valid_to_precision = Some("unknown".into());
        assert!(
            change_line(&c).contains("[valid 2019 → ended, date unknown]"),
            "{}",
            change_line(&c)
        );
    }

    #[test]
    fn a_window_end_covers_the_whole_period_named() {
        assert_eq!(period_last_day(d("2023-01-01"), "year"), d("2023-12-31"));
        assert_eq!(period_last_day(d("2024-02-01"), "month"), d("2024-02-29"));
        assert_eq!(period_last_day(d("2023-12-01"), "month"), d("2023-12-31"));
        assert_eq!(period_last_day(d("2023-06-15"), "day"), d("2023-06-15"));
        // parse_when, the same notation as at
        assert_eq!(parse_when("2023"), Some(t("2023-01-01T00:00:00Z")));
        assert_eq!(parse_when("2023-06"), Some(t("2023-06-01T00:00:00Z")));
    }

    fn change(kind: &str) -> GraphChange {
        GraphChange {
            fact_id: Uuid::nil(),
            at: t("2026-08-28T10:00:00Z"),
            kind: kind.to_string(),
            subject_id: Uuid::nil(),
            subject_name: "Acme".to_string(),
            predicate_label: Some("founded in".to_string()),
            object_name: None,
            object_value: None,
            previous_object_name: None,
            previous_object_value: None,
            valid_from: None,
            valid_to: None,
            // With no dates at either end there is no precision—the fixture must uphold this invariant too (see `facts.valid_from_precision`)
            // Neither end has a date, so neither end has a precision (see the two precision columns on facts)
            valid_from_precision: None,
            valid_to_precision: None,
            confidence: 0.9,
            document_id: None,
            filename: None,
            quote: None,
            quote_origin: None,
        }
    }

    // Within the same second one can also record first and correct later; truncating to the whole second would fold the two beliefs back together.
    #[test]
    fn change_stamps_round_trip_without_losing_the_record_clock() {
        let mut c = change("corrected");
        for stamp in [
            "2026-09-05T02:43:53Z",
            "2026-09-05T02:43:53.382Z",
            "2026-09-05T02:43:53.382001Z",
            "2026-09-05T02:43:53.382002Z",
        ] {
            c.at = t(stamp);
            let line = change_line(&c);
            let printed = line.split_whitespace().next().unwrap();
            assert_eq!(printed, stamp);
            assert_eq!(parse_when(printed), Some(c.at));
        }
    }

    /// `before` subtracts one microsecond at the ledger's resolution, not one second and not one day; the summary says the moment the person asked about
    #[test]
    fn before_is_the_microsecond_before_at_the_ledgers_resolution() {
        let t0 = t("2026-09-05T02:43:53.382Z");
        assert_eq!(just_before(t0), t("2026-09-05T02:43:53.381999Z"));
        assert_eq!(
            just_before(t("2026-09-05T02:43:53Z")),
            t("2026-09-05T02:43:52.999999Z"),
            "小数为零要向秒借位——正是不该让模型算的那一步"
        );
        assert_eq!(
            entity_facts_detail(2, None, Some(just_before(t0)), Some(t0)),
            "2 facts as recorded before 2026-09-05T02:43:53.382Z"
        );
    }

    #[test]
    fn fact_details_keep_the_record_instant_beside_the_world_date() {
        let at = Some(t("2024-08-01T00:00:00Z"));
        let as_of = Some(t("2026-09-05T02:43:53.382001Z"));
        assert_eq!(
            entity_facts_detail(2, at, as_of, None),
            "2 facts at 2024-08-01T00:00:00Z, as recorded by 2026-09-05T02:43:53.382001Z"
        );
        assert_eq!(
            entity_facts_detail(2, None, as_of, None),
            "2 facts as recorded by 2026-09-05T02:43:53.382001Z"
        );
        assert_eq!(
            entity_facts_detail(2, at, None, None),
            "2 facts as of 2024-08-01T00:00:00Z"
        );
        assert_eq!(entity_facts_detail(0, None, None, None), "0 facts");
    }

    fn attribute_fact(value: serde_json::Value) -> EntityFact {
        EntityFact {
            said_as: None,
            recorded_at: chrono::Utc::now(),
            invalidated_at: None,
            supersedes: None,
            document_ids: vec![],
            id: Uuid::nil(),
            direction: "out".into(),
            predicate_key: Some("salary".into()),
            predicate_label: Some("salary".into()),
            inferred: false,
            temporal: Some("state".into()),
            other_id: None,
            other_name: None,
            other_type: None,
            qualifiers: Vec::new(),
            statement_qualifiers: Vec::new(),
            object_value: Some(value),
            valid_from: Some(t("2023-06-01T00:00:00Z")),
            valid_to: Some(t("2024-02-20T00:00:00Z")),
            valid_from_precision: Some("day".into()),
            valid_to_precision: Some("day".into()),
            holds_from: Some(t("2023-06-01T00:00:00Z")),
            holds_to: Some(t("2024-02-20T00:00:00Z")),
            attested_by: None,
            confidence: 0.9,
            evidence_count: 1,
            stale: false,
            corrected: false,
            end_derived: false,
            end_after_period: false,
            time_corrected: false,
            corrected_ends: None,
            closed_by_id: None,
            closed_by: None,
            closed_by_value: None,
            correction_note: None,
            last_evidence_time: None,
            contested: None,
        }
    }

    /// The value of an attribute fact has to reach the model's eyes (#348). Previously this was `salary → ? (2023-06-01 → 2024-02-20)`:
    /// the interval and confidence were there, but the value was not, so the model could only say "no salary information"—even though the ledger had it
    #[test]
    fn an_attribute_fact_shows_the_model_its_value() {
        let line = fact_line(
            &attribute_fact(serde_json::json!({ "value": 28000, "unit": "CNY" })),
            "",
        );
        assert!(line.starts_with("salary → 28000 CNY"), "{line}");
        assert!(line.contains("(2023-06-01 → 2024-02-20)"), "{line}");

        // The same rule as the client's fmtObjectValue: a boolean is drawn as ✓/✗, and a mapping summary reads its summary
        let flag = fact_line(&attribute_fact(serde_json::json!({ "value": true })), "");
        assert!(flag.starts_with("salary → ✓"), "{flag}");
        let mapped = fact_line(
            &attribute_fact(serde_json::json!({ "summary": "orders.total" })),
            "",
        );
        assert!(mapped.starts_with("salary → orders.total"), "{mapped}");
    }

    /// A relation the ontology did not catch is still "?": that question mark is for "no predicate", not for "has a value"
    #[test]
    fn a_missing_object_still_reads_as_a_question_mark() {
        let mut f = attribute_fact(serde_json::json!(null));
        f.object_value = None;
        assert!(
            fact_line(&f, "").starts_with("salary → ?"),
            "{}",
            fact_line(&f, "")
        );
    }

    /// A literal value is read as itself. Going through `Value::to_string()` would print the string with its quotes,
    /// and the model would then treat `"1993"` as part of the answer
    #[test]
    fn a_literal_value_reads_as_itself_not_as_json() {
        let mut c = change("asserted");
        c.object_value = Some(serde_json::json!("1993"));
        assert!(
            change_line(&c).ends_with("Acme founded in 1993"),
            "{}",
            change_line(&c)
        );
    }

    /// The two axes must be **visibly two different things** on the same line: the recording moment first, unlabelled,
    /// and the world-axis interval after it, marked with the word `valid`. If they were a run of bare dates, the model would
    /// read "recorded in 2026" as "happened in 2026"—the very reason this tool exists is to prevent that
    #[test]
    fn the_record_time_and_the_valid_range_do_not_read_as_one_date_run() {
        let mut c = change("corrected");
        c.object_name = Some("Berlin".to_string());
        c.predicate_label = Some("headquartered in".to_string());
        c.valid_from = Some(t("2019-01-01T00:00:00Z"));
        c.valid_from_precision = Some("day".into());
        let line = change_line(&c);
        assert!(
            line.starts_with("2026-08-28T10:00:00Z corrected: "),
            "{line}"
        );
        assert!(line.contains("[valid 2019-01-01 → now]"), "{line}");
    }

    /// With no evidence, say nothing at all. Inventing a from "…" would make an assertion with no provenance
    /// look as if it had provenance
    #[test]
    fn a_fact_with_no_evidence_claims_no_document() {
        let mut c = change("rejected");
        c.object_name = Some("Berlin".to_string());
        assert!(!change_line(&c).contains("from"), "{}", change_line(&c));
    }

    #[test]
    fn evidence_carries_the_filename_and_the_quote() {
        let mut c = change("corrected");
        c.object_name = Some("Berlin".to_string());
        c.filename = Some("annual-report.pdf".to_string());
        c.quote = Some("moved its head office to Berlin".to_string());
        let line = change_line(&c);
        assert!(line.contains("from \"annual-report.pdf\""), "{line}");
        assert!(
            line.contains("\"moved its head office to Berlin\""),
            "{line}"
        );
    }
}
