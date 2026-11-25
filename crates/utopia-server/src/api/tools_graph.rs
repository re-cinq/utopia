//! Graph query tools (#558): `paths_between`, `neighbors`, `timeline`, and
//! sorting in `find_entities`, plus filtering and grouping in `entity_facts`.
//!
//! Previously the model had only two graph actions: find an entity by name, and dump all of an entity's facts.
//! "The relation between OpenAI and Anthropic" required it to wire together hundreds of facts from both sides in its context:
//! it managed it three times out of sixteen; "OpenAI's timeline" was a 433-line dump that it had to sort itself.
//! Here the query work is moved back to the server: paths are searched server-side, neighbours are grouped by predicate, timelines are ordered by
//! world time, and facts can be filtered by predicate, object type and time period.
//!
//! **Entity arguments accept a uuid or a name.** Accepting a name saves a round of find_entities, and the common follow-up
//! after that round, "which OpenAI do you mean?": the ordering rule picks one, and writes who was picked, and who else
//! matched, at the top of the result; if the model disagrees it retries with the id.

use super::tools::{
    entity_facts_detail, fact_line, just_before, literal_text, page_offset, parse_when, Step,
    ToolCtx, ToolResult, ToolSink,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use utopia_core::models::{EntityFact, GraphNode};
use utopia_store::paths::{Limits, Path, PathEdge};
use uuid::Uuid;

const NEIGHBORS_DEFAULT: usize = 40;
const TIMELINE_DEFAULT: usize = 60;
const FACTS_DEFAULT: usize = 80;
const LIST_MAX: usize = 300;

// ---- Entity arguments -------------------------------------------------------------------

/// Sorting for name matches: **type-identified ones first, exact same-name hits first, then the ones with more facts**.
///
/// Phrases with no type ("lawsuit against OpenAI", #559) are left out when other candidates exist: almost all of them are
/// descriptions that extraction let in by mistake, and listing them would only turn the question into one more disambiguation. When all are untyped they are listed, so a
/// library in which no type was identified at all must still be usable. **Those with this exact name are always listed, typed or not**: the question is
/// "Acme", and while Acme itself has not yet been typed, leaving only "Acme lawsuit" would point the person at the wrong entity;
/// untyped entities must still be findable (0009).
pub(super) fn rank(mut hits: Vec<GraphNode>, query: &str) -> Vec<GraphNode> {
    let q = query.trim().to_lowercase();
    let exact = |n: &GraphNode| n.name.trim().to_lowercase() == q;
    if hits.iter().any(|n| n.type_label.is_some()) {
        hits.retain(|n| n.type_label.is_some() || exact(n));
    }
    hits.sort_by(|a, b| {
        exact(b)
            .cmp(&exact(a))
            .then(b.degree.cmp(&a.degree))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits
}

/// Whether the first place is clearly the one: only it, or only it as an exact hit, or its fact count is three times the second place's
pub(super) fn dominant(ranked: &[GraphNode], query: &str) -> bool {
    let q = query.trim().to_lowercase();
    match ranked {
        [] => false,
        [_] => true,
        [first, second, ..] => {
            let exact = |n: &GraphNode| n.name.trim().to_lowercase() == q;
            (exact(first) && !exact(second)) || first.degree >= 3 * second.degree.max(1)
        }
    }
}

fn node_line(n: &GraphNode) -> String {
    let dis = n
        .disambiguator
        .as_deref()
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    format!(
        "{} | {}{} | {} | {} facts",
        n.id,
        n.name,
        dis,
        // Entities whose type was not identified are still found and referenced (0009)
        n.type_label.as_deref().unwrap_or("untyped"),
        n.degree
    )
}

/// Records the entities accepted in this turn. The next round is told "use these ids directly, do not look up by name again", so here it
/// may only hold ones that were actually read, or the one the search explicitly pointed to: within one turn, first accepted by name and then read by id, record only once
fn remember(sink: &mut ToolSink, n: &GraphNode) {
    let id = n.id.to_string();
    if sink.resolved.iter().any(|e| e["id"] == id.as_str()) {
        return;
    }
    sink.resolved.push(json!({
        "id": id, "name": n.name, "type": n.type_label
    }));
}

/// Resolve one entity argument to an id. The name goes through [`rank`]; whoever was picked is written into `note` and handed to the top of the result
struct Resolved {
    id: Uuid,
    name: String,
    note: Option<String>,
    /// The argument is already an id: it was not looked up in the library, and `name` is just that id string
    by_id: bool,
}

enum ResolveError {
    Unresolved(String),
    ReadFailed,
}

async fn resolve(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    raw: &str,
) -> Result<Resolved, ResolveError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ResolveError::Unresolved(
            "an entity name or id is required".to_string(),
        ));
    }
    if let Ok(id) = raw.parse::<Uuid>() {
        return Ok(Resolved {
            id,
            name: raw.to_string(),
            note: None,
            by_id: true,
        });
    }
    let (hits, _) = lookup(ctx, raw).await.map_err(|e| {
        tracing::warn!(error = %e, "Entity lookup failed");
        ResolveError::ReadFailed
    })?;
    let (ranked, by_question) = rank_by_question(ctx, sink, rank(hits, raw), raw).await;
    let Some(first) = ranked.first() else {
        return Err(ResolveError::Unresolved(format!(
            "no entity named \"{raw}\" in this base"
        )));
    };
    remember(sink, first);
    let others: Vec<String> = ranked
        .iter()
        .skip(1)
        .take(4)
        .map(|n| {
            format!(
                "{} ({}, {} facts, {})",
                n.name,
                n.type_label.as_deref().unwrap_or("untyped"),
                n.degree,
                n.id
            )
        })
        .collect();
    let mut note = format!(
        "\"{raw}\" = {} ({}, {} facts)",
        first.name,
        first.type_label.as_deref().unwrap_or("untyped"),
        first.degree
    );
    if by_question {
        note.push_str(", closest to the question");
    }
    if !others.is_empty() {
        note.push_str(&format!(
            "; other matches: {}. Pass an id to choose another.",
            others.join("; ")
        ));
    }
    Ok(Resolved {
        id: first.id,
        name: first.name.clone(),
        note: Some(note),
        by_id: false,
    })
}

/// Time parameter trio: `at` for the world axis, `as_of` / `before` for the record axis (before takes precedence, minus one microsecond)
struct Moments {
    at: Option<chrono::DateTime<chrono::Utc>>,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    before: Option<chrono::DateTime<chrono::Utc>>,
}

fn moments(args: &Value) -> Moments {
    let at = args["at"].as_str().and_then(parse_when);
    let before = args["before"].as_str().and_then(parse_when);
    let as_of = match before {
        Some(t) => Some(just_before(t)),
        None => args["as_of"].as_str().and_then(parse_when),
    };
    Moments { at, as_of, before }
}

fn limit_of(args: &Value, default: usize) -> usize {
    args["limit"]
        .as_u64()
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(default)
        .min(LIST_MAX)
}

fn contains_ci(haystack: Option<&str>, needle: &str) -> bool {
    haystack.is_some_and(|h| h.to_lowercase().contains(&needle.to_lowercase()))
}

// ---- evidence marks (#935) -------------------------------------------------------

/// For each fact displayed this time, its first valid evidence (#935).
///
/// The `[n]` of the fact is printed on the fact line, and the evidence block is registered in this turn's source list; clicking it in the UI shows that sentence. Previously
/// an answer produced by the graph had no number at all, and the UI could only say "no source cited". Registration happens when a line is printed,
/// so numbers follow the order of appearance and share one numbering with retrieval results (one block, one number).
///
/// Empty when there is no source list (MCP), or when the evidence cannot be read: fact lines are unchanged and carry no number.
///
/// Each fact also carries that evidence's quote (follow-up to #968): the number opens that block, and the entry's `quotes`
/// is the sentence in the block that states it, which the UI uses to highlight that sentence
struct Evidence(HashMap<Uuid, (utopia_core::models::ChunkView, Option<String>)>);

impl Evidence {
    /// Collect the fact ids into the function's own `Vec` before passing them in: holding the display list's iterator across an await would stop the tool's
    /// future from being `Send` for every lifetime (both the conversation hook and the MCP route require it)
    async fn load(
        ctx: &ToolCtx<'_>,
        fact_ids: Vec<Uuid>,
        as_of: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        if !ctx.has_source_list() {
            return Self(HashMap::new());
        }
        match utopia_store::graph::first_live_evidence(&ctx.state.pool, ctx.kb_id, &fact_ids, as_of)
            .await
        {
            Ok(rows) => Self(
                rows.into_iter()
                    .map(|(fact, chunk, quote)| (fact, (chunk, quote)))
                    .collect(),
            ),
            Err(e) => {
                tracing::warn!(error = %e, "Evidence lookup for citation marks failed");
                Self(HashMap::new())
            }
        }
    }

    /// The number of the line that closes this row (#970 step 2): the inferred end point is written in that fact's original sentence
    fn closer_mark(&self, sink: &mut ToolSink, f: &EntityFact) -> String {
        f.closed_by_id
            .map(|id| self.mark(sink, id))
            .unwrap_or_default()
    }

    /// ` [n]`: the number the fact's evidence gets once it is entered in the list. A fact with no valid evidence (derived facts too) carries no number
    fn mark(&self, sink: &mut ToolSink, fact_id: Uuid) -> String {
        match self.0.get(&fact_id) {
            Some((chunk, quote)) => {
                let n = super::tools::cite(sink, chunk.id.to_string(), |n| {
                    super::tools::source_json(n, chunk)
                });
                if let Some(quote) = quote {
                    super::tools::add_quote(sink, n, quote);
                }
                format!(" [{n}]")
            }
            None => String::new(),
        }
    }
}

/// The facts shown, plus the lines that close them (#970 step 2): one evidence query reads back the blocks on both sides
fn with_closers<'f>(facts: impl IntoIterator<Item = &'f EntityFact>) -> Vec<Uuid> {
    facts
        .into_iter()
        .flat_map(|f| std::iter::once(f.id).chain(f.closed_by_id))
        .collect()
}

// ---- find_entities ---------------------------------------------------------------

pub async fn find_entities(ctx: &ToolCtx<'_>, sink: &mut ToolSink, args: &Value) -> ToolResult {
    let name = args["name"].as_str().unwrap_or("").to_string();
    let (hits, total) = match lookup(ctx, &name).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "Entity lookup failed");
            return ToolResult::new(
                "Could not look up entities.".into(),
                Step::new("entity", name, "failed").status("failed").json(),
            )
            .error();
        }
    };
    let read = hits.len();
    let (ranked, by_question) = rank_by_question(ctx, sink, rank(hits, &name), &name).await;
    let clear = by_question || dominant(&ranked, &name);
    let mut text = if ranked.is_empty() {
        "No matching entities.".to_string()
    } else if clear {
        let mut lines = vec![format!("Best match: {}", node_line(&ranked[0]))];
        if ranked.len() > 1 {
            lines.push("Other matches:".to_string());
            lines.extend(ranked[1..].iter().map(node_line));
        }
        lines.join("\n")
    } else {
        let mut lines = vec![format!(
            "Several entities match \"{name}\"; pick by type and fact count, or ask the user:"
        )];
        lines.extend(ranked.iter().map(node_line));
        lines.join("\n")
    };
    // Truncation must be stated (same as `rule_matches`): if only the first few are read, the model will take them as this
    // name's full set and then tell a person "the library has only these"
    if let Some(total) = total.filter(|t| *t > read as i64) {
        text.push_str(&format!(
            "\n({total} entities have a name containing \"{name}\"; only the first {read} were \
             read. A longer or more exact name narrows the search.)"
        ));
    }
    // With same-name ambiguity, candidates are only search results, not accepted entities. Previously all were recorded here, so the next round received a list
    // of same-named ids and a "use these directly" line, and the one not chosen often still ranked first. The chosen one is recorded when it is
    // read (entity_facts / neighbors / timeline); the previous turn's candidates can be looked up in the replay of
    // the earlier tool round-trip
    if let Some(best) = ranked.first().filter(|_| clear) {
        remember(sink, best);
    }
    let mut step =
        Step::new("entity", name.as_str(), format!("{} matches", ranked.len())).count(ranked.len());
    if let Some(total) = total.filter(|t| *t > read as i64) {
        step = step.total(total as usize);
    }
    ToolResult::new(text, step.json()).structured(json!({
        "kb_id": ctx.kb_id,
        "matched_entities": total, "truncated": total.is_some_and(|t| t > read as i64),
        "entities": ranked.iter().map(|n| json!({
            "id": n.id, "name": n.name, "type_key": n.type_key,
            "type_label": n.type_label, "disambiguator": n.disambiguator,
            "fact_count": n.degree,
        })).collect::<Vec<_>>()
    }))
}

// ---- entity_facts ----------------------------------------------------------------

/// The other end of the fact: the counterpart entity, or an attribute value
/// Attributes on the edge (0037) follow the counterpart: `Vega Capital [amount: 5000000000 $]`.
/// The question the model asks most often when reading fact lines is "how much was invested"; if the amount is not on the line it answers "no amount information".
/// Attributes recorded by document-role word (0061) follow, named by the role word: `[lines: 10-42]`
fn qualifiers_text(f: &EntityFact) -> String {
    let typed = f
        .qualifiers
        .iter()
        .map(|q| qualifier_text(&q.key, q.value.as_ref(), q.entity_name.as_deref()));
    let worded = f
        .statement_qualifiers
        .iter()
        .map(|q| qualifier_text(&q.role, q.value.as_ref(), q.entity_name.as_deref()));
    let parts: Vec<String> = typed.chain(worded).collect();
    if parts.is_empty() {
        return String::new();
    }
    format!(" [{}]", parts.join(", "))
}

fn qualifier_text(name: &str, value: Option<&Value>, entity_name: Option<&str>) -> String {
    let v = value
        .and_then(|v| v.get("value"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .or_else(|| entity_name.map(String::from))
        .unwrap_or_else(|| "?".to_string());
    let u = value
        .and_then(|v| v.get("unit"))
        .and_then(|u| u.as_str())
        .map(|u| format!(" {u}"))
        .unwrap_or_default();
    format!("{name}: {v}{u}")
}

fn other_text(f: &EntityFact) -> String {
    let literal = f
        .object_value
        .as_ref()
        .and_then(literal_text)
        .filter(|_| f.other_name.is_none());
    f.other_name
        .as_deref()
        .or(literal.as_deref())
        .unwrap_or("?")
        .to_string()
}

/// `closer_mark` is the number of the evidence line that closes it (#970 step 2; empty for MCP and when there is no evidence)
fn range_text(f: &EntityFact, closer_mark: &str) -> String {
    let successor =
        super::tools::successor_text(f.closed_by.as_deref(), f.closed_by_value.as_ref());
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
    if range.is_empty() {
        range
    } else {
        format!(" ({range})")
    }
}

fn confidence_text(f: &EntityFact) -> String {
    format!("[{}%]", (f.confidence * 100.0).round() as i32)
}

/// Header of a predicate group: outgoing edges `pred →`, incoming edges `← pred`
fn group_key(f: &EntityFact) -> String {
    let pred = f.predicate_label.as_deref().unwrap_or("?");
    if f.direction == "out" {
        format!("{pred} →")
    } else {
        format!("← {pred}")
    }
}

/// Group by predicate; larger groups come first; within a group the input order (time order) is kept. Returns (group name, that group's facts)
pub(super) fn grouped(facts: &[EntityFact]) -> Vec<(String, Vec<&EntityFact>)> {
    let mut groups: BTreeMap<String, Vec<&EntityFact>> = BTreeMap::new();
    for f in facts {
        groups.entry(group_key(f)).or_default().push(f);
    }
    let mut out: Vec<(String, Vec<&EntityFact>)> = groups.into_iter().collect();
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    out
}

/// When a filter matches nothing, tell the model which predicates this entity has: the words it guesses ("board member") and
/// the words in the library (`comprised`, `has_member`) often do not match (#560), and returning empty-handed would only make it guess again
pub(super) fn predicates_of(facts: &[EntityFact]) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for f in facts {
        *counts.entry(group_key(f)).or_default() += 1;
    }
    let mut named: Vec<(String, usize)> = counts.into_iter().collect();
    named.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    named
        .iter()
        .take(20)
        .map(|(k, n)| format!("{k} ({n})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// When substring search finds nothing, search by word: each word is looked up in the library separately, and a candidate counts as a match
/// if the number of its words it contains reaches "all but one, and at least two" (`"OpenAI board members"` → `"OpenAI's board of directors"`).
/// Names the model gives often include a word the library does not have (members, company, this); whole-word matching would miss all of those
///
/// returns the entities found and how many matched this name in total. Only the first eight are read; the total tells the model this is not
/// everything. The word-by-word path has no total
async fn lookup(
    ctx: &ToolCtx<'_>,
    raw: &str,
) -> utopia_core::AppResult<(Vec<GraphNode>, Option<i64>)> {
    // Tool calls come from chat / MCP: the question of the present, not replay. Passing None lets `degree` be computed
    // as of now, consistent with the status quo; the search box on the replay graph goes through `/kbs/{id}/entities` and attaches its own
    // time
    let (hits, total) =
        utopia_store::graph::search_entities(&ctx.state.pool, ctx.kb_id, raw, 8, 0, None, false)
            .await?;
    if !hits.is_empty() {
        return Ok((hits, Some(total)));
    }
    let words: Vec<&str> = raw.split_whitespace().filter(|w| w.len() >= 2).collect();
    if words.len() < 2 {
        return Ok((hits, Some(total)));
    }
    let mut pool: Vec<GraphNode> = Vec::new();
    for w in &words {
        let (found, _) =
            utopia_store::graph::search_entities(&ctx.state.pool, ctx.kb_id, w, 40, 0, None, false)
                .await?;
        for n in found {
            if !pool.iter().any(|p| p.id == n.id) {
                pool.push(n);
            }
        }
    }
    let need = words.len().saturating_sub(1).max(2);
    let mut scored: Vec<(usize, GraphNode)> = pool
        .into_iter()
        .map(|n| (word_score(&n.name, &words), n))
        .filter(|(s, _)| *s >= need)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.degree.cmp(&a.1.degree)));
    Ok((scored.into_iter().map(|(_, n)| n).take(8).collect(), None))
}

/// Number of words in the name (case-insensitive)
pub(super) fn word_score(name: &str, words: &[&str]) -> usize {
    let lower = name.to_lowercase();
    words
        .iter()
        .filter(|w| lower.contains(&w.to_lowercase()))
        .count()
}

/// Same-name candidates are ranked by the question: the user's question is embedded into a vector and compared with each candidate's context profile, nearest first. The question is
/// embedded only once per turn (`embed_once`): within one turn, each by-name entity lookup comes here, yet the sentence is the same one.
/// Exact hits still come first (asking about OpenAI puts the one named OpenAI first), and profiles only order within the same tier.
/// With no question (MCP), no embedding model, or only one candidate: return as-is; the second value says whether it was used
async fn rank_by_question(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    ranked: Vec<GraphNode>,
    query: &str,
) -> (Vec<GraphNode>, bool) {
    if ranked.len() < 2 {
        return (ranked, false);
    }
    let Some(question) = ctx.question else {
        return (ranked, false);
    };
    let Some(vec) = ctx.embed_once(sink, question).await else {
        return (ranked, false);
    };
    let ids: Vec<Uuid> = ranked.iter().map(|n| n.id).collect();
    let Ok(dist) =
        utopia_store::graph::profile_distances(&ctx.state.pool, ctx.kb_id, &ids, &vec).await
    else {
        return (ranked, false);
    };
    let dist: HashMap<Uuid, f64> = dist.into_iter().collect();
    (reorder_by_distance(ranked, query, &dist), true)
}

/// Pure function: the exact-hit tier comes first; within a tier, ascending by profile distance, with no profile at the bottom; then by fact count
pub(super) fn reorder_by_distance(
    mut ranked: Vec<GraphNode>,
    query: &str,
    dist: &HashMap<Uuid, f64>,
) -> Vec<GraphNode> {
    let q = query.trim().to_lowercase();
    let exact = |n: &GraphNode| n.name.trim().to_lowercase() == q;
    ranked.sort_by(|a, b| {
        exact(b)
            .cmp(&exact(a))
            .then_with(|| {
                let da = dist.get(&a.id).copied().unwrap_or(f64::MAX);
                let db = dist.get(&b.id).copied().unwrap_or(f64::MAX);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then(b.degree.cmp(&a.degree))
    });
    ranked
}

/// Align the predicate argument with the library's words: the model says "board member", the library calls it `has_member`. When substrings do not match,
/// embed it once and take the nearest relation type (relation vectors live in `relation_types.embedding`); those within the distance
/// limit count as the ones it meant. Relations auto-expanded from the ontology have no vector (#560), so they cannot be matched
const PREDICATE_DISTANCE: f32 = 0.45;
const PREDICATE_CANDIDATES: i64 = 5;

async fn aligned_predicates(ctx: &ToolCtx<'_>, sink: &mut ToolSink, word: &str) -> Vec<String> {
    let Some(vec) = ctx.embed_once(sink, word).await else {
        return Vec::new();
    };
    let near = utopia_store::ontology::nearest_relation_types(
        &ctx.state.pool,
        ctx.kb_id,
        &vec,
        PREDICATE_CANDIDATES,
        None,
    )
    .await
    .unwrap_or_default();
    for c in &near {
        tracing::debug!(word, key = c.key, distance = c.distance, "谓词对齐候选");
    }
    near.into_iter()
        .filter(|c| c.distance <= PREDICATE_DISTANCE)
        .map(|c| c.key)
        .collect()
}

/// Filter; when a predicate does not match by substring, align it by vector once and return a note at the top of the result
async fn filtered<'f>(
    ctx: &ToolCtx<'_>,
    sink: &mut ToolSink,
    facts: &'f [EntityFact],
    filter: &FactFilter<'_>,
) -> (Vec<&'f EntityFact>, Option<String>) {
    let kept: Vec<&EntityFact> = facts.iter().filter(|f| filter.keeps(f)).collect();
    let Some(word) = filter.predicate else {
        return (kept, None);
    };
    if !kept.is_empty() || facts.is_empty() {
        return (kept, None);
    }
    let keys: HashSet<String> = aligned_predicates(ctx, sink, word)
        .await
        .into_iter()
        .collect();
    if keys.is_empty() {
        return (kept, None);
    }
    let widened = FactFilter {
        predicate: None,
        ..*filter
    };
    let kept: Vec<&EntityFact> = facts
        .iter()
        .filter(|f| {
            widened.keeps(f) && f.predicate_key.as_deref().is_some_and(|k| keys.contains(k))
        })
        .collect();
    let mut names: Vec<&str> = keys.iter().map(String::as_str).collect();
    names.sort();
    let note = format!("predicate \"{word}\" read as {}", names.join(", "));
    (kept, Some(note))
}

#[derive(Clone, Copy)]
struct FactFilter<'a> {
    predicate: Option<&'a str>,
    object_type: Option<&'a str>,
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
}

impl FactFilter<'_> {
    fn from_args(args: &Value) -> FactFilter<'_> {
        FactFilter {
            predicate: args["predicate"].as_str().filter(|s| !s.trim().is_empty()),
            object_type: args["object_type"]
                .as_str()
                .filter(|s| !s.trim().is_empty()),
            since: args["since"].as_str().and_then(parse_when),
            until: args["until"].as_str().and_then(parse_when),
        }
    }

    fn narrows(&self) -> bool {
        self.predicate.is_some()
            || self.object_type.is_some()
            || self.since.is_some()
            || self.until.is_some()
    }

    /// The time period uses the **stated** interval: the start is before `until`, the end is after `since` (an open end counts as later)
    fn keeps(&self, f: &EntityFact) -> bool {
        if let Some(p) = self.predicate {
            if !contains_ci(f.predicate_key.as_deref(), p)
                && !contains_ci(f.predicate_label.as_deref(), p)
            {
                return false;
            }
        }
        if let Some(t) = self.object_type {
            if !contains_ci(f.other_type.as_deref(), t) {
                return false;
            }
        }
        if let Some(until) = self.until {
            if f.valid_from.is_some_and(|from| from > until) {
                return false;
            }
        }
        if let Some(since) = self.since {
            if f.valid_to.is_some_and(|to| to < since) {
                return false;
            }
        }
        true
    }
}

pub async fn entity_facts(ctx: &ToolCtx<'_>, sink: &mut ToolSink, args: &Value) -> ToolResult {
    let offset = match page_offset(args, "facts") {
        Ok(offset) => offset,
        Err(result) => return result,
    };
    let raw = args["entity_id"]
        .as_str()
        .or_else(|| args["entity"].as_str())
        .unwrap_or("");
    let who = match resolve(ctx, sink, raw).await {
        Ok(r) => r,
        Err(ResolveError::ReadFailed) => {
            return ToolResult::new(
                "Could not look up entities.".into(),
                Step::new("facts", "?", "failed").status("failed").json(),
            )
            .error();
        }
        Err(ResolveError::Unresolved(e)) => {
            return ToolResult::new(
                format!(
                    "Invalid entity: {e} (expected a name, or the uuid returned by find_entities)."
                ),
                Step::new("facts", "?", "invalid id")
                    .status("not_found")
                    .json(),
            )
            .error()
        }
    };
    let m = moments(args);
    let filter = FactFilter::from_args(args);
    let limit = limit_of(args, FACTS_DEFAULT);
    let (node, facts) =
        match utopia_store::graph::entity_detail(&ctx.state.pool, ctx.kb_id, who.id, m.at, m.as_of)
            .await
        {
            Ok(detail) => detail,
            Err(e) => {
                let text = if matches!(e, utopia_core::AppError::NotFound) {
                    "Entity not found."
                } else {
                    tracing::warn!(error = %e, "Entity facts lookup failed");
                    "Could not read the entity facts."
                };
                return ToolResult::new(
                    text.into(),
                    Step::new("facts", "?", "failed").status("failed").json(),
                )
                .error();
            }
        };
    // Reading by id is also acceptance: the model often searches first and gets a run of same-named entities, then reads one by id; the one it reads
    // is the one this turn is about (the name-based `resolve` already recorded it, so it is not recorded twice here)
    remember(sink, &node);
    // The rule's conclusion is also part of this entity (0021). **If not given, the model would judge those readings again itself**
    // and the threshold is written into the rule, which it cannot see, so the two judgements would eventually disagree
    let derived = match
        // Both axes are passed together (#549): as_of goes back to March, and the derivation goes back to March too
        utopia_store::reasoning::derived_for_entity(
            &ctx.state.pool,
            ctx.kb_id,
            who.id,
            m.at,
            m.as_of,
        )
        .await {
            Ok(derived) => derived,
            Err(e) => {
                tracing::warn!(error = %e, "Derived facts lookup failed");
                return ToolResult::new("Could not read the derived facts.".into(),
                    Step::new("facts", node.name, "failed").status("failed").json()).error();
            }
        };
    let derived_lines: Vec<String> = derived
        .iter()
        .map(|d| {
            format!(
                "{} · {} · {} [rule: {}]",
                d.subject,
                d.predicate,
                d.object,
                d.rule_name.as_deref().unwrap_or(&d.rule),
            )
        })
        .collect();

    // Names on their own line (0041): "Haitan 1" and "Ocean Explorer No. 1" are the same, and the model must know this when answering
    let names = utopia_store::names::for_entity(&ctx.state.pool, ctx.kb_id, who.id, m.as_of)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Entity names lookup failed");
            Vec::new()
        });
    let (kept, aligned) = filtered(ctx, sink, &facts, &filter).await;
    let shown: Vec<&EntityFact> = kept.iter().copied().skip(offset).take(limit).collect();
    let end = offset + shown.len();
    let next_offset = (end < kept.len()).then_some(end);
    let mut lines: Vec<String> = Vec::new();
    if let Some(note) = &who.note {
        lines.push(note.clone());
    }
    let other_names: Vec<&str> = names
        .iter()
        .filter(|n| !n.canonical)
        .map(|n| n.name.as_str())
        .collect();
    if !other_names.is_empty() {
        lines.push(format!("Also known as: {}", other_names.join(", ")));
    }
    if let Some(a) = &aligned {
        lines.push(a.clone());
    }
    let type_label = node.type_label.as_deref().unwrap_or("untyped");
    if facts.is_empty() && derived.is_empty() {
        lines.push(match m.at {
            Some(t) => format!(
                "{}: no facts valid as of {}.",
                node.name,
                crate::time_text::instant(t)
            ),
            None => format!("{}: no recorded facts.", node.name),
        });
    } else {
        let mut head = format!("{} ({type_label}) · {} facts", node.name, facts.len());
        if filter.narrows() {
            head.push_str(&format!(", {} match the filter", kept.len()));
            if kept.is_empty() {
                head.push_str(&format!(
                    ". Predicates on this entity: {}",
                    predicates_of(&facts)
                ));
            }
        }
        if offset > 0 || shown.len() < kept.len() {
            head.push_str(&format!(", {} shown at offset {offset}", shown.len()));
        }
        if let Some(next) = next_offset {
            head.push_str(&format!(
                ". Call entity_facts with offset={next} and the same entity, filters and time parameters to continue"
            ));
        }
        lines.push(head);
        let ids = with_closers(shown.iter().copied());
        let evidence = Evidence::load(ctx, ids, m.as_of).await;
        let shown_owned: Vec<EntityFact> = shown.iter().map(|f| (*f).clone()).collect();
        for (key, group) in grouped(&shown_owned) {
            lines.push(format!("## {key} ({})", group.len()));
            for f in group {
                let closer = evidence.closer_mark(sink, f);
                lines.push(format!(
                    "{}{}{} {}{}",
                    other_text(f),
                    qualifiers_text(f),
                    range_text(f, &closer),
                    confidence_text(f),
                    evidence.mark(sink, f.id)
                ));
            }
        }
        // Derived facts carry no number of their own: they are computed by a rule, and the source sits on their premises, each premise carrying its own
        lines.extend(derived_lines);
    }
    let detail = entity_facts_detail(shown.len(), m.at, m.as_of, m.before);
    ToolResult::new(
        lines.join("\n"),
        Step::new("facts", node.name.as_str(), detail)
            .count(shown.len())
            .moments(m.at, m.as_of, m.before)
            .json(),
    )
    .structured(json!({
        "kb_id": ctx.kb_id,
        "entity": {"id": node.id, "name": node.name,
            "type_key": node.type_key, "type_label": node.type_label},
        "names": names.iter().map(|n| json!({
            "fact_id": n.fact_id, "name": n.name, "canonical": n.canonical,
            "recorded_at": n.recorded_at, "valid_from": n.valid_from, "valid_to": n.valid_to,
            "document_ids": n.document_ids,
        })).collect::<Vec<_>>(),
        "at": m.at, "as_of": m.as_of, "before": m.before,
        "total_facts": facts.len(), "matched_facts": kept.len(),
        "limit": limit, "offset": offset, "next_offset": next_offset,
        "truncated": next_offset.is_some(),
        "facts": shown.iter().map(|f| json!({
            "id": f.id, "direction": f.direction,
            "predicate_key": f.predicate_key, "predicate_label": f.predicate_label,
            "inferred": f.inferred, "temporal": f.temporal,
            "other_id": f.other_id, "other_name": f.other_name,
            "object_value": f.object_value, "confidence": f.confidence,
            "qualifiers": f.qualifiers.iter().map(|q| json!({
                "qualifier_type_id": q.qualifier_type_id, "key": q.key, "label": q.label,
                "value": q.value, "entity_id": q.entity_id, "entity_name": q.entity_name,
            })).collect::<Vec<_>>(),
            "statement_qualifiers": f.statement_qualifiers,
            "valid_from": f.valid_from, "valid_to": f.valid_to,
            "valid_from_precision": f.valid_from_precision,
            "valid_to_precision": f.valid_to_precision,
            "holds_from": f.holds_from, "holds_to": f.holds_to,
            "recorded_at": f.recorded_at, "invalidated_at": f.invalidated_at,
            "supersedes": f.supersedes, "document_ids": f.document_ids,
        })).collect::<Vec<_>>(),
        "derived_facts": derived.iter().map(|d| json!({
            "id": d.id, "subject_id": d.subject_id, "subject": d.subject,
            "predicate_id": d.predicate_id, "predicate": d.predicate,
            "object_id": d.object_id, "object": d.object, "object_value": d.object_value,
            "rule": d.rule, "rule_name": d.rule_name,
            "rule_id": d.rule_id, "attribute_rule_id": d.attribute_rule_id,
            "valid_from": d.valid_from, "valid_to": d.valid_to,
            "valid_from_precision": d.valid_from_precision,
            "valid_to_precision": d.valid_to_precision,
            "derived_at": d.derived_at, "invalidated_at": d.invalidated_at,
            "confidence": d.confidence,
        })).collect::<Vec<_>>()
    }))
}

// ---- neighbors -------------------------------------------------------------------

pub async fn neighbors(ctx: &ToolCtx<'_>, sink: &mut ToolSink, args: &Value) -> ToolResult {
    let raw = args["entity"]
        .as_str()
        .or_else(|| args["entity_id"].as_str())
        .unwrap_or("");
    let who = match resolve(ctx, sink, raw).await {
        Ok(r) => r,
        Err(ResolveError::ReadFailed) => {
            return ToolResult::new(
                "Could not look up entities.".into(),
                Step::new("neighbors", "?", "failed")
                    .status("failed")
                    .json(),
            )
            .error();
        }
        Err(ResolveError::Unresolved(e)) => {
            return ToolResult::new(
                format!("Unknown entity: {e}."),
                Step::new("neighbors", "?", "unknown entity")
                    .status("not_found")
                    .json(),
            )
        }
    };
    let m = moments(args);
    let filter = FactFilter::from_args(args);
    let limit = limit_of(args, NEIGHBORS_DEFAULT);
    let (node, facts) =
        match utopia_store::graph::entity_detail(&ctx.state.pool, ctx.kb_id, who.id, m.at, m.as_of)
            .await
        {
            Ok(detail) => detail,
            Err(utopia_core::AppError::NotFound) => {
                return ToolResult::new(
                    "Entity not found.".to_string(),
                    Step::new("neighbors", "?", "not found")
                        .status("not_found")
                        .json(),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Entity neighbors lookup failed");
                return ToolResult::new(
                    "Could not read the entity facts.".into(),
                    Step::new("neighbors", "?", "failed")
                        .status("failed")
                        .json(),
                )
                .error();
            }
        };
    remember(sink, &node);
    // Neighbours are the counterpart **entities**; attribute values do not count as neighbours, entity_facts has them
    let (matched, aligned) = filtered(ctx, sink, &facts, &filter).await;
    let linked: Vec<&EntityFact> = matched
        .into_iter()
        .filter(|f| f.other_id.is_some())
        .collect();
    let shown: Vec<EntityFact> = linked.iter().take(limit).map(|f| (*f).clone()).collect();
    let mut lines: Vec<String> = Vec::new();
    if let Some(note) = &who.note {
        lines.push(note.clone());
    }
    if let Some(a) = &aligned {
        lines.push(a.clone());
    }
    let type_label = node.type_label.as_deref().unwrap_or("untyped");
    if shown.is_empty() {
        let all_linked: Vec<EntityFact> = facts
            .iter()
            .filter(|f| f.other_id.is_some())
            .cloned()
            .collect();
        let hint = if filter.narrows() && !all_linked.is_empty() {
            format!(" Predicates on this entity: {}", predicates_of(&all_linked))
        } else {
            String::new()
        };
        lines.push(format!(
            "{} ({type_label}): no linked entities{}.{hint}",
            node.name,
            if filter.narrows() {
                " match the filter"
            } else {
                ""
            }
        ));
    } else {
        let ids = with_closers(shown.iter());
        let evidence = Evidence::load(ctx, ids, m.as_of).await;
        let groups = grouped(&shown);
        // What is counted is the counterpart entity, not facts: the same edge is often two facts (one with a date, one without)
        let entities: HashSet<Uuid> = linked.iter().filter_map(|f| f.other_id).collect();
        let mut head = format!(
            "{} ({type_label}): {} linked entities under {} predicates",
            node.name,
            entities.len(),
            groups.len()
        );
        if shown.len() < linked.len() {
            head.push_str(&format!(
                " ({} shown; narrow with predicate= or object_type=)",
                shown.len()
            ));
        }
        lines.push(head);
        for (key, group) in groups {
            let items: Vec<String> = group
                .iter()
                .map(|f| {
                    let ty = f
                        .other_type
                        .as_deref()
                        .map(|t| format!(" [{t}]"))
                        .unwrap_or_default();
                    let closer = evidence.closer_mark(sink, f);
                    format!(
                        "{}{}{} {}{}",
                        other_text(f),
                        ty,
                        range_text(f, &closer),
                        confidence_text(f),
                        evidence.mark(sink, f.id)
                    )
                })
                .collect();
            lines.push(format!("{key} {}", items.join(" · ")));
        }
    }
    let detail = format!("{} of {} linked", shown.len(), linked.len());
    ToolResult::new(
        lines.join("\n"),
        Step::new("neighbors", node.name, detail)
            .count(shown.len())
            .total(linked.len())
            .json(),
    )
}

// ---- timeline --------------------------------------------------------------------

pub async fn timeline(ctx: &ToolCtx<'_>, sink: &mut ToolSink, args: &Value) -> ToolResult {
    let raw = args["entity"]
        .as_str()
        .or_else(|| args["entity_id"].as_str())
        .unwrap_or("");
    let who = match resolve(ctx, sink, raw).await {
        Ok(r) => r,
        Err(ResolveError::ReadFailed) => {
            return ToolResult::new(
                "Could not look up entities.".into(),
                Step::new("timeline", "?", "failed").status("failed").json(),
            )
            .error();
        }
        Err(ResolveError::Unresolved(e)) => {
            return ToolResult::new(
                format!("Unknown entity: {e}."),
                Step::new("timeline", "?", "unknown entity")
                    .status("not_found")
                    .json(),
            )
        }
    };
    let m = moments(args);
    let filter = FactFilter::from_args(args);
    let limit = limit_of(args, TIMELINE_DEFAULT);
    let (node, facts) =
        match utopia_store::graph::entity_detail(&ctx.state.pool, ctx.kb_id, who.id, None, m.as_of)
            .await
        {
            Ok(detail) => detail,
            Err(utopia_core::AppError::NotFound) => {
                return ToolResult::new(
                    "Entity not found.".to_string(),
                    Step::new("timeline", "?", "not found")
                        .status("not_found")
                        .json(),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Entity timeline lookup failed");
                return ToolResult::new(
                    "Could not read the entity facts.".into(),
                    Step::new("timeline", "?", "failed").status("failed").json(),
                )
                .error();
            }
        };
    remember(sink, &node);
    // Only facts that **state a time in the world**. Those with no date have their start at the ingestion moment, and putting them on the timeline would
    // treat the date of an article as the date of the event
    let (matched, aligned) = filtered(ctx, sink, &facts, &filter).await;
    let mut dated: Vec<&EntityFact> = matched
        .into_iter()
        .filter(|f| f.valid_from.is_some())
        .collect();
    dated.sort_by_key(|f| f.valid_from);
    let undated = facts.iter().filter(|f| f.valid_from.is_none()).count();
    let shown: Vec<&EntityFact> = dated.iter().copied().take(limit).collect();
    let mut lines: Vec<String> = Vec::new();
    if let Some(note) = &who.note {
        lines.push(note.clone());
    }
    if let Some(a) = &aligned {
        lines.push(a.clone());
    }
    let type_label = node.type_label.as_deref().unwrap_or("untyped");
    if shown.is_empty() {
        lines.push(format!(
            "{} ({type_label}): no dated facts{}; {undated} facts carry no date (entity_facts lists them).",
            node.name,
            if filter.narrows() { " in that window" } else { "" }
        ));
    } else {
        let first = shown[0].valid_from.expect("dated");
        let last = shown[shown.len() - 1].valid_from.expect("dated");
        let mut head = format!("{} ({type_label}): {} dated facts", node.name, dated.len());
        if shown.len() < dated.len() {
            head.push_str(&format!(
                ", {} shown from {} to {} (pass since/until to see the rest)",
                shown.len(),
                crate::time_text::world(first, shown[0].valid_from_precision.as_deref()),
                crate::time_text::world(
                    last,
                    shown[shown.len() - 1].valid_from_precision.as_deref()
                )
            ));
        }
        if undated > 0 {
            head.push_str(&format!(
                "; {undated} undated facts omitted (entity_facts has them)"
            ));
        }
        lines.push(head);
        let ids = with_closers(shown.iter().copied());
        let evidence = Evidence::load(ctx, ids, m.as_of).await;
        for f in &shown {
            let stamp = crate::time_text::world(
                f.valid_from.expect("dated"),
                f.valid_from_precision.as_deref(),
            );
            let closer = evidence.closer_mark(sink, f);
            lines.push(format!(
                "{stamp}  {}{}",
                fact_line(f, &closer),
                evidence.mark(sink, f.id)
            ));
        }
    }
    let detail = format!("{} of {} dated", shown.len(), dated.len());
    ToolResult::new(
        lines.join("\n"),
        Step::new("timeline", node.name, detail)
            .count(shown.len())
            .total(dated.len())
            .json(),
    )
}

// ---- paths_between ---------------------------------------------------------------

/// How an edge is written on the chain: walked forwards `A —pred→ B`, backwards `A ←pred— B`
pub(super) fn edge_text(prev: Uuid, e: &PathEdge, closer_mark: &str) -> String {
    let pred = e.predicate.as_deref().unwrap_or("?");
    let successor =
        super::tools::successor_text(e.closed_by.as_deref(), e.closed_by_value.as_ref());
    let range = crate::time_text::span(crate::time_text::Span {
        valid_from: e.valid_from,
        from_precision: e.valid_from_precision.as_deref(),
        valid_to: e.valid_to,
        to_precision: e.valid_to_precision.as_deref(),
        holds_from: e.holds_from,
        holds_to: e.holds_to,
        end_derived: e.end_derived,
        corrected: e.corrected_ends.as_deref(),
        closed_by: successor.as_deref().map(|who| (who, closer_mark)),
        correction_note: e.correction_note.as_deref(),
    });
    let range = if range.is_empty() {
        range
    } else {
        format!(" ({range})")
    };
    let conf = (e.confidence * 100.0).round() as i32;
    if e.subject_id == prev {
        format!(
            "{} —{pred}→ {}{range} [{conf}%]",
            e.subject_name, e.object_name
        )
    } else {
        format!(
            "{} ←{pred}— {}{range} [{conf}%]",
            e.object_name, e.subject_name
        )
    }
}

#[cfg(test)]
pub(super) fn path_text(p: &Path) -> String {
    path_text_marked(p, |_| String::new())
}

/// One path, with what `mark` gives each fact after each hop (in the conversation it is that fact's evidence `[n]`, #935);
/// the hop that the timeline closes also gets its number from `mark` for the line that closes it (#970 step 2)
fn path_text_marked(p: &Path, mut mark: impl FnMut(Uuid) -> String) -> String {
    let mut parts = Vec::new();
    for (i, e) in p.edges.iter().enumerate() {
        let closer = e.closed_by_id.map(&mut mark).unwrap_or_default();
        let own = mark(e.fact_id);
        parts.push(format!("{}{own}", edge_text(p.nodes[i], e, &closer)));
    }
    parts.join("; ")
}

/// The name of one end of this path: the edges at both ends of the path write it
fn name_on_path(p: &Path, id: Uuid) -> Option<&str> {
    p.edges.iter().find_map(|e| {
        if e.subject_id == id {
            Some(e.subject_name.as_str())
        } else if e.object_id == id {
            Some(e.object_name.as_str())
        } else {
            None
        }
    })
}

pub async fn paths_between(ctx: &ToolCtx<'_>, sink: &mut ToolSink, args: &Value) -> ToolResult {
    let mut notes = Vec::new();
    let mut ends = Vec::new();
    for key in ["from", "to"] {
        match resolve(ctx, sink, args[key].as_str().unwrap_or("")).await {
            Ok(r) => {
                if let Some(n) = &r.note {
                    notes.push(n.clone());
                }
                ends.push(r);
            }
            Err(ResolveError::ReadFailed) => {
                return ToolResult::new(
                    "Could not look up entities.".into(),
                    Step::new("path", "?", "failed").status("failed").json(),
                )
                .error();
            }
            Err(ResolveError::Unresolved(e)) => {
                return ToolResult::new(
                    format!("Unknown `{key}`: {e}."),
                    Step::new("path", "?", "unknown entity")
                        .status("not_found")
                        .json(),
                )
            }
        }
    }
    let m = moments(args);
    let max_hops = args["max_hops"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(3)
        .clamp(1, 3);
    let limits = Limits {
        max_hops,
        ..Limits::default()
    };
    let paths = match utopia_store::paths::paths_between(
        &ctx.state.pool,
        ctx.kb_id,
        ends[0].id,
        ends[1].id,
        m.at,
        m.as_of,
        limits,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "Path search failed");
            return ToolResult::new(
                "Could not search paths.".into(),
                Step::new("path", "?", "failed").status("failed").json(),
            )
            .error();
        }
    };
    // An end given as an id was not looked up in the library. If a path is found, it is on the route and its name is written on the edge; if not found,
    // it may simply be that this id is not in this library (copied wrong, or from another library): then "no path between the two" would read like a fact about the graph,
    // so first confirm it, and if it cannot be confirmed, say so plainly
    for (key, end) in ["from", "to"].into_iter().zip(ends.iter_mut()) {
        if !end.by_id {
            continue;
        }
        if let Some(p) = paths.first() {
            if let Some(name) = name_on_path(p, end.id) {
                end.name = name.to_string();
            }
            continue;
        }
        match utopia_store::graph::entity_node(&ctx.state.pool, ctx.kb_id, end.id, m.as_of).await {
            Ok(Some(node)) => end.name = node.name,
            Ok(None) => {
                return ToolResult::new(
                    format!(
                        "Unknown `{key}`: no entity with id {} in this base.",
                        end.id
                    ),
                    Step::new("path", "?", "unknown entity")
                        .status("not_found")
                        .json(),
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "Entity lookup failed");
                return ToolResult::new(
                    "Could not look up entities.".into(),
                    Step::new("path", "?", "failed").status("failed").json(),
                )
                .error();
            }
        }
    }
    let (from, to) = (&ends[0], &ends[1]);
    let label = format!("{} ↔ {}", from.name, to.name);
    let when = match (m.at, m.before, m.as_of) {
        (Some(t), _, _) => format!(" at {}", crate::time_text::world(t, Some("day"))),
        (None, Some(b), _) => format!(" as recorded before {}", crate::time_text::instant(b)),
        (None, None, Some(r)) => format!(" as recorded by {}", crate::time_text::instant(r)),
        (None, None, None) => String::new(),
    };
    let mut lines = notes;
    if paths.is_empty() {
        lines.push(format!(
            "No path of up to {max_hops} hop{} between {} and {}{when}. They may be unrelated in \
             this base, or connected only through a hub; try neighbors on each.",
            if max_hops == 1 { "" } else { "s" },
            from.name,
            to.name
        ));
        return ToolResult::new(
            lines.join("\n"),
            Step::new("path", label, "no path").count(0).json(),
        );
    }
    let shortest = paths[0].hops();
    lines.push(format!(
        "{} path{} between {} and {}{when} (up to {max_hops} hops, shortest first):",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" },
        from.name,
        to.name
    ));
    // Each hop is a fact with its own number; the hop that the timeline closes also needs its line's number
    let hops: Vec<Uuid> = paths
        .iter()
        .flat_map(|p| {
            p.edges
                .iter()
                .flat_map(|e| std::iter::once(e.fact_id).chain(e.closed_by_id))
        })
        .collect();
    let evidence = Evidence::load(ctx, hops, m.as_of).await;
    for (i, p) in paths.iter().enumerate() {
        let text = path_text_marked(p, |fact| evidence.mark(sink, fact));
        lines.push(format!("{}. {}", i + 1, text));
    }
    if paths.len() >= limits.max_paths {
        lines.push(format!(
            "(Only the first {} paths, shortest first, are listed; there may be more.)",
            limits.max_paths
        ));
    }
    let detail = format!(
        "{} path{}, shortest {shortest} hop{}",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" },
        if shortest == 1 { "" } else { "s" }
    );
    ToolResult::new(
        lines.join("\n"),
        Step::new("path", label, detail)
            .count(paths.len())
            .more(paths.len() >= limits.max_paths)
            .field("hops", shortest)
            .json(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, ty: Option<&str>, degree: i64) -> GraphNode {
        GraphNode {
            id: Uuid::now_v7(),
            name: name.into(),
            type_key: ty.map(|t| t.to_lowercase()),
            type_label: ty.map(String::from),
            color: "#000".into(),
            shape: "circle".into(),
            degree,
            disambiguator: None,
        }
    }

    /// Phrase entities (untyped) are not listed; exact hits come first, then those with more facts
    #[test]
    fn a_name_match_is_ranked_by_type_exactness_and_weight() {
        let hits = vec![
            node("lawsuit against OpenAI", None, 5),
            node("OpenAI's board of directors", Some("Organization"), 23),
            node("OpenAI", Some("ResearchProject"), 555),
            node("OpenAI LP", Some("Corporation"), 6),
        ];
        let ranked = rank(hits, "OpenAI");
        let names: Vec<&str> = ranked.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["OpenAI", "OpenAI's board of directors", "OpenAI LP"],
            "untyped 短语不列，精确命中第一"
        );
        assert!(
            dominant(&ranked, "OpenAI"),
            "精确命中且事实数是第二名的三倍"
        );
    }

    /// An untyped exact-hit entity is not pushed out by a typed entity with the same name
    #[test]
    fn an_exact_untyped_match_is_not_hidden_by_a_typed_namesake() {
        let ranked = rank(
            vec![
                node("Acme lawsuit", Some("Event"), 1),
                node("Acme", None, 200),
                node("Acme corporate history", None, 4),
            ],
            "Acme",
        );
        let names: Vec<&str> = ranked.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["Acme", "Acme lawsuit"],
            "叫这个名字的 untyped 照列且在前；别的 untyped 短语仍不列"
        );
        assert!(dominant(&ranked, "Acme"), "只有它精确命中");
    }

    #[test]
    fn a_base_with_no_types_still_lists_its_entities() {
        let ranked = rank(
            vec![node("Acme", None, 3), node("Acme Corp", None, 9)],
            "acme",
        );
        let names: Vec<&str> = ranked.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["Acme", "Acme Corp"],
            "全 untyped 时照列，精确命中在前"
        );
        assert!(dominant(&ranked, "acme"));
    }

    /// Nobody is clearly the one: for two same-named entities of the same magnitude, do not pick for the model
    #[test]
    fn two_namesakes_of_equal_weight_are_not_dominant() {
        let ranked = rank(
            vec![
                node("Zhang Wei", Some("Person"), 12),
                node("Zhang Wei", Some("Person"), 10),
            ],
            "Zhang Wei",
        );
        assert!(!dominant(&ranked, "Zhang Wei"));
        let ranked = rank(
            vec![
                node("Zhang Wei", Some("Person"), 40),
                node("Zhang Wei", Some("Person"), 10),
            ],
            "Zhang Wei",
        );
        assert!(dominant(&ranked, "Zhang Wei"), "事实数三倍以上就明显");
    }

    fn fact(direction: &str, pred: &str, other: &str, other_type: Option<&str>) -> EntityFact {
        EntityFact {
            said_as: None,
            recorded_at: chrono::Utc::now(),
            invalidated_at: None,
            supersedes: None,
            document_ids: vec![],
            id: Uuid::now_v7(),
            direction: direction.into(),
            predicate_key: Some(pred.into()),
            predicate_label: Some(pred.into()),
            inferred: false,
            temporal: Some("state".into()),
            other_id: Some(Uuid::now_v7()),
            other_name: Some(other.into()),
            other_type: other_type.map(String::from),
            qualifiers: Vec::new(),
            statement_qualifiers: Vec::new(),
            object_value: None,
            valid_from: Some("2021-01-01T00:00:00Z".parse().unwrap()),
            valid_to: None,
            valid_from_precision: Some("year".into()),
            valid_to_precision: None,
            holds_from: Some("2021-01-01T00:00:00Z".parse().unwrap()),
            holds_to: None,
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

    /// The question's vector orders only within the same tier: exact hits still come first, within a tier the nearer ones first, those with no profile at the bottom
    #[test]
    fn the_question_orders_namesakes_but_not_over_an_exact_match() {
        let exact_far = node("Sam Altman", Some("Organization"), 66);
        let exact_near = node("Sam Altman", Some("Person"), 27);
        let partial_nearest = node("Sam Altman's efforts", Some("Event"), 2);
        let no_profile = node("Sam Altman Jr", Some("Person"), 1);
        let mut dist = HashMap::new();
        dist.insert(exact_far.id, 0.40);
        dist.insert(exact_near.id, 0.20);
        dist.insert(partial_nearest.id, 0.05);
        let ranked = reorder_by_distance(
            vec![
                exact_far.clone(),
                no_profile.clone(),
                partial_nearest.clone(),
                exact_near.clone(),
            ],
            "Sam Altman",
            &dist,
        );
        let ids: Vec<Uuid> = ranked.iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            vec![
                exact_near.id,
                exact_far.id,
                partial_nearest.id,
                no_profile.id
            ]
        );
    }

    #[test]
    fn a_multi_word_name_is_scored_by_the_words_it_contains() {
        let words = ["OpenAI", "board", "members"];
        assert_eq!(word_score("OpenAI's board of directors", &words), 2);
        assert_eq!(word_score("OpenAI LP", &words), 1);
        assert_eq!(word_score("openai board members list", &words), 3);
    }

    /// When returning empty-handed, report the predicates on the entity, largest first
    #[test]
    fn an_empty_filter_names_the_predicates_that_exist() {
        let facts = vec![
            fact("out", "comprised", "Ilya Sutskever", Some("Person")),
            fact("out", "comprised", "Helen Toner", Some("Person")),
            fact("in", "removed", "Sam Altman", Some("Person")),
        ];
        assert_eq!(predicates_of(&facts), "comprised → (2), ← removed (1)");
    }

    /// Grouping: larger groups first, outgoing `pred →`, incoming `← pred`
    #[test]
    fn facts_group_by_predicate_largest_group_first() {
        let facts = vec![
            fact("out", "employee", "Bob", Some("Person")),
            fact("in", "founder", "Carol", Some("Person")),
            fact("out", "employee", "Dan", Some("Person")),
        ];
        let groups = grouped(&facts);
        let keys: Vec<&str> = groups.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["employee →", "← founder"]);
        assert_eq!(groups[0].1.len(), 2);
    }

    /// Filter: predicate by substring, object type by substring, time period by the stated interval
    #[test]
    fn a_filter_reads_predicate_type_and_window() {
        let mut f = fact("out", "works_for", "Acme", Some("Organization"));
        f.valid_from = Some("2019-01-01T00:00:00Z".parse().unwrap());
        f.valid_to = Some("2020-06-01T00:00:00Z".parse().unwrap());
        fn filter<'a>(
            p: Option<&'a str>,
            t: Option<&'a str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> FactFilter<'a> {
            FactFilter {
                predicate: p,
                object_type: t,
                since: since.map(|s| s.parse().unwrap()),
                until: until.map(|s| s.parse().unwrap()),
            }
        }
        assert!(filter(Some("works"), None, None, None).keeps(&f));
        assert!(!filter(Some("founder"), None, None, None).keeps(&f));
        assert!(filter(None, Some("organ"), None, None).keeps(&f));
        assert!(!filter(None, Some("person"), None, None).keeps(&f));
        assert!(
            filter(None, None, Some("2020-01-01T00:00:00Z"), None).keeps(&f),
            "2020 年初它还在"
        );
        assert!(
            !filter(None, None, Some("2021-01-01T00:00:00Z"), None).keeps(&f),
            "2021 年它已结束"
        );
        assert!(
            !filter(None, None, None, Some("2018-06-01T00:00:00Z")).keeps(&f),
            "2018 年它还没开始"
        );
        assert!(!filter(None, None, None, None).narrows());
    }

    /// Chain notation: forward uses →, backward uses ←, each edge carries its own interval
    #[test]
    fn a_chain_reads_in_walking_order() {
        let (a, x, b) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let edge = |s: Uuid, sn: &str, o: Uuid, on: &str, pred: &str| PathEdge {
            fact_id: Uuid::now_v7(),
            subject_id: s,
            subject_name: sn.into(),
            object_id: o,
            object_name: on.into(),
            predicate: Some(pred.into()),
            valid_from: Some("2021-01-01T00:00:00Z".parse().unwrap()),
            valid_from_precision: Some("year".into()),
            valid_to: None,
            valid_to_precision: None,
            holds_from: Some("2021-01-01T00:00:00Z".parse().unwrap()),
            holds_to: None,
            confidence: 0.9,
            end_derived: false,
            time_corrected: false,
            corrected_ends: None,
            closed_by_id: None,
            closed_by: None,
            closed_by_value: None,
            correction_note: None,
        };
        let p = Path {
            nodes: vec![a, x, b],
            edges: vec![
                edge(a, "OpenAI", x, "Dario Amodei", "employee"),
                edge(x, "Dario Amodei", b, "Anthropic", "founder"),
            ],
            specificity: 0.0,
        };
        assert_eq!(
            path_text(&p),
            "OpenAI —employee→ Dario Amodei (2021 → now) [90%]; Dario Amodei —founder→ Anthropic (2021 → now) [90%]"
        );
        let back = Path {
            nodes: vec![b, x, a],
            edges: vec![
                edge(x, "Dario Amodei", b, "Anthropic", "founder"),
                edge(a, "OpenAI", x, "Dario Amodei", "employee"),
            ],
            specificity: 0.0,
        };
        assert_eq!(
            path_text(&back),
            "Anthropic ←founder— Dario Amodei (2021 → now) [90%]; Dario Amodei ←employee— OpenAI (2021 → now) [90%]"
        );
    }
    #[test]
    fn qualifier_strings_are_text_not_json_encodings() {
        use utopia_core::models::FactQualifier;
        let mut f = fact("out", "works_for", "Acme", None);
        assert_eq!(qualifiers_text(&f), "");
        for (value, expected) in [
            (json!({"value":"等级 \"A\""}), "等级 \"A\""),
            (json!({"value":"ends\""}), "ends\""),
            (json!({"value":"C:\\reports\\a.txt"}), "C:\\reports\\a.txt"),
            (json!({"value":"line one\n第二行"}), "line one\n第二行"),
            (json!({"value":"中文“引号”"}), "中文“引号”"),
            (json!({"value":10}), "10"),
            (json!({"value":true}), "true"),
            (json!({"value":false}), "false"),
            (json!({"value":10,"unit":"kg"}), "10 kg"),
            (json!({"value":null}), "null"),
        ] {
            f.qualifiers = vec![FactQualifier {
                qualifier_type_id: Uuid::now_v7(),
                key: "detail".into(),
                label: "Detail".into(),
                value: Some(value),
                entity_id: None,
                entity_name: None,
            }];
            assert_eq!(qualifiers_text(&f), format!(" [detail: {expected}]"));
        }
        f.qualifiers[0].value = None;
        f.qualifiers[0].entity_id = f.other_id;
        f.qualifiers[0].entity_name = Some("Acme".into());
        assert_eq!(qualifiers_text(&f), " [detail: Acme]");
        f.qualifiers[0].entity_name = None;
        assert_eq!(qualifiers_text(&f), " [detail: ?]");
    }
}
