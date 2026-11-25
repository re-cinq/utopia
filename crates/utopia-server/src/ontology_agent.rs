//! Ontology agent (0061 decisions 2 and 3): phrase shapes and category words that alignment judged "none" or "undecided" are handed, together with the existing ontology and
//! the capability questions, to the model, which proposes the classes and properties to create. Proposals go into `ontology_proposals`; **they become ontology only when a person adopts them**
//! (0012). After adoption builds the element, one alignment pass is queued, and those shapes re-judge themselves by the 0053 fingerprint.
//!
//! A shape's identity is the phrase plus the class keys at both ends plus whether the object is a literal: the same as the `phrase_bindings` key, except that keys are stored in the proposal
//! rather than ids, so a person can read them

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use utopia_core::models::{RelationAxioms, RelationTypeView};
use utopia_core::AppError;
use utopia_extract::ontology_agent as agent;
use utopia_store::agent_reviews::{self, Review};
use utopia_store::ontology::StoredProposal;
use utopia_store::phrase_bindings::PhraseSignature;
use utopia_store::type_bindings::KindWordSignature;
use utopia_store::{competency_questions, phrase_bindings, type_bindings};
use uuid::Uuid;

use crate::extraction::chat_retrying_rate_limits_at;
use crate::llm_util;
use crate::state::AppState;

/// Ask this many questions at once for a KB that has none
pub(crate) const QUESTIONS_PER_ROUND: usize = 10;
/// How many of the most-mentioned shapes / category words / entities to look at when raising questions
const TOP_SIGNATURES: usize = 40;
const TOP_KIND_WORDS: usize = 20;
const TOP_ENTITIES: usize = 20;

/// If more properties than this structurally match a shape, keep only this many nearest by vector, with definitions (same as the alignment shortlist)
const GLOSSARY_PER_SHAPE: usize = 10;
/// For each shape, this many properties nearest in meaning get definitions even if their structure does not match
const GLOSSARY_NEAREST_ANYWAY: usize = 5;

/// Number of shapes handed to the model per call (cost lesson from 0044 decision 3: the vocabulary is sent once per batch, not per shape)
pub(crate) const SIGNATURES_PER_CALL: usize = 12;
/// Each batch carries at most this many category words
pub(crate) const KIND_WORDS_PER_CALL: usize = 6;
/// Ask at most this many shapes per round, most statements first; the rest wait for the next round. Even clicking once in the UI spends only this much
pub(crate) const SIGNATURES_PER_ROUND: usize = 120;
/// The agent is brought in only if at least this many shapes went unbound when alignment wrapped up (fewer than this and a person handles them on the review page)
pub(crate) const TRIGGER_UNBOUND: usize = 20;

/// How a shape is written into a proposal
fn shape_of(sig: &PhraseSignature) -> Value {
    json!({
        "phrase": sig.phrase,
        "subject": sig.subject_type_key,
        "object": sig.object_type_key,
        "value": sig.object_is_value,
        // Proposals show the first example; keep its identity from the same read.
        "example_statement_ids": sig.example_statement_ids.first().into_iter().collect::<Vec<_>>(),
    })
}

/// Job entry point: only one run per KB at a time (same as both alignment kinds; running in parallel would hit the endpoint with 502s)
#[cfg(test)]
pub async fn propose(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    propose_with(state, kb_id, false).await
}

/// `full_glossary` is a measurement knob (`scripts/bench/agent.mjs`): every batch sends the definitions of all properties,
/// so that failure rate, duplicate proposals and tokens can be compared with the per-batch trimmed vocabulary on the same batch of shapes. The product path does not turn it on
pub async fn propose_with(
    state: &AppState,
    kb_id: Uuid,
    full_glossary: bool,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    let kb = utopia_store::kbs::get(pool, kb_id).await?;
    let settings = utopia_store::settings::get(pool, kb.workspace_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot propose ontology"))?;
    // Proposing an ontology is a judgment call: think at the endpoint's default strength
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot propose ontology"))?;
    let mut guard = pool.acquire().await?;
    let locked: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_lock(hashtext('propose_ontology'), hashtext($1))",
    )
    .bind(kb_id.to_string())
    .fetch_one(&mut *guard)
    .await?;
    if !locked {
        tracing::info!(%kb_id, "本体代理已有一份在跑，这次跳过");
        return Ok(());
    }
    let result = propose_locked(state, &kb, &settings, &client, full_glossary).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext('propose_ontology'), hashtext($1))")
        .bind(kb_id.to_string())
        .execute(&mut *guard)
        .await;
    result
}

/// One merged proposal: the same key was proposed in several batches, so the shapes, category words and questions are combined, and the definition is taken from the first time
struct Merged {
    proposal: agent::Proposal,
    /// Some = "already in the ontology": relation | attribute | class, goes into map_to.
    /// The direction is recorded on each shape (for the same property, "X wrote Y" reads in reverse, "Y's author X" reads forward)
    map_kind: Option<String>,
    shapes: Vec<Value>,
    phrases: Vec<String>,
    words: Vec<String>,
    /// When reusing, the moment at which the agent read each category word's decision; `seen_decided_at` on the same shape
    words_seen: serde_json::Map<String, Value>,
    quotes: Vec<String>,
    serves: Vec<Uuid>,
}

async fn propose_locked(
    state: &AppState,
    kb: &utopia_core::models::KnowledgeBase,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
    full_glossary: bool,
) -> anyhow::Result<()> {
    let kb_id = kb.id;
    let pool = &state.pool;
    let classes = utopia_store::graph::entity_types(pool, kb_id).await?;
    let props = utopia_store::ontology::relation_type_views(pool, kb_id).await?;
    let class_key: HashMap<Uuid, &str> = classes.iter().map(|c| (c.id, c.key.as_str())).collect();
    let class_keys: HashSet<&str> = classes.iter().map(|c| c.key.as_str()).collect();
    let prop_keys: HashSet<&str> = props.iter().map(|p| p.key.as_str()).collect();
    let prop_kind: HashMap<&str, &str> = props
        .iter()
        .map(|p| (p.key.as_str(), p.kind.as_str()))
        .collect();
    // Fingerprint of the vocabulary: class keys + property keys. A shape the agent saw but did not propose, or answered "already exists", is not sent again while the vocabulary is unchanged
    let basis = {
        let mut keys: Vec<&str> = class_keys.iter().copied().collect();
        keys.sort_unstable();
        let mut pkeys: Vec<&str> = prop_keys.iter().copied().collect();
        pkeys.sort_unstable();
        let digest = Sha256::digest(format!("{}##{}", keys.join(" "), pkeys.join(" ")).as_bytes());
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };

    // Those already proposed (whether or not the agent took a position) are not proposed again: adopted ones await alignment re-judging; rejected ones a person has already said no to;
    // open ones a person has not looked at yet. Seen-but-not-proposed and answered "already exists" are sent again only when the vocabulary changes (cut 1.1: in the first real run
    // 85 of 120 were not proposed, and they came back at the front of the next round)
    // A seen-but-not-proposed shape records the statement count at the time (basis = "count:n"): it is sent again only when the statement count doubles. Adding one
    // property need not be asked of the agent again, since alignment itself re-judges with the new property (0053 fingerprint); asking the agent would only burn all the
    // unproposed shapes again after each adoption. "Already exists" answers record the vocabulary fingerprint: sent again when the vocabulary changes
    let mut proposed_shapes: HashSet<String> = HashSet::new();
    let mut proposed_words: HashSet<String> = HashSet::new();
    let mut declined_shapes: HashMap<String, i64> = HashMap::new();
    let mut declined_words: HashMap<String, i64> = HashMap::new();
    for r in agent_reviews::list(pool, kb_id).await? {
        let word = r.shape.get("kind_word").and_then(Value::as_str);
        if r.outcome == "declined" {
            let seen_count = r
                .basis
                .strip_prefix("count:")
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or(1);
            match (r.kind.as_str(), word) {
                ("phrase", _) => {
                    declined_shapes.insert(shape_key(&r.shape), seen_count);
                }
                (_, Some(w)) => {
                    declined_words.insert(w.to_string(), seen_count);
                }
                _ => {}
            }
            continue;
        }
        if r.outcome != "proposed" && r.basis != basis {
            continue;
        }
        match (r.kind.as_str(), word) {
            ("phrase", _) => {
                proposed_shapes.insert(shape_key(&r.shape));
            }
            (_, Some(w)) => {
                proposed_words.insert(w.to_string());
            }
            _ => {}
        }
    }
    for s in utopia_store::ontology::signatures_already_proposed(pool, kb_id).await? {
        for p in s
            .get("phrases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            proposed_shapes.insert(shape_key(p));
        }
        for w in s
            .get("kind_words")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(w) = w.as_str() {
                proposed_words.insert(w.to_string());
            }
        }
    }

    // Shapes that did not bind: those alignment judged "none" or "undecided". Those not yet judged wait for alignment and are not guessed at here
    let sigs = phrase_bindings::signatures(pool, kb_id).await?;
    let bindings: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
        .await?
        .into_iter()
        .map(|b| (b.key(), b))
        .collect();
    let mut open: Vec<&PhraseSignature> = sigs
        .iter()
        .filter(|s| {
            matches!(
                bindings.get(&s.key()).map(|b| b.status.as_str()),
                Some("none" | "undecided")
            )
        })
        .filter(|s| {
            let key = shape_key(&shape_of(s));
            !proposed_shapes.contains(&key)
                && declined_shapes
                    .get(&key)
                    .is_none_or(|seen| s.count >= seen * 2)
        })
        .collect();
    open.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.phrase.cmp(&b.phrase)));
    open.truncate(SIGNATURES_PER_ROUND);

    // Unclassified category words likewise
    let words = type_bindings::signatures(pool, kb_id).await?;
    let word_bindings: HashMap<String, _> = type_bindings::bindings(pool, kb_id)
        .await?
        .into_iter()
        .map(|b| (b.kind_word.clone(), b))
        .collect();
    let mut open_words: Vec<&KindWordSignature> = words
        .iter()
        .filter(|w| {
            matches!(
                word_bindings.get(&w.kind_word).map(|b| b.status.as_str()),
                Some("none" | "undecided")
            )
        })
        .filter(|w| {
            !proposed_words.contains(&w.kind_word)
                && declined_words
                    .get(&w.kind_word)
                    .is_none_or(|seen| w.count >= seen * 2)
        })
        .collect();
    open_words.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.kind_word.cmp(&b.kind_word))
    });
    let calls = open
        .len()
        .div_ceil(SIGNATURES_PER_CALL)
        .max(open_words.len().div_ceil(KIND_WORDS_PER_CALL));
    if calls == 0 {
        tracing::info!(%kb_id, "本体代理：没有新的形状要看");
        return Ok(());
    }
    open_words.truncate(calls * KIND_WORDS_PER_CALL);

    // A KB with no questions at all (decision 1): have the agent raise questions first; the answer counts once a person accepts them; this round still proposes by phrase as before
    if competency_questions::list(pool, kb_id).await?.is_empty() {
        if let Err(e) = propose_questions_with(state, kb_id, settings, client).await {
            tracing::warn!(%kb_id, error = %e, "本体代理：提问题失败，这一轮不带问题");
        }
    }
    let questions = competency_questions::accepted(pool, kb_id).await?;
    let q_items: Vec<agent::QuestionItem<'_>> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| agent::QuestionItem {
            id: i as i64,
            text: &q.question,
        })
        .collect();
    let q_ids: HashSet<i64> = (0..questions.len() as i64).collect();
    let keys_of = |ids: &[Uuid]| -> Vec<&str> {
        ids.iter()
            .filter_map(|id| class_key.get(id).copied())
            .collect()
    };
    // The vocabulary is trimmed per batch (first real run: 98 properties with definitions cost 4k tokens per call, more than half of one call).
    // Each shape gets definitions only for the properties that structurally match; if too many match, keep the nearest few by vector: the criterion is the same
    // as the alignment candidates and shortlist. The remaining properties report only key and label, so the model knows they are there
    let closure =
        crate::phrase_alignment::closures(classes.iter().map(|c| (c.id, c.parents.as_slice())));
    let nearest: Vec<Vec<Uuid>> = {
        let queries: Vec<String> = open
            .iter()
            .map(|s| match (s.examples.first(), s.quotes.first()) {
                (Some(e), Some(q)) => format!("{} · {e} · {q}", s.phrase),
                (Some(e), None) => format!("{} · {e}", s.phrase),
                _ => s.phrase.clone(),
            })
            .collect();
        match crate::ontology_index::nearest_for_each(
            state,
            kb_id,
            &queries,
            GLOSSARY_PER_SHAPE as i64,
            crate::ontology_index::Target::Predicate(None),
        )
        .await
        {
            Ok(v) => v
                .into_iter()
                .map(|cands| cands.into_iter().map(|c| c.id).collect())
                .collect(),
            Err(e) => {
                tracing::warn!(%kb_id, error = %e, "本体代理：形状向量没算出来，词表按结构裁");
                vec![Vec::new(); open.len()]
            }
        }
    };
    let glossary_for = |ids: std::ops::Range<usize>| -> agent::Glossary<'_> {
        let mut full: HashSet<Uuid> = HashSet::new();
        if full_glossary {
            full.extend(props.iter().map(|p| p.id));
        }
        for i in ids {
            let (Some(s), near) = (open.get(i), nearest.get(i)) else {
                continue;
            };
            let fitting: Vec<&RelationTypeView> = props
                .iter()
                .filter(|p| crate::phrase_alignment::structurally_fits(p, s, &closure))
                .collect();
            // The few nearest in meaning get definitions regardless of whether structure matches: a shape with an untyped end, or a literal object,
            // does not structurally match positionHeld / workLocation, and without a definition the model proposed them again
            // (in a controlled comparison duplicate proposals went from 1% to 6%; seven of ten came from this)
            if let Some(near) = near {
                full.extend(near.iter().take(GLOSSARY_NEAREST_ANYWAY).copied());
            }
            if fitting.len() > GLOSSARY_PER_SHAPE {
                if let Some(near) = near.filter(|n| !n.is_empty()) {
                    full.extend(
                        fitting
                            .iter()
                            .filter(|p| near.contains(&p.id))
                            .map(|p| p.id),
                    );
                    continue;
                }
            }
            full.extend(fitting.iter().map(|p| p.id));
        }
        agent::Glossary {
            classes: classes
                .iter()
                .map(|c| (c.key.as_str(), c.label.as_str(), c.description.as_str()))
                .collect(),
            properties: props
                .iter()
                .filter(|p| full.contains(&p.id))
                .map(|p| {
                    (
                        p.key.as_str(),
                        p.label.as_str(),
                        p.kind.as_str(),
                        keys_of(&p.domains),
                        keys_of(&p.ranges),
                        p.description.as_str(),
                    )
                })
                .collect(),
            others: props
                .iter()
                .filter(|p| !full.contains(&p.id))
                .map(|p| (p.key.as_str(), p.label.as_str()))
                .collect(),
        }
    };
    let sig_items: Vec<agent::OpenSignature<'_>> = open
        .iter()
        .enumerate()
        .map(|(i, s)| agent::OpenSignature {
            id: i as i64,
            phrase: &s.phrase,
            subject_class: s.subject_type_key.as_deref(),
            object_class: s.object_type_key.as_deref(),
            object_is_value: s.object_is_value,
            statement_count: s.count,
            examples: &s.examples,
            quotes: &s.quotes,
        })
        .collect();
    let word_items: Vec<agent::OpenKindWord<'_>> = open_words
        .iter()
        .enumerate()
        .map(|(i, w)| agent::OpenKindWord {
            id: i as i64,
            kind_word: &w.kind_word,
            count: w.count,
            examples: &w.examples,
            phrases: &w.phrases,
        })
        .collect();
    tracing::info!(%kb_id, shapes = open.len(), kind_words = open_words.len(), questions = questions.len(), calls, "本体代理开始");
    // Reused proposals record the moment the agent's read decision was made. A person's decision at adoption time is not that moment: it was made after the agent read it,
    // and reuse does not overwrite it. Do not compare against the proposal row's stored time: that comes after all the model calls, and moves again when the next round merges into the same row
    let seen_shape = |s: &PhraseSignature| bindings.get(&s.key()).map(|b| b.decided_at);
    let seen_word = |w: &KindWordSignature| word_bindings.get(&w.kind_word).map(|b| b.decided_at);

    let mut merged: HashMap<String, Merged> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    // Where each shape seen this round went (batches whose call failed are not recorded; they are sent again next round)
    let mut reviews: Vec<Review> = Vec::new();
    let (mut failed, mut malformed, mut skipped_existing) = (0usize, 0usize, 0usize);
    for b in 0..calls {
        let sig_batch = sig_items
            .get(b * SIGNATURES_PER_CALL..)
            .map(|rest| &rest[..rest.len().min(SIGNATURES_PER_CALL)])
            .unwrap_or(&[]);
        let word_batch = word_items
            .get(b * KIND_WORDS_PER_CALL..)
            .map(|rest| &rest[..rest.len().min(KIND_WORDS_PER_CALL)])
            .unwrap_or(&[]);
        let sig_ids: HashSet<i64> = sig_batch.iter().map(|s| s.id).collect();
        let word_ids: HashSet<i64> = word_batch.iter().map(|w| w.id).collect();
        let glossary = glossary_for(b * SIGNATURES_PER_CALL..(b + 1) * SIGNATURES_PER_CALL);
        let messages = agent::build_proposal_messages(
            sig_batch,
            word_batch,
            &glossary,
            &q_items,
            &kb.ontology_lang,
        );
        let reply =
            match chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0)).await
            {
                Ok(r) => r,
                Err(e) => {
                    if utopia_core::error::is_terminal(&e) {
                        return Err(e);
                    }
                    failed += 1;
                    tracing::warn!(%kb_id, batch = b, error = %e, "本体代理：这一批调用失败");
                    continue;
                }
            };
        let parsed = match agent::parse_proposal_response(&reply.text, &sig_ids, &word_ids, &q_ids)
        {
            Ok(p) => p,
            Err(e) => {
                failed += 1;
                tracing::warn!(%kb_id, batch = b, error = %e, "本体代理：回复读不出");
                continue;
            }
        };
        malformed += parsed.malformed;
        for why in &parsed.reasons {
            tracing::warn!(%kb_id, batch = b, why = %why, "本体代理：坏项");
        }
        let mut sig_outcome: HashMap<i64, (&str, String)> = HashMap::new();
        let mut word_outcome: HashMap<i64, (&str, String)> = HashMap::new();
        // "Already in the ontology": stored as a map_to proposal; a person clicking "use existing" is a person's binding decision
        for e in &parsed.existing {
            let map_kind = if let Some(k) = prop_kind.get(e.key.as_str()) {
                (*k).to_string()
            } else if class_keys.contains(e.key.as_str()) {
                "class".to_string()
            } else {
                malformed += 1;
                tracing::warn!(%kb_id, batch = b, why = %format!("existing: no such key in the ontology: {}", e.key), "本体代理：坏项");
                continue;
            };
            let slot = format!("map_to:{}", e.key);
            let entry = merged.entry(slot.clone()).or_insert_with(|| {
                order.push(slot);
                Merged {
                    proposal: agent::Proposal {
                        kind: "existing".into(),
                        key: e.key.clone(),
                        label: e.key.clone(),
                        definition: String::new(),
                        value: false,
                        datatype: None,
                        domains: Vec::new(),
                        ranges: Vec::new(),
                        parents: Vec::new(),
                        signatures: Vec::new(),
                        kind_words: Vec::new(),
                        questions: Vec::new(),
                    },
                    map_kind: Some(map_kind),
                    shapes: Vec::new(),
                    phrases: Vec::new(),
                    words: Vec::new(),
                    words_seen: serde_json::Map::new(),
                    quotes: Vec::new(),
                    serves: Vec::new(),
                }
            });
            if map_kind_is_class(entry) {
                // A class attaches to category words; a shape pointing at a class is a mis-filled cell and is not recorded
                for id in &e.kind_words {
                    if let Some(w) = open_words.get(*id as usize) {
                        push_word(entry, w, seen_word(w));
                        word_outcome.insert(*id, ("existing", e.key.clone()));
                    }
                }
            } else {
                for id in &e.signatures {
                    if let Some(sig) = open.get(*id as usize) {
                        push_shape(entry, sig, e.direction.as_deref(), seen_shape(sig));
                        sig_outcome.insert(*id, ("existing", e.key.clone()));
                    }
                }
            }
        }
        for p in parsed.proposals {
            // The model was told not to propose what already exists; if it proposed it anyway, treat it as not said (alignment will attach the shape to it)
            let taken = match p.kind.as_str() {
                "class" => class_keys.contains(p.key.as_str()),
                _ => prop_keys.contains(p.key.as_str()),
            };
            if taken {
                skipped_existing += 1;
                continue;
            }
            let slot = format!("{}:{}", p.kind, p.key);
            let entry = merged.entry(slot.clone()).or_insert_with(|| {
                order.push(slot);
                Merged {
                    proposal: agent::Proposal {
                        signatures: Vec::new(),
                        kind_words: Vec::new(),
                        questions: Vec::new(),
                        ..p.clone()
                    },
                    map_kind: None,
                    shapes: Vec::new(),
                    phrases: Vec::new(),
                    words: Vec::new(),
                    words_seen: serde_json::Map::new(),
                    quotes: Vec::new(),
                    serves: Vec::new(),
                }
            });
            let target = format!("{}:{}", section_of(&p), p.key);
            // New elements do not record the moment they were read: adopting one replaces no decision, and no one reads this cell
            for id in p.signatures {
                let Some(s) = open.get(id as usize) else {
                    continue;
                };
                push_shape(entry, s, None, None);
                sig_outcome.insert(id, ("proposed", target.clone()));
            }
            for id in p.kind_words {
                let Some(w) = open_words.get(id as usize) else {
                    continue;
                };
                push_word(entry, w, None);
                word_outcome.insert(id, ("proposed", target.clone()));
            }
            for id in p.questions {
                let Some(q) = questions.get(id as usize) else {
                    continue;
                };
                if !entry.serves.contains(&q.id) {
                    entry.serves.push(q.id);
                }
            }
        }
        for si in sig_batch {
            let (outcome, target) = sig_outcome
                .get(&si.id)
                .map(|(o, t)| (*o, Some(t.clone())))
                .unwrap_or(("declined", None));
            reviews.push(Review {
                kind: "phrase".into(),
                shape: shape_of(open[si.id as usize]),
                outcome: outcome.into(),
                target,
                basis: if outcome == "declined" {
                    format!("count:{}", open[si.id as usize].count)
                } else {
                    basis.clone()
                },
            });
        }
        for wi in word_batch {
            let (outcome, target) = word_outcome
                .get(&wi.id)
                .map(|(o, t)| (*o, Some(t.clone())))
                .unwrap_or(("declined", None));
            reviews.push(Review {
                kind: "kind_word".into(),
                shape: json!({ "kind_word": wi.kind_word }),
                outcome: outcome.into(),
                target,
                basis: if outcome == "declined" {
                    format!("count:{}", wi.count)
                } else {
                    basis.clone()
                },
            });
        }
    }

    let items: Vec<utopia_store::ontology::AgentProposal> = order
        .iter()
        .filter_map(|slot| merged.get(slot))
        .filter(|m| !m.shapes.is_empty() || !m.words.is_empty())
        .map(|m| {
            let p = &m.proposal;
            if let Some(map_kind) = &m.map_kind {
                let label = if map_kind == "class" {
                    classes
                        .iter()
                        .find(|c| c.key == p.key)
                        .map(|c| c.label.as_str())
                } else {
                    props
                        .iter()
                        .find(|r| r.key == p.key)
                        .map(|r| r.label.as_str())
                };
                return utopia_store::ontology::AgentProposal {
                    section: "map_to".into(),
                    key: p.key.clone(),
                    payload: json!({
                        "key": p.key,
                        "label": label.unwrap_or(p.key.as_str()),
                        "kind": map_kind,
                        "forms": m.phrases,
                        "kind_words": m.words,
                        "examples": m.quotes,
                        "proposed_by": "agent",
                    }),
                    serves: Vec::new(),
                    signatures: json!({ "phrases": m.shapes, "kind_words": m.words,
                                        "kind_words_seen_decided_at": m.words_seen }),
                };
            }
            let section = section_of(p);
            let mut payload = json!({
                "key": p.key,
                "label": p.label,
                "description": p.definition,
                "forms": m.phrases,
                "kind_words": m.words,
                "examples": m.quotes,
                "proposed_by": "agent",
            });
            match section {
                "entity_types" => {
                    payload["parents"] = json!(p.parents);
                }
                "attribute_types" => {
                    payload["datatype"] = json!(datatype_or_text(p.datatype.as_deref()));
                    payload["domains"] = json!(p.domains);
                }
                _ => {
                    payload["temporal"] = json!("state");
                    payload["domains"] = json!(p.domains);
                    payload["ranges"] = json!(p.ranges);
                }
            }
            utopia_store::ontology::AgentProposal {
                section: section.to_string(),
                key: p.key.clone(),
                payload,
                serves: m.serves.clone(),
                signatures: json!({ "phrases": m.shapes, "kind_words": m.words }),
            }
        })
        .collect();
    // The same target already had a batch of shapes answered in a previous round: merge them, do not overwrite (in the first real run the third round overwrote the second)
    let mut items = items;
    // Each new element carries the two existing elements nearest to it in the ontology (by vector, without asking the model). Under the trimmed vocabulary the model cannot see
    // the definitions of most properties, and in a controlled comparison proposals that duplicated or reversed an existing property rose from 1% to 5% (awardReceived,
    // performer are examples); the reviewer must see "which it most resembles" before adopting
    for class_side in [false, true] {
        let idx: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                it.section != "map_to" && (it.section == "entity_types") == class_side
            })
            .map(|(i, _)| i)
            .collect();
        let queries: Vec<String> = idx
            .iter()
            .map(|i| {
                let p = &items[*i].payload;
                format!(
                    "{} · {}",
                    p.get("label").and_then(Value::as_str).unwrap_or(""),
                    p.get("description").and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect();
        let target = if class_side {
            crate::ontology_index::Target::Class
        } else {
            crate::ontology_index::Target::Predicate(None)
        };
        match crate::ontology_index::nearest_for_each(state, kb_id, &queries, 2, target).await {
            Ok(found) => {
                for (i, cands) in idx.into_iter().zip(found) {
                    items[i].payload["closest"] = json!(cands
                        .iter()
                        .map(|c| json!({ "key": c.key, "label": c.label, "distance": c.distance }))
                        .collect::<Vec<_>>());
                }
            }
            Err(e) => {
                tracing::warn!(%kb_id, error = %e, "本体代理：最近的已有元素没算出来，提案不带这一格");
            }
        }
    }
    for it in items.iter_mut().filter(|it| it.section == "map_to") {
        if let Some(prev) =
            utopia_store::ontology::open_proposal(pool, kb_id, "map_to", &it.key).await?
        {
            merge_map_to(it, &prev);
        }
    }
    utopia_store::ontology::save_agent_proposals(pool, kb_id, &items).await?;
    agent_reviews::record(pool, kb_id, &reviews).await?;
    tracing::info!(%kb_id, proposals = items.len(), failed, malformed, skipped_existing, calls, glossary = if full_glossary { "full" } else { "trimmed" }, "本体代理结束");
    if !items.is_empty() {
        state.emit_pending(kb_id);
    }
    Ok(())
}

fn section_of(p: &agent::Proposal) -> &'static str {
    match (p.kind.as_str(), p.value) {
        ("class", _) => "entity_types",
        (_, true) => "attribute_types",
        _ => "relation_types",
    }
}

fn map_kind_is_class(m: &Merged) -> bool {
    m.map_kind.as_deref() == Some("class")
}

/// The identity of a shape is written as one key: phrase + class keys at both ends + whether the object is a literal (direction does not count --
/// shapes in map_to carry a direction, which must be removed when comparing identity)
fn shape_key(v: &Value) -> String {
    json!({
        "phrase": v.get("phrase"),
        "subject": v.get("subject"),
        "object": v.get("object"),
        "value": v.get("value"),
    })
    .to_string()
}

/// A shape's identity: phrase + class keys at both ends + whether the object is a literal (direction does not count)
fn same_shape(a: &Value, b: &Value) -> bool {
    ["phrase", "subject", "object", "value"]
        .iter()
        .all(|k| a.get(k) == b.get(k))
}

fn push_shape(
    entry: &mut Merged,
    s: &PhraseSignature,
    direction: Option<&str>,
    seen: Option<DateTime<Utc>>,
) {
    let mut shape = shape_of(s);
    if let Some(d) = direction {
        shape["direction"] = json!(d);
    }
    if let Some(at) = seen {
        shape["seen_decided_at"] = json!(at);
    }
    if entry.shapes.iter().any(|x| same_shape(x, &shape)) {
        return;
    }
    entry.shapes.push(shape);
    if !entry.phrases.contains(&s.phrase) {
        entry.phrases.push(s.phrase.clone());
    }
    for q in s.quotes.iter().take(1) {
        if entry.quotes.len() < 3 && !entry.quotes.contains(q) {
            entry.quotes.push(q.clone());
        }
    }
}

fn push_word(entry: &mut Merged, w: &KindWordSignature, seen: Option<DateTime<Utc>>) {
    if entry.words.contains(&w.kind_word) {
        return;
    }
    entry.words.push(w.kind_word.clone());
    if let Some(at) = seen {
        entry.words_seen.insert(w.kind_word.clone(), json!(at));
    }
    for e in w.examples.iter().take(1) {
        if entry.quotes.len() < 3 && !entry.quotes.contains(e) {
            entry.quotes.push(e.clone());
        }
    }
}

/// Merges the map_to row left open by the previous round into this round's: shapes, phrases, category words and examples are unioned, earlier ones first
fn merge_map_to(
    it: &mut utopia_store::ontology::AgentProposal,
    prev: &utopia_store::ontology::StoredProposal,
) {
    let mut shapes: Vec<Value> = prev
        .signatures
        .get("phrases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for s in it
        .signatures
        .get("phrases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !shapes.iter().any(|x| same_shape(x, s)) {
            shapes.push(s.clone());
        }
    }
    let union = |field: &str, from_payload: bool| -> Vec<String> {
        let (a, b) = if from_payload {
            (prev.payload.get(field), it.payload.get(field))
        } else {
            (prev.signatures.get(field), it.signatures.get(field))
        };
        let mut out = strings(a);
        for x in strings(b) {
            if !out.contains(&x) {
                out.push(x);
            }
        }
        out
    };
    let words = union("kind_words", false);
    let forms = union("forms", true);
    let examples: Vec<String> = union("examples", true).into_iter().take(3).collect();
    // Shapes come across carrying the moment each was read; category words, kept in a separate cell, must be merged as well. If only this round's is kept, the previous round's words lose
    // their read moment, and at adoption the person's decision that the agent read is no longer replaced
    let seen_of = |signatures: &Value| {
        signatures
            .get("kind_words_seen_decided_at")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let mut words_seen = seen_of(&prev.signatures);
    for (w, at) in seen_of(&it.signatures) {
        words_seen.entry(w).or_insert(at);
    }
    it.payload["forms"] = json!(forms);
    it.payload["kind_words"] = json!(words);
    it.payload["examples"] = json!(examples);
    it.signatures = json!({ "phrases": shapes, "kind_words": words,
                            "kind_words_seen_decided_at": words_seen });
}

fn datatype_or_text(d: Option<&str>) -> &str {
    match d {
        Some(d @ ("text" | "number" | "date" | "bool")) => d,
        _ => "text",
    }
}

/// The few cells a person changed at adoption. None = as proposed
#[derive(Debug, Default, Deserialize)]
pub struct AdoptEdits {
    pub label: Option<String>,
    pub description: Option<String>,
    pub datatype: Option<String>,
    pub domains: Option<Vec<String>>,
    pub ranges: Option<Vec<String>>,
    pub parents: Option<Vec<String>>,
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect()
}

/// Receipt of adoption
#[derive(Debug)]
pub struct Adopted {
    /// The id of the new element; when reusing, the id of the target
    pub id: Uuid,
    /// Shapes or category words left unwritten on reuse, each with what the person decided: decided by the person after the agent read it, and differing from the proposal.
    /// The two people's judgments differ, and the adopting person must see that, so they are listed rather than silently skipped
    pub skipped: Vec<Value>,
}

/// One binding's decision at this moment: who, when it was decided, and what it was decided as (comparisons use ids; the receipt writes keys)
#[derive(sqlx::FromRow)]
struct Decided {
    decided_by: String,
    decided_at: DateTime<Utc>,
    status: String,
    target_id: Option<Uuid>,
    target: Option<String>,
    #[sqlx(default)]
    direction: Option<String>,
}

impl Decided {
    /// Decided by the person after the agent read it. The proposal answered the decision the agent read, and that one is still replaced; what the person decided afterward, the proposal and
    /// the adopting person's card have never seen, so it is not overwritten. Only whether it is the moment the agent read is checked, not the order: `decided_at`
    /// is the moment the transaction that decided it began, so a person who started first and committed later has a time earlier than the one the agent read. A proposal with no recorded
    /// read moment (saved before this cell existed) leaves every person's decision unchanged
    fn by_person_after_read(&self, read: Option<DateTime<Utc>>) -> bool {
        self.decided_by == "person" && read.is_none_or(|at| at != self.decided_at)
    }

    /// Decided the same as the proposal: bound to the same property, same direction, or the same class (category words have no direction)
    fn agrees(&self, target: Uuid, direction: Option<&str>) -> bool {
        self.status == "bound"
            && self.target_id == Some(target)
            && self.direction.as_deref() == direction
    }
}

/// The `decided_at` the agent had when it read this binding (recorded by `push_shape` and `push_word`)
fn read_at(seen: Option<&Value>) -> Option<DateTime<Utc>> {
    seen.and_then(|v| serde_json::from_value(v.clone()).ok())
}

/// Adopt one agent proposal (0061 decision 3): lock the proposal, in one transaction re-read and lock the class it needs or the target it reuses,
/// write the element (for reuse, write the person's decision), the regression case, the adopted status and the follow-up jobs, committed once.
///
/// Before, these steps each committed separately: the property was saved first, and only when writing the regression case from the examples was it found that the object was an entity (`bad_property_shape`),
/// leaving an adopted-less property, an alignment queued, and a proposal whose key was already taken and could never be adopted again
pub async fn adopt(
    state: &AppState,
    kb_id: Uuid,
    section: &str,
    key: &str,
    edits: AdoptEdits,
    actor: Uuid,
) -> Result<Adopted, AppError> {
    let pool = &state.pool;
    let mut tx = pool.begin().await?;
    let (adopted, detail) = match adopt_on(&mut tx, kb_id, section, key, edits, actor).await {
        Ok(done) => done,
        Err(error) => {
            // Roll back, then answer: a request retried at once does not hit the lock that has not yet been released
            if let Err(rollback) = tx.rollback().await {
                return Err(AppError::Other(
                    anyhow::Error::new(error)
                        .context(format!("rolling back an adoption: {rollback}")),
                ));
            }
            return Err(busy(error));
        }
    };
    tx.commit().await?;
    let _ = utopia_store::audit::record(
        pool,
        Some(kb_id),
        actor,
        "ontology_proposal.adopted",
        section.trim_end_matches('s'),
        Some(adopted.id),
        detail,
    )
    .await;
    state.emit_graph(kb_id);
    if section == "map_to" {
        state.emit_review(kb_id);
    }
    Ok(adopted)
}

/// If adoption collides with another write and the whole transaction rolls back, nothing is left behind, and a retry is safe: answered with `alignment_busy`,
/// which the review page recognizes as retryable. A deadlock (40P01) answers this way wherever it hits in this transaction (for example when reusing a class re-points entities by word,
/// and merging with entities that an earlier edit changed and a later one merged deadlock each other), and the message does not name who was changing it. A lock wait timeout (55P03) happens only
/// in the class-reuse section (only it sets `lock_timeout`), and is answered with the same words the review page gives when a category word is classed
fn busy(error: AppError) -> AppError {
    if matches!(&error, AppError::Db(sqlx::Error::Database(e))
        if e.code().as_deref() == Some("40P01"))
    {
        return AppError::CodedConflict {
            code: "alignment_busy",
            message: "Another change touched what this adoption writes; nothing was saved. Please try again."
                .into(),
        };
    }
    type_bindings::busy_on_lock_timeout(error)
}

/// Looks up these classes by key and locks them against deletion (FOR KEY SHARE); the ids returned correspond one-to-one with the keys. Until adoption commits they cannot be deleted;
/// a class being deleted is waited on and then answers `unknown_class`, so it never writes the parent class or hits a foreign key at the domain step and answers 500. Locked in id order.
///
/// Not FOR SHARE: that would block every UPDATE on these rows. The vector backfill task (`set_type_embeddings`) in
/// one transaction updates a batch of classes in heap order, and FOR SHARE in id order would wait on it into a deadlock. Adoption only reads ids, so blocking deletion is enough
async fn lock_classes(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    keys: &[String],
) -> Result<Vec<Uuid>, AppError> {
    let found: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT key, id FROM entity_types WHERE kb_id = $1 AND key = ANY($2)
          ORDER BY id FOR KEY SHARE",
    )
    .bind(kb_id)
    .bind(keys)
    .fetch_all(&mut **tx)
    .await?;
    keys.iter()
        .map(|k| {
            found
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, id)| *id)
                .ok_or_else(|| {
                    AppError::invalid("unknown_class", format!("class `{k}` does not exist"))
                })
        })
        .collect()
}

/// The row adoption locks
#[derive(sqlx::FromRow)]
struct Locked {
    id: Uuid,
    #[sqlx(flatten)]
    proposal: StoredProposal,
}

/// Everything adoption does in its transaction; returns the receipt and the cells the audit should record
async fn adopt_on(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    section: &str,
    key: &str,
    edits: AdoptEdits,
    actor: Uuid,
) -> Result<(Adopted, Value), AppError> {
    // Lock the proposal first: another adoption of the same one, a rejection, and the agent's refresh all wait for this commit, after which it is no longer open
    // Only accept what the agent proposed: proposals left by the old Suggest have no adoption entry point
    let locked: Option<Locked> = sqlx::query_as(
        "SELECT id, section, key, payload, proposed_by, serves, signatures
           FROM ontology_proposals
          WHERE kb_id = $1 AND section = $2 AND key = $3 AND status = 'open' AND proposed_by = 'agent'
            FOR UPDATE",
    )
    .bind(kb_id)
    .bind(section)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(locked) = locked else {
        return Err(AppError::NotFound);
    };
    let p = &locked.proposal;
    // How many cells a person changed: the second number of decision 5 needs it
    let edited = edits.label.is_some()
        || edits.description.is_some()
        || edits.datatype.is_some()
        || edits.domains.is_some()
        || edits.ranges.is_some()
        || edits.parents.is_some();
    let label = edits
        .label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| p.payload.get("label").and_then(Value::as_str))
        .unwrap_or(key)
        .to_string();
    let description = edits
        .description
        .as_deref()
        .or_else(|| p.payload.get("description").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let mut skipped = Vec::new();
    let id = match section {
        // "Use existing": each shape gets a person's binding decision (as on the review page), and each category word is put into that class
        "map_to" => adopt_map_to(tx, kb_id, p, &mut skipped).await?,
        "entity_types" => {
            let parents = lock_classes(
                tx,
                kb_id,
                &edits
                    .parents
                    .unwrap_or_else(|| strings(p.payload.get("parents"))),
            )
            .await?;
            let id = utopia_store::ontology::create_entity_type(
                &mut **tx,
                kb_id,
                key,
                &label,
                utopia_store::palette::color_for_key(key),
                "circle",
                &parents,
                &description,
            )
            .await?;
            // One more class: category words that were "none" or "undecided" must be judged again, and phrase alignment queues behind it
            embed_then_align(tx, kb_id, "align_types").await?;
            id
        }
        "relation_types" | "attribute_types" => {
            let kind = if section == "attribute_types" {
                "attribute"
            } else {
                "relation"
            };
            let domains = edits
                .domains
                .unwrap_or_else(|| strings(p.payload.get("domains")));
            let ranges = if kind == "relation" {
                edits
                    .ranges
                    .unwrap_or_else(|| strings(p.payload.get("ranges")))
            } else {
                Vec::new()
            };
            // Domain and range locked in one go, the whole batch sorted by id
            let classes = lock_classes(tx, kb_id, &[domains.as_slice(), &ranges].concat()).await?;
            let (domains, ranges) = classes.split_at(domains.len());
            let datatype = if kind == "attribute" {
                Some(
                    datatype_or_text(
                        edits
                            .datatype
                            .as_deref()
                            .or_else(|| p.payload.get("datatype").and_then(Value::as_str)),
                    )
                    .to_string(),
                )
            } else {
                None
            };
            let id = utopia_store::ontology::create_relation_type(
                &mut **tx,
                kb_id,
                key,
                &label,
                "state",
                RelationAxioms::default(),
                &description,
                kind,
                domains,
                ranges,
                datatype.as_deref(),
                None,
            )
            .await?;
            // One more property: shapes judged none / undecided may now match (the 0053 fingerprint changed)
            embed_then_align(tx, kb_id, "align_phrases").await?;
            id
        }
        _ => {
            return Err(AppError::invalid(
                "bad_section",
                "section 只能是 map_to、entity_types、relation_types 或 attribute_types",
            ))
        }
    };
    finish_adoption(tx, kb_id, &locked, id, actor, edited, &skipped).await?;
    let detail =
        json!({ "key": key, "label": label, "proposed_by": p.proposed_by, "serves": p.serves });
    Ok((Adopted { id, skipped }, detail))
}

/// Follow-up jobs for the new element, committed together with the element: backfill vectors first, then align, in the same order as the editor. New elements need vectors, or
/// the alignment shortlist cannot see them (when candidates outnumber the shortlist length they are picked by vector; a property with no vector never enters the shortlist, so the shape's fingerprint
/// does not change and it is not re-judged even after adoption — this is how the first real run of bordered_by went). Previously adoption embedded in the request
/// (cut 1.1); now, when alignment starts and sees that a vector backfill is still queued or running, it waits for it (`align_*_reasking`).
/// Both are queued without checking for an identical one already queued: a queued one may be picked up before this commit and read an ontology that lacks this
/// element (0051)
async fn embed_then_align(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    align: &str,
) -> Result<(), AppError> {
    let payload = json!({ "kb_id": kb_id });
    utopia_store::jobs::enqueue_with_max_attempts_tx(tx, "embed_ontology", payload.clone(), 3)
        .await?;
    // Alignment is due a few seconds late, the same as the editor (vector backfill at 2 seconds, alignment at 5 seconds). If both were due at once, the workers would pick both up together,
    // alignment would see the backfill running, and move itself half a minute later. When the backfill is slower it is still waited for
    utopia_store::jobs::enqueue_with_max_attempts_after_tx(
        tx,
        align,
        payload,
        3,
        std::time::Duration::from_secs(5),
    )
    .await?;
    Ok(())
}

/// Regression case and adopted status (decision 4), inside the adoption transaction. Whether a person edited it (the second number of decision 5) is written by the same
/// UPDATE as the status: before, it was a separate statement after adoption committed. A shape left unwritten on reuse (`skipped`) gets no case: the person decided
/// differently from the proposal; one the person later decided the same is still written
async fn finish_adoption(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    locked: &Locked,
    property_id: Uuid,
    actor: Uuid,
    edited: bool,
    skipped: &[Value],
) -> Result<(), AppError> {
    let proposal = &locked.proposal;
    let mut examples = Vec::new();
    let property = matches!(
        proposal.section.as_str(),
        "relation_types" | "attribute_types"
    ) || (proposal.section == "map_to"
        && proposal.payload.get("kind").and_then(Value::as_str) != Some("class"));
    if property {
        for shape in proposal
            .signatures
            .get("phrases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|shape| !skipped.iter().any(|s| same_shape(s, shape)))
        {
            let ids = match shape.get("example_statement_ids") {
                Some(ids) => serde_json::from_value::<Vec<Uuid>>(ids.clone()).map_err(|_| {
                    AppError::invalid("invalid_examples", "Invalid proposal statement identities.")
                })?,
                None => Vec::new(),
            };
            let direction = match shape.get("direction").and_then(Value::as_str) {
                Some("reverse") => "reverse",
                _ => "forward",
            };
            examples.extend(ids.into_iter().map(|statement| (statement, direction)));
        }
    }
    for (statement_id, direction) in examples {
        // A negative case observes disagreement; it must not veto adoption or gain an
        // automatic positive counterpart. Removed examples are skipped too, never replaced.
        let live: Option<Uuid> = sqlx::query_scalar("SELECT f.id FROM facts f WHERE f.kb_id=$1 AND f.id=$2 AND f.layer='open' AND f.invalidated_at IS NULL
            AND NOT EXISTS (SELECT 1 FROM ontology_regression_cases c WHERE c.kb_id=f.kb_id AND c.statement_id=f.id AND c.expected_property_id IS NULL) FOR SHARE OF f")
            .bind(kb_id).bind(statement_id).fetch_optional(&mut **tx).await?;
        if live.is_none() {
            continue;
        }
        utopia_store::ontology_regressions::add_on(
            tx,
            kb_id,
            utopia_store::ontology_regressions::NewCase {
                statement_id,
                expected_property_id: Some(property_id),
                expected_direction: Some(direction),
                created_by: actor,
                origin: "adoption",
            },
        )
        .await?;
    }
    sqlx::query(
        "UPDATE ontology_proposals SET status='adopted',decided_by=$2,decided_at=now(),edited=edited OR $3 WHERE id=$1",
    )
    .bind(locked.id)
    .bind(actor)
    .bind(edited)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Adoption of a map_to: inside the adoption transaction. If the target is a property, each shape in the proposal is written as a person's binding (property + direction),
/// and a projection is queued along with it (same as the review page's `decide_alignment_phrase`); if the target is a class, each category word is put into it, and phrase alignment
/// is queued along with it. Returns the target's id.
///
/// A shape or category word that the person decided differently after the agent read it is not written: one matching the proposal counts as already written (the shape still gets its regression case), and one not matching
/// is kept as it is and recorded in `skipped`. Adoption where nothing at all could be written does not count as adopted
async fn adopt_map_to(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    p: &StoredProposal,
    skipped: &mut Vec<Value>,
) -> Result<Uuid, AppError> {
    let kind = p
        .payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("relation");
    // Written by this call; a person decided it after the agent read it, identically to the proposal, so it counts as already written
    let (mut written, mut agreed) = (0usize, 0usize);
    if kind == "class" {
        let class = lock_classes(tx, kb_id, std::slice::from_ref(&p.key)).await?[0];
        // While the entity is being extracted or aligned, wait only 2 seconds, same as the category-word decision on the review page
        sqlx::query("SET LOCAL lock_timeout = '2s'")
            .execute(&mut **tx)
            .await?;
        let votes = json!({ "person": p.key, "via": "ontology_agent" });
        let seen = p.signatures.get("kind_words_seen_decided_at");
        for w in strings(p.signatures.get("kind_words")) {
            // Lock the existing binding row first and then look: between the look and the write, the person cannot change it. When there is no such row yet, nothing can be locked,
            // and a person's first decision could be overwritten by this write. The class key is read by subquery, not a join: once another transaction commits this row,
            // Postgres recomputes this query with the new version, but a join would still wait on the earlier row (EvalPlanQual),
            // and the class the person just decided would come out empty in the receipt
            let now: Option<Decided> = sqlx::query_as(
                "SELECT b.decided_by, b.decided_at, b.status, b.type_id AS target_id,
                        (SELECT t.key FROM entity_types t WHERE t.id = b.type_id) AS target
                   FROM type_bindings b
                  WHERE b.kb_id = $1 AND b.kind_word = $2 FOR UPDATE OF b",
            )
            .bind(kb_id)
            .bind(type_bindings::normalize(&w))
            .fetch_optional(&mut **tx)
            .await?;
            let read = read_at(seen.and_then(|s| s.get(w.as_str())));
            if let Some(d) = now.filter(|d| d.by_person_after_read(read)) {
                if d.agrees(class, None) {
                    // Not rewritten: the decision is still that person's
                    agreed += 1;
                } else {
                    skipped.push(json!({ "kind_word": w,
                                         "person": { "status": d.status, "class": d.target } }));
                }
                continue;
            }
            if type_bindings::decide_and_apply_human_tx(tx, kb_id, &w, Some(class), &votes).await? {
                written += 1;
            }
        }
        if written + agreed == 0 {
            return Err(nothing_left());
        }
        // The class changed, so the phrase signatures of these entities change with it: phrase alignment is committed with the decision, and the whole adoption queues one; if no word was written
        // nothing is queued, since the person's decision already queued one. It is queued regardless of whether an identical one is already queued, for the same reason as `embed_then_align`:
        // the queued one might be picked up before this commit
        if written > 0 {
            utopia_store::jobs::enqueue_with_max_attempts_tx(
                tx,
                "align_phrases",
                json!({ "kb_id": kb_id }),
                3,
            )
            .await?;
        }
        return Ok(class);
    }
    // The target is re-read in the transaction and locked against deletion (FOR KEY SHARE, same reason as `lock_classes`): until adoption commits it cannot be deleted
    let property: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM relation_types WHERE kb_id = $1 AND key = $2 FOR KEY SHARE",
    )
    .bind(kb_id)
    .bind(&p.key)
    .fetch_optional(&mut **tx)
    .await?;
    let property = property
        .ok_or_else(|| AppError::invalid("unknown_property", "no property with that key"))?;
    let sigs = phrase_bindings::signatures(&mut **tx, kb_id).await?;
    for shape in p
        .signatures
        .get("phrases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        // A shape with no statements left at this moment is skipped. The direction is the shape's own
        let Some(sig) = sigs.iter().find(|s| same_shape(&shape_of(s), shape)) else {
            continue;
        };
        let direction = match shape.get("direction").and_then(Value::as_str) {
            Some("reverse") => "reverse",
            _ => "forward",
        };
        // Lock the existing binding row first, then look; the property key is read by subquery, the same as for category words
        let now: Option<Decided> = sqlx::query_as(
            "SELECT b.decided_by, b.decided_at, b.status, b.relation_type_id AS target_id,
                    (SELECT r.key FROM relation_types r WHERE r.id = b.relation_type_id) AS target,
                    b.direction
               FROM phrase_bindings b
              WHERE b.kb_id = $1 AND b.phrase = $2 AND b.subject_type_id IS NOT DISTINCT FROM $3
                AND b.object_type_id IS NOT DISTINCT FROM $4 AND b.object_is_value = $5
                FOR UPDATE OF b",
        )
        .bind(kb_id)
        .bind(phrase_bindings::normalize(&sig.phrase))
        .bind(sig.subject_type_id)
        .bind(sig.object_type_id.filter(|_| !sig.object_is_value))
        .bind(sig.object_is_value)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(d) =
            now.filter(|d| d.by_person_after_read(read_at(shape.get("seen_decided_at"))))
        {
            if d.agrees(property, Some(direction)) {
                // Not rewritten: the decision is still that person's, and the regression case is still written
                agreed += 1;
            } else {
                // The shape's four cells are the same as the one in the proposal: `finish_adoption` uses them to recognize this entry, and writes no regression case
                skipped.push(json!({
                    "phrase": sig.phrase, "subject": sig.subject_type_key,
                    "object": sig.object_type_key, "value": sig.object_is_value,
                    "person": { "status": d.status, "property": d.target, "direction": d.direction } }));
            }
            continue;
        }
        let votes = json!({ "person": { "property": p.key, "direction": direction, "via": "ontology_agent" } });
        if phrase_bindings::decide_on(
            tx,
            kb_id,
            sig,
            phrase_bindings::Decision {
                relation_type_id: Some(property),
                direction: Some(direction),
                status: "bound",
                votes: &votes,
                decided_by: "person",
                basis: None,
                marks: None,
                marks_asked: false,
            },
        )
        .await?
        {
            written += 1;
        }
    }
    if written + agreed == 0 {
        return Err(nothing_left());
    }
    if written > 0 {
        // The decision is committed together with its projection (0051). One job is queued for the whole commit, not one per shape: all these decisions are in the same
        // commit, and the job reads the current bindings when it runs, so 0051's requirement that "every decision has a job committed with it" still holds
        utopia_store::jobs::enqueue_queued_once_tx(
            tx,
            phrase_bindings::MATERIALIZE_KIND,
            json!({ "kb_id": kb_id }),
            3,
        )
        .await?;
    }
    Ok(property)
}

/// None of the reused shapes or category words could be written: they were all decided by a person differently from the proposal and kept as they are, or no longer have statements. The message
/// does not say the person decided after the agent read it: an old proposal with no recorded read moment keeps a person's earlier decision too
fn nothing_left() -> AppError {
    AppError::CodedConflict {
        code: "proposal_stale",
        message: "Nothing in this proposal is left to write: each of its shapes or kind words is decided differently by a person or has no statements any more."
            .into(),
    }
}

/// Job entry point: raise questions for a KB (decision 1's "a KB without questions is not stuck"). Reached by clicking in the UI, or when the ontology agent finds
/// the KB has no questions at all
pub async fn propose_questions(state: &AppState, kb_id: Uuid) -> anyhow::Result<usize> {
    let pool = &state.pool;
    let kb = utopia_store::kbs::get(pool, kb_id).await?;
    let settings = utopia_store::settings::get(pool, kb.workspace_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot propose questions"))?;
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot propose questions"))?;
    propose_questions_with(state, kb_id, &settings, &client).await
}

#[derive(sqlx::FromRow)]
struct EntityDegree {
    name: String,
    class: Option<String>,
    degree: i64,
}

async fn propose_questions_with(
    state: &AppState,
    kb_id: Uuid,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
) -> anyhow::Result<usize> {
    use utopia_extract::question_agent as qa;
    let pool = &state.pool;
    let classes = utopia_store::graph::entity_types(pool, kb_id).await?;
    let props = utopia_store::ontology::relation_type_views(pool, kb_id).await?;
    let class_key: HashMap<Uuid, &str> = classes.iter().map(|c| (c.id, c.key.as_str())).collect();
    let prop_key: HashMap<Uuid, &str> = props.iter().map(|p| (p.id, p.key.as_str())).collect();
    let keys_of = |ids: &[Uuid]| -> Vec<&str> {
        ids.iter()
            .filter_map(|id| class_key.get(id).copied())
            .collect()
    };
    let glossary = agent::Glossary {
        classes: classes
            .iter()
            .map(|c| (c.key.as_str(), c.label.as_str(), c.description.as_str()))
            .collect(),
        properties: props
            .iter()
            .map(|p| {
                (
                    p.key.as_str(),
                    p.label.as_str(),
                    p.kind.as_str(),
                    keys_of(&p.domains),
                    keys_of(&p.ranges),
                    p.description.as_str(),
                )
            })
            .collect(),
        others: Vec::new(),
    };
    // The most-mentioned shapes, with the property alignment bound them to
    let mut sigs = phrase_bindings::signatures(pool, kb_id).await?;
    sigs.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.phrase.cmp(&b.phrase)));
    sigs.truncate(TOP_SIGNATURES);
    let bindings: HashMap<_, _> = phrase_bindings::bindings(pool, kb_id)
        .await?
        .into_iter()
        .map(|b| (b.key(), b))
        .collect();
    let top_sigs: Vec<qa::TopSignature<'_>> = sigs
        .iter()
        .map(|s| qa::TopSignature {
            phrase: &s.phrase,
            subject_class: s.subject_type_key.as_deref(),
            object_class: s.object_type_key.as_deref(),
            object_is_value: s.object_is_value,
            statement_count: s.count,
            property: bindings
                .get(&s.key())
                .filter(|b| b.status == "bound")
                .and_then(|b| b.relation_type_id)
                .and_then(|id| prop_key.get(&id).copied()),
            example: s.quotes.first().map(String::as_str),
        })
        .collect();
    let mut words = type_bindings::signatures(pool, kb_id).await?;
    words.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.kind_word.cmp(&b.kind_word))
    });
    words.truncate(TOP_KIND_WORDS);
    let top_words: Vec<qa::TopKindWord<'_>> = words
        .iter()
        .map(|w| qa::TopKindWord {
            kind_word: &w.kind_word,
            count: w.count,
        })
        .collect();
    let degrees: Vec<EntityDegree> = sqlx::query_as(
        "SELECT e.canonical_name AS name, t.key AS class, count(f.id) AS degree
           FROM entities e
           LEFT JOIN entity_types t ON t.id = e.type_id
           JOIN facts f ON f.kb_id = e.kb_id AND (f.subject_id = e.id OR f.object_id = e.id)
                        AND f.invalidated_at IS NULL
          WHERE e.kb_id = $1
          GROUP BY e.id, e.canonical_name, t.key
          ORDER BY count(f.id) DESC, e.canonical_name
          LIMIT $2",
    )
    .bind(kb_id)
    .bind(TOP_ENTITIES as i64)
    .fetch_all(pool)
    .await?;
    let top_entities: Vec<qa::TopEntity<'_>> = degrees
        .iter()
        .map(|d| qa::TopEntity {
            name: &d.name,
            class: d.class.as_deref(),
            degree: d.degree,
        })
        .collect();
    let have = competency_questions::list(pool, kb_id).await?;
    let existing: Vec<&str> = have
        .iter()
        .filter(|q| q.status != "rejected")
        .map(|q| q.question.as_str())
        .collect();
    if top_sigs.is_empty() && top_words.is_empty() && top_entities.is_empty() {
        tracing::info!(%kb_id, "提问题：库里还没有东西可问");
        return Ok(0);
    }
    let messages = qa::build_question_messages(
        &top_sigs,
        &top_words,
        &top_entities,
        &glossary,
        &existing,
        QUESTIONS_PER_ROUND,
    );
    let reply = chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0)).await?;
    let class_keys: HashSet<&str> = classes.iter().map(|c| c.key.as_str()).collect();
    let prop_keys: HashSet<&str> = props.iter().map(|p| p.key.as_str()).collect();
    let (proposed, malformed) = qa::parse_question_response(&reply.text, &class_keys, &prop_keys)?;
    let mut written = 0usize;
    for q in proposed.iter().take(QUESTIONS_PER_ROUND) {
        if competency_questions::exists_text(pool, kb_id, &q.question).await? {
            continue;
        }
        let needs = json!({ "classes": q.classes, "properties": q.properties });
        competency_questions::create(
            pool,
            kb_id,
            competency_questions::NewQuestion {
                question: &q.question,
                expected_answer: None,
                needs: Some(&needs),
                origin: "agent",
                status: "proposed",
                created_by: None,
            },
        )
        .await?;
        written += 1;
    }
    tracing::info!(%kb_id, proposed = proposed.len(), written, malformed, "提问题结束");
    if written > 0 {
        state.emit_pending(kb_id);
    }
    Ok(written)
}

#[cfg(test)]
#[path = "ontology_agent_tests.rs"]
mod tests;
