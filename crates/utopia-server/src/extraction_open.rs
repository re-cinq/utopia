//! The write path of the open graph (0044 cut 1, #729): what the document says is recorded in its own words.
//!
//! The only difference from `extraction::run` is one thing: **the prompt has no ontology**, so the model does not pick relations, pick classes,
//! or compute dates. The reply holds what it mentions (`e`, both named things and merely described ones), the statements it makes (`s`: the relation
//! phrase copied verbatim, subject and object written by name, time words copied verbatim, the quote copied as a whole sentence), and aliases (`n`). On write, a statement becomes
//! a fact row with `layer = 'open'`, with the phrase kept on the row; qualifiers attach to `statement_qualifiers`
//! by the document's own role words; time words go into `time_mentions` unchanged, and nobody counts them as dates, which is
//! 0045's job. Typed facts are computed from these rows by alignment (cut 2), and are not written here.
//!
//! **Statements point at things by name, not by number.** The first version gave `e` numbers, wrote statements with numbers, and indexed quotes by sentence,
//! to save output tokens; measured, deepseek-v4-flash mixed up the numbers in dense paragraphs: in the same chunk, two replies
//! said "NVIDIA has reached AI" once and "tokens are tokens" once, where the source says "AI has reached its
//! inflection point". The prototype writes subject and object by name and copies the quote for each statement, and fabricated none of 333. Faithfulness before saving money.
//!
//! What is reused is the identity part: names still go through `resolve_handle` (two same-named things in one reply
//! do not collapse into one, and 0041's name facts are still recorded), and described things are built as entities without name facts
//! (`create_described`), so that "a hospital" does not become a bridge for recall.
//!
//! Two timelines (0022 / #714 / 0064): the attestation when a statement is stored only counts the date the source system gives the document; with none, it is
//! left empty. The date the document itself states (the `t` each chunk reports) is written onto the statements after extraction
//! by the time-resolution step, section by section (`time_resolution::attest_statements`).

use crate::extraction::{
    chat_retrying_rate_limits_at, drop_signal, incomplete_reason, origin_ceiling, resolve_handle,
    span_in_quote,
};
use crate::state::AppState;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use utopia_core::models::{Document, KnowledgeBase, LlmSettings, Proposer};
use utopia_extract::KnownEntity;
use utopia_store::documents::ChunkForExtract;
use utopia_store::extraction_drops::reason;
use utopia_store::graph::FactObject;
use utopia_store::graph::Validity;
use utopia_store::pending::{Outcome, Proposal};
pub(crate) use utopia_store::resolution::name_key;
use uuid::Uuid;

/// Intermediate product after one chunk's call and parsing, which the apply half reads to write to the store (#588 cut 1).
/// `name_vecs` (0041 decision 3, channel 2) is computed right after parsing; apply's `resolve_handle`
/// needs it immediately, and computing it in two places would only pass more arguments between the helpers.
struct ChunkParsed {
    extraction: utopia_extract::open::OpenExtraction,
    name_vecs: HashMap<String, Vec<f32>>,
}

/// Identity clues belong to the statement's subject, not to every name in the chunk.
/// Keep only grounded, single-sentence quotes with one subject owner. Overlapping
/// quotes assigned to different subjects cannot tell us whose department was named.
fn mention_evidence(
    extraction: &utopia_extract::open::OpenExtraction,
    text: &str,
) -> HashMap<String, String> {
    let quotes: Vec<_> = extraction
        .statements
        .iter()
        .filter_map(|s| {
            let quote = s.quote.as_deref()?.trim();
            Some((name_key(&s.subject), quote, locate(text, quote)?))
        })
        .collect();
    let mut evidence = HashMap::<String, String>::new();
    for (subject, quote, (start, end)) in &quotes {
        // Conservative about abbreviations too: a missed clue falls back to the
        // existing resolver; joining separate sentences can attach to the wrong person.
        let body = quote.trim_end_matches(['.', '!', '?', '。', '！', '？', '"', '”', '’']);
        if body.contains(['.', '!', '?', '。', '！', '？', ';', '；'])
            || !span_in_quote(subject, quote)
            || quotes.iter().any(|(other, _, (a, b))| {
                other != subject && ((a < end && start < b) || span_in_quote(other, quote))
            })
        {
            continue;
        }
        let entry = evidence.entry(subject.clone()).or_default();
        entry.push_str(quote);
        entry.push('\n');
    }
    evidence
}

/// Why the reply is truncated: the endpoint hit the token limit (#760), or the guard saw it repeating and stopped reading (#1301).
/// Both are handled as truncation, and the dropped row must say which one it was: a repetition should check the limit, which would be looking in the wrong place
#[derive(Debug, Clone, Copy)]
enum Cut {
    Ceiling,
    Repeating,
}

/// The first half-step for a chunk: call the model, parse the reply, compute name vectors (#588 cut 1). This half does not write the graph,
/// so chunks may be done out of order: concurrency (cut 2) acts on exactly this; the second half `apply_open_extraction`
/// must run in chunk order. A `pushed` (0054) chunk is itself the contract: it does not call the model, it just parses.
///
/// `Ok(None)` means this chunk was skipped: the call failed or the reply could not be parsed; the reason is already recorded in `drop_signal` and
/// `unextracted`, and the main loop moves on to the next chunk.
///
/// `Err` has two causes: no chat model, or an empty balance (#1095). An empty balance
/// stops the document, and the error keeps its chain for `alerting::hopeless`.
///
/// The takeover check (epoch) stays in the main loop, not here: it is checked both before calling the model and before the reply is written.
#[allow(clippy::too_many_arguments)]
async fn call_and_parse_one_chunk(
    state: &AppState,
    settings: Option<&LlmSettings>,
    client: Option<&utopia_llm::LlmClient>,
    embed_client: Option<&utopia_llm::LlmClient>,
    kb_id: Uuid,
    document_id: Uuid,
    filename: &str,
    chunk: &ChunkForExtract,
    opening: Option<&str>,
    known: &[KnownEntity],
    pushed: bool,
    unextracted: &mut Vec<(i32, String)>,
) -> anyhow::Result<Option<ChunkParsed>> {
    // Pushed statements: the chunk is the contract, so parse it rather than asking the model (0054). From the parse onward, the steps below are unchanged
    let (reply_text, cut) = if pushed {
        (chunk.text.clone(), None)
    } else {
        let (settings, client) = match (settings, client) {
            (Some(s), Some(c)) => (s, c),
            _ => anyhow::bail!("Chat model not configured; cannot extract"),
        };
        let messages =
            utopia_extract::open::build_open_messages(filename, known, opening, &chunk.text);
        // Temperature 0: verbatim copying should not depend on sampling. When the endpoint defaults to 1.0, two replies for the same chunk differ in density by three times
        let reply = match chat_retrying_rate_limits_at(
            state,
            settings,
            client,
            &messages,
            Some(0.0),
        )
        .await
        {
            Ok(r) => r,
            Err(e) if utopia_llm::out_of_credit(&e).is_some() => return Err(e),
            Err(e) => {
                tracing::warn!(%document_id, seq = chunk.seq, error = %e, "开放抽取调用失败，跳过该分块");
                drop_signal(
                    state,
                    kb_id,
                    document_id,
                    reason::CHUNK_UNEXTRACTED,
                    "request failed; this chunk did not enter the graph",
                    Some(&format!("#{}: {e}", chunk.seq)),
                )
                .await;
                unextracted.push((chunk.seq, format!("request failed: {e}")));
                return Ok(None);
            }
        };
        tracing::debug!(%document_id, seq = chunk.seq, reply = %reply.text, "开放抽取的原始回复");
        // The parser sees only that the JSON lost its tail, not why, so the reason has to be carried over from the reply (#760)
        let cut = if reply.repeating {
            Some(Cut::Repeating)
        } else if reply.hit_token_ceiling() {
            Some(Cut::Ceiling)
        } else {
            None
        };
        (reply.text, cut)
    };
    let mut extraction = match utopia_extract::open::parse_open_response(&reply_text) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(%document_id, seq = chunk.seq, error = %e, hit_token_ceiling = matches!(cut, Some(Cut::Ceiling)), repeating = matches!(cut, Some(Cut::Repeating)), "开放抽取回复解析失败，跳过该分块");
            // A whole chunk did not make it into the graph, and the reasons differ: hitting the limit means "the answer was too long", which can be changed; repetition is the model stuck on one line;
            // other parse failures mean "the reply is malformed". Writing these as one sentence would make them indistinguishable
            let (why, detail) = match cut {
                Some(Cut::Ceiling) => (
                    "response hit the token ceiling and could not be parsed; this chunk did not enter the graph",
                    format!("#{}: hit the token ceiling; {e}", chunk.seq),
                ),
                Some(Cut::Repeating) => (
                    "response started repeating itself and could not be parsed; this chunk did not enter the graph",
                    format!("#{}: started repeating itself; {e}", chunk.seq),
                ),
                None => (
                    "response could not be parsed; this chunk did not enter the graph",
                    format!("#{}: {e}", chunk.seq),
                ),
            };
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::CHUNK_UNEXTRACTED,
                why,
                Some(&detail),
            )
            .await;
            unextracted.push((chunk.seq, format!("response parsing failed: {e}")));
            return Ok(None);
        }
    };
    // Where the model wrote a value into the object slot, move it to the value and record one entry for each; pushed statements are a contract and are not touched (0054)
    if !pushed {
        for name in utopia_extract::open::values_are_not_things(&mut extraction) {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::OBJECT_IS_VALUE,
                "kept, with the object read as a value",
                Some(&name),
            )
            .await;
        }
    }
    if extraction.truncated {
        drop_signal(
            state,
            kb_id,
            document_id,
            reason::TRUNCATED_REPLY,
            match cut {
                Some(Cut::Ceiling) => {
                    "the open reply hit the token ceiling; kept up to the last complete item"
                }
                Some(Cut::Repeating) => {
                    "the reply started repeating itself; kept up to the last complete item"
                }
                // The reply ended normally but is invalid as a whole: the model wrote the brackets wrongly. Those read out one by one are kept; the count of
                // broken ones is in the signal below
                None => "the open reply was not valid JSON as a whole; read item by item",
            },
            None,
        )
        .await;
    }
    if extraction.skipped > 0 {
        drop_signal(
            state,
            kb_id,
            document_id,
            reason::MALFORMED_ITEM,
            &format!(
                "{} items in the open reply were malformed",
                extraction.skipped
            ),
            None,
        )
        .await;
    }
    // Name vectors (0041 decision 3, channel 2): for named things in this chunk, each name string gets one vector; during resolution it is used to find near neighbours
    // among the store's name vectors. One batch per chunk; if it cannot be computed (the endpoint blipped), extraction is not blocked,
    // this chunk just loses one recall channel
    let name_vecs: HashMap<String, Vec<f32>> = match (settings, embed_client) {
        (Some(settings), Some(client)) => {
            let mut wanted: Vec<(String, String)> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for e in &extraction.entities {
                let n = e.name.trim();
                if e.named && !n.is_empty() && seen.insert(name_key(n)) {
                    wanted.push((name_key(n), n.to_string()));
                }
            }
            embed_names(state, settings, client, &wanted).await
        }
        _ => HashMap::new(),
    };
    Ok(Some(ChunkParsed {
        extraction,
        name_vecs,
    }))
}

/// The canonical keys of the two arrays `e` and `s` in a pushed chunk, in original order (the memory key of 9012)
struct PushedRaw {
    entities: Vec<String>,
    statements: Vec<String>,
}

impl PushedRaw {
    fn parse(chunk_text: &str) -> Self {
        let payload: serde_json::Value = serde_json::from_str(chunk_text).unwrap_or_default();
        let keys = |field: &str| -> Vec<String> {
            payload[field]
                .as_array()
                .map(|items| items.iter().map(|item| item.to_string()).collect())
                .unwrap_or_default()
        };
        Self {
            entities: keys("e"),
            statements: keys("s"),
        }
    }
}

/// Things carried over from the previous version on re-push: unchanged statements (position within the chunk → fact) and unchanged named entities
/// (canonical key → entity). Carried-over items reuse the old result directly, and do not go through resolution or write facts again
#[derive(Default)]
struct Carried {
    statements: HashMap<usize, Uuid>,
    entities: HashMap<String, Uuid>,
}

/// The second half-step for a chunk: write the parsed reply as statements, names and evidence (#588 cut 1).
///
/// **Must run in chunk order**: the entities earlier chunks recognized (`doc_entities`, `known_by_name`), the things described
/// (`described`), and the names awaiting adjudication are all read by later chunks. After the call half runs concurrently, this part stays serial.
///
/// `pushed` (0054): the chunk is itself the contract; time words are located within the whole chunk, not searched for in the quote (0054 decision 4)
#[allow(clippy::too_many_arguments)]
async fn apply_open_extraction(
    state: &AppState,
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
    chunk: &ChunkForExtract,
    ctx: Option<&[f32]>,
    extraction: &utopia_extract::open::OpenExtraction,
    name_vecs: HashMap<String, Vec<f32>>,
    attested_at: Option<chrono::DateTime<chrono::Utc>>,
    bound_types: &HashMap<String, Uuid>,
    pushed: bool,
    identity: bool,
    await_nod: bool,
    carried: &Carried,
    proposer: Proposer,
    pending_count: &mut usize,
    statement_count: &mut usize,
    needs_adjudication: &mut bool,
    human_reviews_found: &mut bool,
    doc_entities: &mut Vec<(Uuid, String, String)>,
    known_by_name: &mut HashMap<String, Uuid>,
    described: &mut HashMap<String, Uuid>,
    handled_by_name: &mut HashMap<String, Vec<Uuid>>,
    ambiguous_bare_cache: &mut HashMap<String, Uuid>,
    touched_names: &mut HashSet<String>,
) -> anyhow::Result<()> {
    // ---- Things: named ones go through identity resolution; described ones are built as entities without name facts ----
    // Object-only mentions and rejected quotes deliberately supply None, including
    // to the old #331 tie/grey-zone paths: another subject's chunk is not their clue.
    // With no owned clue, profile tiers and their ordinary review requests decide.
    let evidence = mention_evidence(extraction, &chunk.text);
    let mut response_claims: HashMap<String, Vec<Uuid>> = HashMap::new();
    let mut local: HashMap<String, Uuid> = HashMap::new();
    // Described things: first record their name and category word, and build them only once a statement points to them (see `place`)
    let mut deferred: HashMap<String, (String, String)> = HashMap::new();
    let pushed_raw = pushed.then(|| PushedRaw::parse(&chunk.text));
    for e in &extraction.entities {
        let name = e.name.trim();
        if name.is_empty() {
            continue;
        }
        let kind = e.kind.trim();
        let key = name_key(name);
        if local.contains_key(&key) {
            continue;
        }
        let carried = pushed_raw
            .as_ref()
            .and_then(|raw| raw.entities.get(e.raw_index))
            .and_then(|key| carried.entities.get(key).copied());
        let id = if e.named {
            let id = match carried {
                Some(id) => id,
                None => {
                    let bound = bound_types
                        .get(&utopia_store::type_bindings::normalize(kind))
                        .copied();
                    let id = resolve_handle(
                        pool,
                        kb_id,
                        bound,
                        name,
                        ctx,
                        name_vecs.get(&key).map(Vec::as_slice),
                        evidence.get(&key).map(String::as_str),
                        identity,
                        &mut response_claims,
                        handled_by_name,
                        ambiguous_bare_cache,
                        needs_adjudication,
                        human_reviews_found,
                    )
                    .await?;
                    // What the model says it is (the film in "a British film"): stored as its own wording;
                    // the class binding is done later by alignment
                    if !kind.is_empty() {
                        let _ = utopia_store::resolution::set_specific_type(pool, id, kind).await;
                    }
                    touched_names.insert(name_key(name));
                    id
                }
            };
            // When the name is literally in this chunk's source text, back-fill the name fact's provenance (0041). Memory logs do not get this:
            // whether that sentence counts as provenance waits for a person's nod (0018)
            if !await_nod && span_in_quote(name, &chunk.text) {
                let _ = utopia_store::names::record(
                    pool,
                    kb_id,
                    id,
                    name,
                    Some(utopia_store::names::NameSource {
                        chunk_id: chunk.id,
                        quote: name,
                    }),
                    attested_at,
                )
                .await;
            }
            if let Some(key) = pushed_raw
                .as_ref()
                .and_then(|raw| raw.entities.get(e.raw_index))
            {
                utopia_store::pushed_memo::remember_entity(pool, chunk.id, key, id).await?;
            }
            if !doc_entities.iter().any(|(x, _, _)| *x == id) {
                doc_entities.push((id, kind.to_string(), name.to_string()));
                known_by_name.entry(key.clone()).or_insert(id);
            }
            id
        } else {
            deferred.insert(key, (name.to_string(), kind.to_string()));
            continue;
        };
        local.insert(key, id);
    }

    // ---- Statements ----
    // Pushed statements record the facts they wrote by original position: on re-push, an unchanged statement can reuse the old fact directly
    for s in &extraction.statements {
        if let Some(&fact_id) = carried.statements.get(&s.raw_index) {
            let time_words =
                locate_time_words(state, kb_id, document_id, chunk, None, pushed, s).await;
            for (words, start, role) in &time_words {
                utopia_store::time_mentions::record(
                    pool, kb_id, fact_id, chunk.id, words, *start, role,
                )
                .await?;
            }
            continue;
        }
        let phrase = s.phrase.trim();
        if phrase.is_empty() {
            continue;
        }
        let Some(subject) = place(
            pool,
            kb_id,
            &mut local,
            known_by_name,
            &deferred,
            described,
            &s.subject,
        )
        .await?
        else {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::UNKNOWN_REF,
                "a statement's subject is not a listed thing",
                Some(&s.subject),
            )
            .await;
            continue;
        };
        // The object is written by name but was not on the list: the model left it out. The statement is written as is, with the object stored as a literal:
        // no entity is invented, and the statement is not lost (the tier of #559)
        // A value with no letters or digits at all (a "—" in a table, or spaces) says nothing: treated as having no value.
        // This is a structural judgement, not a word list: it checks whether there is content, not what the content is
        let mut value = s
            .value
            .as_deref()
            .map(str::trim)
            .filter(|v| v.chars().any(char::is_alphanumeric));
        // The value the model itself wrote is recorded before "object undeclared" replaces it below: the quote is checked against it,
        // not against an entity's name (#729)
        let stated_value = value;
        let object = match s.object.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
            Some(name) => match place(
                pool,
                kb_id,
                &mut local,
                known_by_name,
                &deferred,
                described,
                name,
            )
            .await?
            {
                Some(id) => Some(id),
                None => {
                    drop_signal(
                        state,
                        kb_id,
                        document_id,
                        reason::OBJECT_UNDECLARED,
                        "a statement's object is not a listed thing; written as a value",
                        Some(name),
                    )
                    .await;
                    value = value.or(Some(name));
                    None
                }
            },
            None => None,
        };
        // The phrase is the value, and the phrase is the subject: the two ways the model writes it when a table lost its column header (or has no caption sentence)
        // (each of the two categories appears in the long financial reports of #743). The statement is written, since the value and the subject are both there and no information is lost; just log one entry
        // so it can be measured; compare literally only, without recognising words
        let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
        if value.is_some_and(|v| same(v, phrase)) {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::PHRASE_IS_VALUE,
                "kept, but the phrase is the value itself",
                Some(phrase),
            )
            .await;
        } else if same(&s.subject, phrase) {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::PHRASE_IS_SUBJECT,
                "kept, but the phrase is the subject's own name",
                Some(phrase),
            )
            .await;
        }
        let value_json;
        let fact_object = match (object, value) {
            (Some(o), _) if o == subject => {
                drop_signal(
                    state,
                    kb_id,
                    document_id,
                    reason::OBJECT_MISSING,
                    "a statement points at its own subject",
                    Some(phrase),
                )
                .await;
                continue;
            }
            (Some(o), _) => FactObject::Entity(o),
            (None, Some(v)) => {
                value_json = serde_json::json!({ "value": v });
                FactObject::Value(&value_json)
            }
            (None, None) => {
                drop_signal(
                    state,
                    kb_id,
                    document_id,
                    reason::OBJECT_MISSING,
                    "a statement has neither an object nor a value",
                    Some(phrase),
                )
                .await;
                continue;
            }
        };
        // Quote locating. A sentence that cannot be located is still recorded as evidence text, only without an offset, and one entry is logged:
        // sentences the model did not copy verbatim are the early signs of the "not stated in the source" kind, so measure it
        let quote_text = s.quote.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let quote: Option<(&str, Option<(i32, i32)>)> =
            quote_text.map(|q| (q, locate(&chunk.text, q)));
        if let Some((q, None)) = quote {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::QUOTE_NOT_IN_CHUNK,
                "a quoted sentence is not in the chunk verbatim",
                Some(q),
            )
            .await;
        }
        // **The value must appear in its own quote** (#729). When a table row of five columns is flattened into five statements that differ only in value, but the quote
        // is the lead sentence above the table, at most one of the five is right, and nobody can tell which. This check
        // covers only numeric values: the kind where an entity name is written as the value (object_undeclared) is excluded
        if let (Some(v), Some((q, _))) = (stated_value, quote) {
            if v.chars().any(|c| c.is_ascii_digit()) && !shows_value(q, v) {
                drop_signal(
                    state,
                    kb_id,
                    document_id,
                    reason::VALUE_NOT_IN_QUOTE,
                    "a statement's value is not in its own quoted sentence",
                    Some(v),
                )
                .await;
                continue;
            }
        }
        // An open statement has no self-reported confidence from the model: it says "the document says so". Rows derived from a chart description still
        // get the cap (0040 decision 4)
        let confidence = origin_ceiling(&chunk.origin, 1.0);
        // Qualifiers and time words are computed first: the direct-write path and the wait-for-a-nod path use the same data
        // Qualifiers: if the value is the name of something on the list, attach the entity; otherwise attach the text
        let mut qualifiers: Vec<(&str, Option<serde_json::Value>, Option<Uuid>)> = Vec::new();
        for (role, text) in &s.qualifiers {
            let (role, text) = (role.trim(), text.trim());
            if role.is_empty() || text.is_empty() {
                continue;
            }
            match place(
                pool,
                kb_id,
                &mut local,
                known_by_name,
                &deferred,
                described,
                text,
            )
            .await?
            {
                Some(id) => qualifiers.push((role, None, Some(id))),
                None => qualifiers.push((role, Some(serde_json::json!(text)), None)),
            }
        }
        let time_words =
            locate_time_words(state, kb_id, document_id, chunk, quote, pushed, s).await;
        if await_nod {
            // Memory log: everything goes into the pending-confirmation table unchanged (0015); only when a person nods does `pending::confirm` turn it
            // into an open statement: same phrase, qualifiers, time words, and quote offsets
            let (object_id, object_value) = match fact_object {
                FactObject::Entity(id) => (Some(id), None),
                FactObject::Value(v) => (None, Some(v)),
            };
            let qualifiers_json = serde_json::Value::Array(
                qualifiers
                    .iter()
                    .map(|(role, value, entity)| match entity {
                        Some(id) => serde_json::json!({ "role": role, "entity_id": id }),
                        None => serde_json::json!({ "role": role, "value": value }),
                    })
                    .collect(),
            );
            let time_json = serde_json::Value::Array(
                time_words
                    .iter()
                    .map(|(text, start, role)| {
                        serde_json::json!({ "text": text, "char_start": start, "role": role })
                    })
                    .collect(),
            );
            let outcome = utopia_store::pending::propose(
                pool,
                Proposal {
                    kb_id,
                    subject_id: subject,
                    predicate_id: None,
                    object_id,
                    object_value,
                    proposed_predicate: Some(phrase),
                    validity: Validity {
                        attested_at,
                        ..Default::default()
                    },
                    confidence,
                    chunk_id: chunk.id,
                    proposed_by: proposer.user_id,
                    proposed_token: proposer.token_id,
                    phrase: Some(phrase),
                    qualifiers: Some(&qualifiers_json),
                    time_words: Some(&time_json),
                    quote_span: quote.and_then(|(_, span)| span),
                },
            )
            .await?;
            if let Outcome::Proposed(_) = outcome {
                *pending_count += 1;
            }
            *statement_count += 1;
            continue;
        }
        let (fact_id, _created) = utopia_store::graph::insert_open_statement(
            pool,
            kb_id,
            subject,
            phrase,
            fact_object,
            attested_at,
            confidence,
        )
        .await?;
        if let Some(key) = pushed_raw
            .as_ref()
            .and_then(|raw| raw.statements.get(s.raw_index))
        {
            utopia_store::pushed_memo::remember(pool, chunk.id, s.raw_index as i32, key, fact_id)
                .await?;
        }
        // The surface predicate is also written as the phrase: today's read paths all take the name from `fact_surface_predicate`
        utopia_store::graph::add_evidence_located(
            pool,
            fact_id,
            chunk.id,
            quote.map(|(q, _)| q),
            Some(phrase),
            quote.and_then(|(_, span)| span),
        )
        .await?;
        for (role, value, entity) in &qualifiers {
            utopia_store::graph::add_statement_qualifier(
                pool,
                fact_id,
                chunk.id,
                role,
                value.as_ref(),
                *entity,
            )
            .await?;
        }
        for (words, start, role) in &time_words {
            utopia_store::time_mentions::record(
                pool, kb_id, fact_id, chunk.id, words, *start, role,
            )
            .await?;
        }
        *statement_count += 1;
    }

    // ---- Aliases (0041 decision 2): the server checks only that the name really appears in this chunk's source text ----
    // Aliases in memory logs are not recorded: whether that sentence counts as provenance waits for a person's nod (0018)
    for n in extraction.names.iter().filter(|_| !await_nod) {
        let name = n.name.trim();
        if name.is_empty() {
            continue;
        }
        let Some(id) = place(
            pool,
            kb_id,
            &mut local,
            known_by_name,
            &deferred,
            described,
            &n.entity,
        )
        .await?
        else {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::UNKNOWN_REF,
                "a name's thing is not a listed thing",
                Some(&n.entity),
            )
            .await;
            continue;
        };
        if !span_in_quote(name, &chunk.text) {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::NAME_NOT_IN_TEXT,
                "an extra name is not in the chunk",
                Some(name),
            )
            .await;
            continue;
        }
        // A name cannot be the name of two different things at once: if this chunk, or earlier in this document, already recognized it as
        // another entity, it is not recorded
        let key = name_key(name);
        let claimed = handled_by_name
            .get(&key)
            .is_some_and(|ids| ids.iter().any(|x| *x != id));
        if claimed {
            drop_signal(
                state,
                kb_id,
                document_id,
                reason::NAME_CLAIMED_BY_ANOTHER,
                "an extra name is already another thing's name",
                Some(name),
            )
            .await;
            continue;
        }
        let quote = n
            .quote
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .unwrap_or(&chunk.text);
        let _ = utopia_store::names::record(
            pool,
            kb_id,
            id,
            name,
            Some(utopia_store::names::NameSource {
                chunk_id: chunk.id,
                quote,
            }),
            attested_at,
        )
        .await;
        touched_names.insert(key);
    }

    // Once this chunk is extracted, mark it: a chunk claimed during an update carries its mark and is skipped; an interrupted extraction can resume
    utopia_store::documents::mark_chunk_extracted(pool, chunk.id).await?;

    Ok(())
}

/// A document date counts as evidence date only when it comes from the content or from the source system (same basis as `temporal::DATED_AT`).
pub(crate) fn dated_at(doc: &Document) -> Option<chrono::DateTime<chrono::Utc>> {
    if matches!(doc.doc_time_source.as_str(), "content" | "source") {
        doc.doc_time
    } else {
        None
    }
}

/// The time words of a statement: start and end are each one mention, and each must appear verbatim in this statement's quote
async fn locate_time_words<'a>(
    state: &AppState,
    kb_id: Uuid,
    document_id: Uuid,
    chunk: &ChunkForExtract,
    quote: Option<(&str, Option<(i32, i32)>)>,
    pushed: bool,
    s: &'a utopia_extract::open::OpenStatement,
) -> Vec<(&'a str, i32, &'static str)> {
    let mut time_words = Vec::new();
    for (role, words) in [("when", s.when.as_deref()), ("ended", s.ended.as_deref())]
        .into_iter()
        .filter_map(|(role, words)| Some((role, words?.trim())))
        .filter(|(_, w)| !w.is_empty())
    {
        // A pushed statement has no quote: the entry is itself the evidence (0054 decision 4), and this chunk is the one payload,
        // with time words looked up within the chunk. Going through `locate_time` exits at `quote?`, which would drop both start and end
        let located = if pushed {
            locate(&chunk.text, words).map(|(start, _)| start)
        } else {
            locate_time(&chunk.text, quote, words)
        };
        match located {
            Some(start) => time_words.push((words, start, role)),
            None => {
                drop_signal(
                    state,
                    kb_id,
                    document_id,
                    reason::TIME_NOT_IN_QUOTE,
                    "a time mention's words are not in the statement's own sentence",
                    Some(words),
                )
                .await;
            }
        }
    }
    time_words
}

/// The **character** offsets (start, end) of a span of text within the chunk. Characters, not bytes: the UI and SQL's `substr` both count characters,
/// and a Chinese character is three bytes, so byte offsets would be misplaced on screen. Not found gives `None`:
/// offsets can only be computed by the server from the source text; the number the model reports counts for nothing.
///
/// **A run of whitespace matches a run of whitespace**; everything else is compared character by character. PDF text layers break lines at line ends, so "December⏎31, 2024"
/// is copied back by the model as "December 31, 2024"; compared character by character, this quote and its date would not match, so the statement is stored
/// while the time is lost (all four dates in an English board minutes document went missing). Whether a line break or a space is used is not what the source says,
/// so the offset is computed from the position in the source text
fn locate(hay: &str, needle: &str) -> Option<(i32, i32)> {
    let (needle, _) = squashed(needle.trim());
    if needle.is_empty() {
        return None;
    }
    let (hay, at) = squashed(hay);
    let found = hay
        .windows(needle.len())
        .position(|w| w == needle.as_slice())?;
    let start = at[found];
    let end = at[found + needle.len() - 1] + 1;
    Some((start as i32, end as i32))
}

/// Consecutive whitespace is folded into one space. The second item is the character position in the source of each character after folding
fn squashed(text: &str) -> (Vec<char>, Vec<usize>) {
    let (mut out, mut at) = (Vec::new(), Vec::new());
    let mut in_space = false;
    for (i, c) in text.chars().enumerate() {
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
                at.push(i);
            }
            in_space = true;
        } else {
            out.push(c);
            at.push(i);
            in_space = false;
        }
    }
    (out, at)
}

/// The character start of a time word within the chunk. **It must be inside this statement's own quote sentence**: the model attaches a time word to
/// several statements (in the FDA corpus, measured: "week 4" was attached to "should not be taken by patients with allergies", and three of six time-bearing statements were wrong),
/// and being found anywhere in the chunk does not mean this sentence says it. If the quote is in the chunk but the quote itself was not located in the chunk, the start falls back to the first occurrence in the whole chunk
/// whether this statement's value is really written in its quote (#729)
///
/// Thousands separators, whitespace and currency symbols on either side do not count; everything else is compared as is. **Deliberately lenient**: this is a discard
/// rule, and letting through a dubious one is better than dropping a correct one
fn shows_value(quote: &str, value: &str) -> bool {
    let strip = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace() && *c != ',' && !matches!(c, '$' | '￥' | '€' | '£'))
            .collect::<String>()
    };
    let v = strip(value);
    v.is_empty() || strip(quote).contains(&v)
}

fn locate_time(chunk: &str, quote: Option<(&str, Option<(i32, i32)>)>, words: &str) -> Option<i32> {
    let (q, span) = quote?;
    if let Some((inner, _)) = locate(q, words) {
        return match span {
            // Looked for within this one sentence of the source: the quote and the source may not have the same whitespace (a line break plus indentation against one space),
            // so adding the position in the quote would drift
            Some((start, end)) => {
                let sentence: String = chunk
                    .chars()
                    .skip(start.max(0) as usize)
                    .take((end - start).max(0) as usize)
                    .collect();
                Some(start + locate(&sentence, words).map_or(inner, |(i, _)| i))
            }
            None => locate(chunk, words).map(|(s, _)| s),
        };
    }
    // A row of a table: the period number is written in the header row, not in this row; this chunk is that table (the chunker has the header
    // follow every chunk), so look within the whole chunk. The criterion is structural: the quote lands on a table row in the chunk that starts with `|`
    // (the model often drops the leading `| ` from the copy, so look at that row in the chunk, not only at the quote itself)
    let on_table_row = match span {
        Some((start, _)) => {
            let start = chunk
                .char_indices()
                .nth(start.max(0) as usize)
                .map_or(chunk.len(), |(b, _)| b);
            let line_start = chunk[..start].rfind('\n').map_or(0, |i| i + 1);
            chunk[line_start..].trim_start().starts_with('|')
        }
        None => q.trim_start().starts_with('|') || q.contains(" | "),
    };
    if on_table_row {
        return locate(chunk, words).map(|(s, _)| s);
    }
    // Dates on headings (0064): a section's heading states only one date ("## 2025 Q3"), and a statement under it that gives no date uses it as when.
    // The criterion is also structural: these words sit on a heading row in the chunk that starts with `#`
    let (start, _) = locate(chunk, words)?;
    let byte = chunk
        .char_indices()
        .nth(start.max(0) as usize)
        .map_or(chunk.len(), |(b, _)| b);
    let line_start = chunk[..byte].rfind('\n').map_or(0, |i| i + 1);
    let line = chunk[line_start..].lines().next().unwrap_or("");
    let level = line.trim_start().chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) {
        return Some(start);
    }
    None
}

/// Lookup key for a name: whitespace collapsed, lowercased. The name written in a statement and the name listed in `e` must match exactly,
/// the only allowed difference being whitespace and case
/// The number of names sent for embedding per request. The embedding endpoint limits batches per request, on the same scale as the chunk batches in `pipeline`
const NAME_EMBED_BATCH: usize = 16;
/// How many names still lacking a vector to fill in after extracting one document. One batch at a time; the rest is filled by the next document
const NAME_VECTOR_PENDING: i64 = 256;

/// One vector per name in a batch, keyed by `name_key`. If the count does not match, the whole batch is abandoned (pairing is by position, and one wrong entry
/// misaligns everything, the same rule as `pipeline::embed_pending`); any failure only logs and returns empty: name vectors
/// are an aid to recall, and extraction does not fail because of them
async fn embed_names(
    state: &AppState,
    settings: &LlmSettings,
    client: &utopia_llm::LlmClient,
    names: &[(String, String)],
) -> HashMap<String, Vec<f32>> {
    let mut out: HashMap<String, Vec<f32>> = HashMap::new();
    for batch in names.chunks(NAME_EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let _permit = crate::llm_util::acquire_embed(state, settings).await;
        match client.embed(&texts).await {
            Ok(vectors) if vectors.len() == batch.len() => {
                out.extend(batch.iter().map(|(k, _)| k.clone()).zip(vectors));
            }
            Ok(vectors) => {
                tracing::warn!(
                    sent = batch.len(),
                    got = vectors.len(),
                    "名字向量数量对不上，这一批放弃"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "名字向量没算出来，这一批退回字面召回");
            }
        }
    }
    out
}

/// The `embed_names` task: fills in one batch of names still lacking vectors in this library; if a batch was full, there may be more, so
/// queue another to continue. Without an embedding model configured it does nothing; when settings change, the next document queues again
pub(crate) async fn embed_names_job(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    let Some(settings) = utopia_store::settings::get(&state.pool, kb.workspace_id).await? else {
        return Ok(());
    };
    let Some(client) = crate::llm_util::embed_client(&settings) else {
        return Ok(());
    };
    let filled = embed_pending_names(state, &settings, &client, kb_id).await?;
    if filled > 0 {
        tracing::info!(%kb_id, names = filled, "名字向量已补");
    }
    // After filling in names, queue another: while one is running, documents extracted meanwhile that hit "already running" do not queue (see the exclusion there),
    // and the names they wrote are filled by the next run; and a full batch means there may well be more. Only one is queued, so this runs at most one extra empty pass
    if filled > 0 {
        utopia_store::jobs::enqueue_unless_queued(
            &state.pool,
            "embed_names",
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    Ok(())
}

/// Fills in one batch of names in this library that still lack vectors (the names written recently are all in there).
/// Some of those computed during resolution are computed again here: resolution cannot get the id of the name fact (the name is written in
/// one statement of `create_entity`); saving only one embedding call is not worth changing resolution's return value for
async fn embed_pending_names(
    state: &AppState,
    settings: &LlmSettings,
    client: &utopia_llm::LlmClient,
    kb_id: Uuid,
) -> anyhow::Result<usize> {
    let pending =
        utopia_store::name_vectors::pending(&state.pool, kb_id, NAME_VECTOR_PENDING).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let mut items: Vec<(Uuid, Uuid, Vec<f32>)> = Vec::with_capacity(pending.len());
    for batch in pending.chunks(NAME_EMBED_BATCH) {
        let texts: Vec<String> = batch.iter().map(|(_, _, n)| n.clone()).collect();
        let _permit = crate::llm_util::acquire_embed(state, settings).await;
        let vectors = client.embed(&texts).await?;
        if vectors.len() != batch.len() {
            anyhow::bail!("嵌入返回 {} 条，送去的是 {} 条", vectors.len(), batch.len());
        }
        items.extend(
            batch
                .iter()
                .map(|(f, e, _)| (*f, *e))
                .zip(vectors)
                .map(|((f, e), v)| (f, e, v)),
        );
    }
    utopia_store::name_vectors::set(&state.pool, kb_id, &items).await?;
    Ok(items.len())
}

/// Names written in statements → entities. Looks first at the list this chunk gave, then at the ones identified earlier in this document (the list in the prompt).
///
/// **Described things are built only when used.** The model lists every noun phrase in a paragraph in `e`: measured in a library of 25 documents,
/// 616 of 848 entities were described things, of which 181 had no statement attached, and some were clauses of 155 characters. A description nobody points at
/// is not an entity, only part of a sentence: so descriptions are first recorded in `deferred`, and the first statement pointing to one
/// builds it; within one document the same description is built only once (`described`)
async fn place(
    pool: &PgPool,
    kb_id: Uuid,
    local: &mut HashMap<String, Uuid>,
    known: &HashMap<String, Uuid>,
    deferred: &HashMap<String, (String, String)>,
    described: &mut HashMap<String, Uuid>,
    name: &str,
) -> anyhow::Result<Option<Uuid>> {
    let key = name_key(name);
    if let Some(id) = local.get(&key).or_else(|| known.get(&key)) {
        return Ok(Some(*id));
    }
    let Some((text, kind)) = deferred.get(&key) else {
        return Ok(None);
    };
    let id = match described.get(&key) {
        Some(id) => *id,
        None => {
            let id = utopia_store::resolution::create_described(
                pool,
                kb_id,
                text,
                (!kind.is_empty()).then_some(kind.as_str()),
            )
            .await?;
            described.insert(key.clone(), id);
            id
        }
    };
    local.insert(key, id);
    Ok(Some(id))
}

/// `await_nod`: this is a memory log (0015): statements do not go into the store directly; they go unchanged to the pending-confirmation table, and become open statements only when a person nods.
/// `proposer`: who said that sentence and with which token (0026); it is recorded along with the pending item.
/// `pushed`: the chunk is the contract (the `statements` source of 0054): no prompt is built and the model is not asked; it is parsed directly;
/// in this case `client` (the chat model) is None; `settings` is passed if present, and the name-vector embedding model comes from it. The other steps are unchanged
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_open(
    state: &AppState,
    doc: &Document,
    kb: &KnowledgeBase,
    settings: Option<&LlmSettings>,
    client: Option<&utopia_llm::LlmClient>,
    my_epoch: i32,
    proposer: Proposer,
    await_nod: bool,
    pushed: bool,
    // 0071: the source's names are identities, so they get no name vectors
    identity: bool,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    let document_id = doc.id;
    let kb_id = kb.id;
    // Tell this document's story from the start in this round; clear the old signals first (re-extraction counts automatically)
    let _ = utopia_store::extraction_drops::clear_for_document(pool, document_id).await;
    let attested_at = dated_at(doc);
    let chunks = utopia_store::documents::chunks_for_extraction(pool, document_id).await?;
    // Memory logs are one sentence per chunk; the previous sentence is not the start of the next, so it is not attached (the same rule as the old path)
    let opening_chunk = if await_nod {
        None
    } else {
        utopia_store::documents::opening_chunk(pool, document_id).await?
    };
    let mut pending_count = 0usize;

    // Named things this document has already recognized, sorted by first appearance, sent into the prompts of later chunks;
    // statements point to them by name. Described things are not on this list: "a hospital" in the next chunk may not refer to the same one
    let mut doc_entities: Vec<(Uuid, String, String)> = Vec::new();
    let mut known_by_name: HashMap<String, Uuid> = HashMap::new();
    // Category words already bound to a class (0044 alignment, first slice): mentions go into resolution carrying their class, so same-name different-class ones can be told apart
    let bound_types = utopia_store::type_bindings::bound_map(pool, kb_id).await?;
    // Described things are reused within this document by their description text: "the northern wing" said three times in one chunk is one thing
    let mut described: HashMap<String, Uuid> = HashMap::new();
    let mut handled_by_name: HashMap<String, Vec<Uuid>> = HashMap::new();
    let mut ambiguous_bare_cache: HashMap<String, Uuid> = HashMap::new();
    let mut touched_names: HashSet<String> = HashSet::new();
    let mut unextracted: Vec<(i32, String)> = Vec::new();
    let mut needs_adjudication = false;
    let mut human_reviews_found = false;
    let mut statement_count = 0usize;

    // The embedding client for name vectors (0041 decision 3, channel 2). If no embedding model is configured it is None: recall falls back to
    // literal equality, and extraction proceeds as usual
    let embed = settings
        .and_then(crate::llm_util::embed_client)
        .filter(|_| !identity);
    // Dates the document itself states (0064 decision 1): each chunk reports them, along with the heading path they sit under. Nothing is passed
    // between chunks: the scope of a date is read from the headings in its own chunk's text, which still holds when chunks are extracted in parallel (#588)
    let mut time_entries: Vec<utopia_extract::time::TimeEntry> = Vec::new();
    let mut extracted_chunks: Vec<Uuid> = Vec::new();
    for chunk in chunks.iter() {
        // If taken over, exit quietly (re-extraction bumps the epoch): the check comes before calling the model
        if utopia_store::documents::extract_epoch(pool, document_id).await? != my_epoch {
            tracing::info!(%document_id, "抽取任务已被新一轮接管，退出");
            return Ok(());
        }
        let ctx: Option<&[f32]> = chunk.embedding.as_ref().map(|v| v.as_slice());
        let known: Vec<utopia_extract::KnownEntity> = doc_entities
            .iter()
            .enumerate()
            .map(|(index, (_, kind, name))| utopia_extract::KnownEntity {
                handle: format!("k{}", index + 1),
                type_key: kind.clone(),
                name: name.clone(),
            })
            .collect();
        let opening = opening_chunk
            .as_ref()
            .filter(|(id, _)| *id != chunk.id)
            .map(|(_, text)| text.as_str());
        // Whether the prompt includes the entities recognized by earlier chunks is a knob set up for measurement (#588); the write half is not governed by it
        let shown: &[utopia_extract::KnownEntity] = if state.extract_known_in_prompt {
            &known
        } else {
            &[]
        };
        let parsed = match call_and_parse_one_chunk(
            state,
            settings,
            client,
            embed.as_ref(),
            kb_id,
            document_id,
            &doc.filename,
            chunk,
            opening,
            shown,
            pushed,
            &mut unextracted,
        )
        .await?
        {
            Some(p) => p,
            None => continue,
        };
        // During the model's reply a rebuild may have deleted the entities in the cache (#1361); the old reply must not write facts or mark the chunk complete
        if utopia_store::documents::extract_epoch(pool, document_id).await? != my_epoch {
            tracing::info!(%document_id, "抽取任务已被新一轮接管，落库前退出");
            return Ok(());
        }
        let ChunkParsed {
            extraction,
            name_vecs,
        } = parsed;
        extracted_chunks.push(chunk.id);
        for entry in &extraction.time {
            // The word must appear verbatim in this chunk (the same rule as names and quotes); if it cannot be found it does not count
            let Some((start, _)) = locate(&chunk.text, &entry.words) else {
                drop_signal(
                    state,
                    kb_id,
                    document_id,
                    reason::TIME_NOT_IN_QUOTE,
                    "a date the passage was said to state is not in the passage",
                    Some(&entry.words),
                )
                .await;
                continue;
            };
            let scope =
                utopia_extract::time::headings_at(&chunk.text, usize::try_from(start).unwrap_or(0));
            if time_entries
                .iter()
                .any(|e| e.kind == entry.kind && e.words == entry.words && e.scope == scope)
            {
                continue;
            }
            time_entries.push(utopia_extract::time::TimeEntry {
                scope,
                chunk: Some(chunk.id.to_string()),
                char_start: Some(start),
                ..entry.clone()
            });
        }
        // On re-push, unchanged statements and names are carried over from the previous version (only on the push path, and when not waiting for a person's nod)
        let carried = if pushed && !await_nod {
            Carried {
                statements: utopia_store::pushed_memo::carry_over(
                    pool,
                    document_id,
                    chunk.id,
                    &PushedRaw::parse(&chunk.text).statements,
                )
                .await?,
                entities: utopia_store::pushed_memo::carried_entities(pool, document_id, chunk.id)
                    .await?,
            }
        } else {
            Carried::default()
        };
        apply_open_extraction(
            state,
            pool,
            kb_id,
            document_id,
            chunk,
            ctx,
            &extraction,
            name_vecs,
            attested_at,
            &bound_types,
            pushed,
            identity,
            await_nod,
            &carried,
            proposer,
            &mut pending_count,
            &mut statement_count,
            &mut needs_adjudication,
            &mut human_reviews_found,
            &mut doc_entities,
            &mut known_by_name,
            &mut described,
            &mut handled_by_name,
            &mut ambiguous_bare_cache,
            &mut touched_names,
        )
        .await?;
    }

    // The disambiguation suffix computed when an entity is created would come before its facts are written, so at the end refresh the names this document touches together
    for name in &touched_names {
        utopia_store::resolution::refresh_disambiguators(pool, kb_id, name).await?;
    }
    if utopia_store::documents::extract_epoch(pool, document_id).await? != my_epoch {
        tracing::info!(%document_id, "抽取任务已被新一轮接管，收尾时退出");
        return Ok(());
    }
    // A chunk that was not extracted must not be marked done (the same rule as the typed path). **But those that were extracted finish as usual**:
    // previously it returned right here, so the later time resolution and the pending-confirmation notification were both skipped: in a memory log, one
    // sentence that fails to extract would stop every later sentence's confirmation card from appearing (#1187). Errors are kept to report at the end
    let incomplete = incomplete_reason(&unextracted, chunks.len());
    if incomplete.is_none() {
        utopia_store::documents::set_graph_status(pool, document_id, "done").await?;
        state.emit_document(kb_id, document_id);
    }
    state.emit_graph(kb_id);
    // The document's time context (0064): the dates each chunk reported this round are merged into the copy stored on the document. Chunks not extracted
    // (unchanged passages claimed during incremental extraction) keep what they reported before; chunks extracted this round take this round's values.
    // If there are none, nothing is written: the parse step still reads the opening once (the old path is its fallback)
    let mut context: utopia_extract::time::DocumentDating = doc
        .time_context
        .clone()
        .and_then(|json| serde_json::from_value(json).ok())
        .unwrap_or_default();
    let redone: HashSet<String> = extracted_chunks.iter().map(Uuid::to_string).collect();
    context
        .entries
        .retain(|e| e.chunk.as_ref().is_some_and(|c| !redone.contains(c)));
    context.entries.extend(time_entries);
    if !context.entries.is_empty() {
        // The start point for the whole document: the shallowest-scope `now`, and at the same level, the one stated first. It is only the baseline for relative time words
        // when there is no nearer start point; it does not cover any fact (0064 decision 2)
        let whole = context
            .entries
            .iter()
            .filter(|e| e.kind == "now")
            .min_by_key(|e| e.scope.len());
        context.date = whole.map(|e| e.from.clone());
        context.date_words = whole.map(|e| e.words.clone());
        utopia_store::documents::set_time_context(
            pool,
            document_id,
            &serde_json::to_value(&context)?,
        )
        .await?;
    }
    // Time-word resolution is a later task (0045): document time context + each mention's reading + computing the intervals
    utopia_store::jobs::enqueue_unless_queued(
        pool,
        "resolve_time",
        serde_json::json!({ "document_id": document_id }),
    )
    .await?;
    // Category words not yet bound when this extraction began, but bound midway: their class is applied to the entities built later in this document
    // (`bound_types` is a copy read at the start of extraction; the projection at the moment of writing cannot see entities built afterwards)
    let caught_up = utopia_store::type_bindings::apply_bound(pool, kb_id).await?;
    if caught_up > 0 {
        tracing::info!(%document_id, entities = caught_up, "抽取途中绑上的类补到了后建的实体上");
    }
    // Newly appearing category words are bound to classes (0044 alignment, first slice): per library, and if one is already queued in the library it is not duplicated
    utopia_store::jobs::enqueue_unless_queued(
        pool,
        "align_types",
        serde_json::json!({ "kb_id": kb_id }),
    )
    .await?;
    // Pushed documents (0054): when the phrase exactly matches the signature of a declared relation, the ontology judges it on the spot (#1242); queue another
    // materialization, since under a bound signature newly arriving statements become typed rows without waiting for the aligner to finish; in a library without a chat model the aligner
    // never finishes. If one is already queued it is not duplicated: pushing a hundred documents computes it only once
    if pushed {
        let bound = crate::phrase_alignment::bind_declared(pool, kb_id, document_id).await?;
        if bound > 0 {
            tracing::info!(%document_id, bound, "推送的短语按声明的关系绑上");
        }
        utopia_store::jobs::enqueue_unless_queued(
            pool,
            utopia_store::phrase_bindings::MATERIALIZE_KIND,
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
        // The entry is the contract (RC-006): the new version has read every chunk, and facts it no longer states are closed here
        if incomplete.is_none() {
            let closing =
                utopia_store::unconfirmed::close_of_document(pool, kb_id, document_id).await?;
            if closing.closed > 0 {
                tracing::info!(%document_id, closed = closing.closed, "推送条目的新版本闭合了它不再陈述的事实");
            }
        }
    }
    // Only wake people when something was added to the queue: the Review count and the confirmation card in the conversation both depend on this signal
    if pending_count > 0 {
        tracing::info!(%document_id, pending_count, "记忆抽出的陈述进了待确认队列");
        state.emit_pending(kb_id);
        state.emit_review(kb_id);
    }

    // Identity source (0071 decision 5): a name vector written in prose earlier, which this document gives identity evidence for, is deleted
    if identity {
        utopia_store::name_vectors::drop_for_document(pool, document_id).await?;
    }
    // Name vectors: the name facts this document newly wrote get their vectors filled in (0041 decision 3, channel 2). They are filled in a per-library
    // `embed_names` task that de-duplicates (#1296): finding names that still lack vectors means scanning the library's name facts, and during a push of hundreds of documents
    // scanning once per document is one of the most expensive parts of a push; if one is queued **or running**, another is not queued. Only a queued one is checked:
    // as soon as the first starts running, every document extracted afterwards queues another one; while one runs for hours (a library with many identity names, #1472),
    // a dozen or so queue up behind it, each scanning again; when a running one ends with names still unfilled, it queues another itself (`embed_names_job`)
    if settings.is_some() && embed.is_some() {
        utopia_store::jobs::enqueue_unless_pending(
            pool,
            "embed_names",
            serde_json::json!({ "kb_id": kb_id }),
            std::time::Duration::ZERO,
        )
        .await?;
    }

    // Ambiguous pairs entered the review queue → governance / adjudication task; if one is already queued for this library, it is not duplicated.
    // Does not queue type resolution or automatic ontology expansion: the open graph has no classes or relations to expand; that is alignment's job
    if kb.governance {
        if needs_adjudication || human_reviews_found {
            utopia_store::jobs::enqueue_unless_queued(
                pool,
                "govern",
                serde_json::json!({ "kb_id": kb_id }),
            )
            .await?;
        }
    } else if needs_adjudication {
        utopia_store::jobs::enqueue_unless_queued(
            pool,
            "adjudicate_entities",
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    if needs_adjudication || human_reviews_found {
        state.emit_review(kb_id);
    }
    if let Some(msg) = incomplete {
        return Err(anyhow::anyhow!(msg));
    }
    tracing::info!(%document_id, statements = statement_count, "开放图谱抽取完成");
    Ok(())
}

#[cfg(test)]
#[path = "extraction_open_known_flow_tests.rs"]
mod known_flow_tests;

#[cfg(test)]
#[path = "extraction_open_identity_names_tests.rs"]
mod identity_names_tests;

#[cfg(test)]
#[path = "extraction_open_out_of_credit_tests.rs"]
mod out_of_credit_tests;

#[cfg(test)]
#[path = "extraction_open_name_vectors_job_tests.rs"]
mod name_vectors_job_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// #729: if this number is not in the quote, the statement has no basis. The discard rule must err on the lenient side
    #[test]
    fn a_value_must_be_in_the_sentence_that_is_quoted() {
        let row = "| Cost of revenue | $46 | $47 | $93 |";
        assert!(shows_value(row, "$46"));
        assert!(shows_value(row, "46"), "货币符号不算数");
        assert!(
            shows_value("| Accounts payable | 1,915 |", "1915"),
            "千分位不算数"
        );
        assert!(shows_value("revenue was $ 1,234", "$1,234"), "空白不算数");
        assert!(
            !shows_value(
                "(A) Acquisition-related costs are comprised of amortization.",
                "$46"
            ),
            "导语里没有这个数——正是压平表格那一行的错法"
        );
        assert!(shows_value("anything", ""), "没有值就没什么可核的");
        assert!(
            shows_value("| a | 4,600 |", "46"),
            "放得松：像是命中就放过，丢错一条比留下一条可疑的更糟"
        );
    }

    #[test]
    fn offsets_are_in_characters_not_bytes() {
        let text = "北京的冬天很冷。去年冬天下了三场雪。";
        assert_eq!(locate(text, "去年冬天"), Some((8, 12)));
        assert_eq!(locate(text, "  去年冬天 "), Some((8, 12)));
        assert_eq!(locate(text, "前年冬天"), None);
        assert_eq!(locate(text, ""), None);
    }

    /// PDF text layers break lines at line ends, and the model copies them back as spaces: a run of whitespace matches a run of whitespace, and offsets are computed from the source
    #[test]
    fn a_line_break_in_the_passage_matches_a_space_in_the_copy() {
        let text = "1. Succession. Zhang Wei will step down as Chief Technology Officer on December\n   31, 2024. Li Na follows.";
        let quote = "Zhang Wei will step down as Chief Technology Officer on December 31, 2024.";
        let span = locate(text, quote).expect("the sentence is in the passage");
        let found: String = text
            .chars()
            .skip(span.0 as usize)
            .take((span.1 - span.0) as usize)
            .collect();
        assert_eq!(
            found,
            "Zhang Wei will step down as Chief Technology Officer on December\n   31, 2024."
        );
        // The position of the time word within its source sentence, not its position within the quote: a line break plus indentation is longer than one space
        let start = locate_time(text, Some((quote, Some(span))), "December 31, 2024").unwrap();
        assert_eq!(
            text.chars()
                .skip(start as usize)
                .take(8)
                .collect::<String>(),
            "December"
        );
        // Conversely, too: the source has one space and the copy has extra whitespace
        assert_eq!(locate("a b", "a  \t b"), Some((0, 3)));
        // Compared character by character apart from whitespace: a copy broken mid-word, or with changed letters, does not count
        assert_eq!(locate("Septem\nber 1, 2024", "September 1, 2024"), None);
        assert_eq!(locate(text, "December 30, 2024"), None);
        assert_eq!(locate("   ", " "), None);
    }

    #[test]
    fn a_time_is_found_inside_its_own_sentence_first() {
        let text = "In 2019 the plant opened. In 2019 it closed again.";
        let quote = ("In 2019 it closed again.", Some((26, 50)));
        assert_eq!(locate_time(text, Some(quote), "2019"), Some(29));
        // Within the quote, and the quote itself was not located: the start falls back to the first occurrence within the whole chunk
        assert_eq!(
            locate_time(text, Some(("it closed in 2019", None)), "2019"),
            Some(3)
        );
        // A time not in this sentence's quote does not count for this statement, even if it appears elsewhere in the chunk
        assert_eq!(
            locate_time(text, Some(("the plant opened.", Some((8, 25)))), "2019"),
            None
        );
        assert_eq!(locate_time(text, None, "2019"), None);
    }

    #[test]
    fn a_table_row_takes_its_period_from_the_header_in_the_same_chunk() {
        let text = "|  | Q2 FY27 | Q1 FY27 |\n| --- | --- | --- |\n| Revenue | $96,221 | $81,615 |";
        let row = ("| Revenue | $96,221 | $81,615 |", Some((46, 76)));
        // The period number is in the header row, not in this row: table rows are searched within the whole chunk
        assert_eq!(locate_time(text, Some(row), "Q2 FY27"), Some(5));
        // The model dropped the leading "| " from the copy: the row the quote locates is still a table row
        let bare = ("Revenue | $96,221 | $81,615 |", Some((48, 76)));
        assert_eq!(locate_time(text, Some(bare), "Q1 FY27"), Some(15));
        // A quote that is not a table row still matches only its own sentence
        assert_eq!(
            locate_time(text, Some(("Revenue was up.", None)), "Q2 FY27"),
            None
        );
    }

    #[test]
    fn names_match_up_to_whitespace_and_case() {
        assert_eq!(name_key("  Harbor   Bridge "), "harbor bridge");
        assert_eq!(name_key("harbor bridge"), name_key("HARBOR BRIDGE"));
        assert_ne!(name_key("Harbor Bridge"), name_key("Harbour Bridge"));
    }
}
