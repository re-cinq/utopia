//! Binding category words to classes (first slice of 0044 decisions 3-4: binding by signature, where signature = the entity's category word).
//!
//! Open extraction only records the document's own category words (`entities.specific_type`: company, person, "metric"),
//! and does not pick classes. Classes are the ontology's business; binding is the alignment's: a KB has dozens or hundreds of distinct category words, each
//! judged only once: example sentences, the relation phrases it takes part in, and the definitions of candidate classes go to the model together. A binding is made only when **both votes agree**; if they disagree,
//! it is recorded as undecided for review (#725 queue 2); a word with no matching class is recorded as none and proposed as a new class by the old flow
//! ("suggest adding a class", `proposed_type`, adopted from the ontology page). Bindings are stored in `type_bindings`, along with the fingerprint of the candidate classes
//! shown to the model when it decided (`basis`: each one's `updated_at` and its ancestor closure, the category-word half of 0053).
//! Expired = the fingerprint recomputed from the current classes does not match, so when the ontology changes only the mismatched ones are re-judged. Previously this was judged by timestamp, which could not see
//! two things: a definition changed while the model was answering (the decision is written after the answer, and the timestamp says the decision is newer, #795), and
//! parent-edge additions and removals, which do not touch `updated_at`. A bound class is written to each entity's `type_id` under that category word (`type_source =
//! 'aligned'`; classes a person set are left alone), and identity resolution's class-scoped range is restored along with it.
//!
//! Inputs are all read from one snapshot before the model is called; when writing the decision, the fingerprint is recomputed in the same transaction against the current classes, and if it does not match
//! the reply is not accepted and another round is queued (`type_bindings::decide_and_apply_if_current`).
//!
//! Where candidate classes come from: if the KB has an embedding model, retrieve the nearest classes by category word plus example names; if not, give the whole table when there are few classes;
//! when there are too many classes and no embeddings, do not judge at all: a bad guess is worse than no guess.

use crate::extraction::chat_retrying_rate_limits_at;
use crate::llm_util;
use crate::ontology_index::{self, Target};
use crate::state::AppState;
use std::collections::{HashMap, HashSet};
use utopia_core::models::EntityType;
use utopia_extract::align::{
    build_kind_word_messages, parse_kind_word_response, ClassCandidate, KindWordItem,
};
use utopia_store::type_bindings::{self, Acceptance, Binding, KindWordSignature};
use uuid::Uuid;

/// How many category words to ask about at once.
const BATCH: usize = 20;
/// How many candidate classes to give each category word.
const CANDIDATES: i64 = 10;
/// With no embedding model, the whole table is given when there are no more classes than this.
const WHOLE_LIST_LIMIT: usize = 60;

/// One vote: which key this category word picked (None = no class matches).
type Vote = Option<String>;

/// Find candidate classes for a batch of category words: retrieve by embedding if available, otherwise the whole table (when there are few classes). Returns each signature's candidate id list.
///
/// On a retrieval error, fall back to the whole table, same as for phrases: candidates are part of the judgment's fingerprint, and a word judged in an errored round
/// has a mismatched fingerprint once recovered, so it is asked again next round. Without the fallback, while the embedding endpoint stayed broken, newly arrived words would never get a class
///
/// The exception is an exhausted balance: do not fall back to the whole table; return this error as-is, so the round fails once and raises an alert, rather than continuing to ask and queue.
async fn candidates_for(
    state: &AppState,
    kb_id: Uuid,
    sigs: &[&KindWordSignature],
    classes: &[EntityType],
) -> anyhow::Result<Vec<Vec<Uuid>>> {
    let queries: Vec<String> = sigs
        .iter()
        .map(|s| format!("{}: {}", s.kind_word, s.examples.join(", ")))
        .collect();
    let nearest = match ontology_index::nearest_for_each(
        state,
        kb_id,
        &queries,
        CANDIDATES,
        Target::ClassLabel,
    )
    .await
    {
        Ok(nearest) => nearest,
        Err(e) if utopia_llm::out_of_credit(&e).is_some() => return Err(e),
        Err(e) => {
            tracing::warn!(%kb_id, error = %e, "类别词对齐检索候选类失败，退回整表");
            Vec::new()
        }
    };
    let mut out = Vec::with_capacity(sigs.len());
    for (i, _) in sigs.iter().enumerate() {
        let found: Vec<Uuid> = nearest
            .get(i)
            .map(|v| v.iter().map(|c| c.id).collect())
            .unwrap_or_default();
        if !found.is_empty() {
            out.push(found);
        } else if classes.len() <= WHOLE_LIST_LIMIT {
            out.push(classes.iter().map(|c| c.id).collect());
        } else {
            out.push(Vec::new());
        }
    }
    Ok(out)
}

/// Whether two votes agree: both pick the same key, or both say none.
fn agree(a: &Vote, b: &Vote) -> bool {
    a == b
}

/// A snippet of the reply that fits in a log line: whitespace collapsed to one space, at most this many characters.
const SNIPPET_CHARS: usize = 240;

fn snippet(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(SNIPPET_CHARS).collect();
    if flat.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// Items in a round that were not decided (call failed, reply unreadable, model omitted them) are re-queued by the job itself up to this many times; past this count it waits
/// for the next document or ontology change before asking again. Without a cap, a reply that is unreadable every time at temperature zero would have
/// the job resend the same prompt every few dozen seconds, with no end
const MAX_REASK: u32 = 3;

/// Runs once over a KB: category words that are new and those that have expired are each judged once; bound ones get their class written, and unbound ones are proposed as suggestions.
/// `reask` is how many times this job has already queued itself (documents and class creation queue at 0).
pub async fn align_types_reasking(state: &AppState, kb_id: Uuid, reask: u32) -> anyhow::Result<()> {
    let pool = &state.pool;
    let kb = utopia_store::kbs::get(pool, kb_id).await?;
    let settings = utopia_store::settings::get(pool, kb.workspace_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot align kind words"))?;
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot align kind words"))?;
    // Ontology vectors are still being backfilled: the editor, on creating a class, queues `embed_ontology` before this job, and candidate classes are retrieved by vector
    // (`candidates_for`); a class without a vector cannot be retrieved, so judging now would leave it out of the candidates. Come back once it finishes —
    // queue one attempt half a minute later, at most one pending. Phrase alignment waits on this job, so the order is vectors → category words → phrases
    if utopia_store::jobs::pending_for_kb(pool, "embed_ontology", kb_id).await? {
        tracing::info!(%kb_id, "类别词对齐：本体向量还在补，半分钟后再看");
        let payload = if reask == 0 {
            serde_json::json!({ "kb_id": kb_id })
        } else {
            serde_json::json!({ "kb_id": kb_id, "reask": reask })
        };
        utopia_store::jobs::enqueue_unless_queued_after(
            pool,
            "align_types",
            payload,
            std::time::Duration::from_secs(30),
        )
        .await?;
        return Ok(());
    }
    // Only one run per KB at a time: it is queued every time a document is extracted or a class is created, and queue dedup only blocks "already queued",
    // so if a run starts while the previous one is still running, they run in parallel. Measured: seeding 14 classes produced 14 parallel jobs, which
    // hammered the model endpoint into 502s. A run that cannot get the lock exits; the one running will see the same batch of words; anything not judged this round comes up next round
    let mut guard = pool.acquire().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext('align_types'), hashtext($1))")
            .bind(kb_id.to_string())
            .fetch_one(&mut *guard)
            .await?;
    if !locked {
        // The run in progress checks for new items itself when it ends (see the end of align_types_locked); no new run is queued here
        tracing::info!(%kb_id, "类别词对齐已有一份在跑，这次跳过");
        return Ok(());
    }
    let result = align_types_locked(state, kb_id, reask, &settings, &client).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext('align_types'), hashtext($1))")
        .bind(kb_id.to_string())
        .execute(&mut *guard)
        .await;
    result
}

async fn align_types_locked(
    state: &AppState,
    kb_id: Uuid,
    reask: u32,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    // First backfill bound classes onto entities under their category word that still have no class: the projection taken when a decision lands only sees the entities of that moment,
    // and entities extracted afterward (the few documents still being extracted during the decision) land with no class; this refills them once per round,
    // so phrase alignment, queued after this, sees classes on both ends of the signature
    let caught_up = type_bindings::apply_bound(pool, kb_id).await?;
    if caught_up > 0 {
        tracing::info!(%kb_id, entities = caught_up, "已绑上的类补到了后建的实体上");
    }
    // Read all inputs from one snapshot before calling the model (#795): class definitions, class versions and parent edges, category words, and existing
    // decisions. If read in several passes, an edit committed between two reads would leave the definition in the prompt and the version in the fingerprint out of step
    let (classes, snapshot, sigs, existing) = {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        let classes = utopia_store::graph::entity_types(&mut *tx, kb_id).await?;
        let snapshot = type_bindings::class_snapshot(&mut *tx, kb_id).await?;
        let sigs = type_bindings::signatures(&mut *tx, kb_id).await?;
        let existing: HashMap<String, Binding> = type_bindings::bindings(&mut *tx, kb_id)
            .await?
            .into_iter()
            .map(|b| (b.kind_word.clone(), b))
            .collect();
        tx.commit().await?;
        (classes, snapshot, sigs, existing)
    };
    let by_id: HashMap<Uuid, &EntityType> = classes.iter().map(|c| (c.id, c)).collect();
    let by_key: HashMap<&str, &EntityType> = classes.iter().map(|c| (c.key.as_str(), c)).collect();
    // Candidates and fingerprints each live category word is shown to the model right now. Retrieval runs once per round: at the start it decides whom to judge,
    // and at the end the same candidates are used to recompute the fingerprint against freshly read classes, to see whether anything expired while it was running
    let all: Vec<&KindWordSignature> = sigs.iter().collect();
    let considered: HashMap<String, (Vec<Uuid>, String)> = sigs
        .iter()
        .zip(candidates_for(state, kb_id, &all, &classes).await?)
        .map(|(s, c)| {
            let basis = snapshot.basis(&c);
            (s.kind_word.clone(), (c, basis))
        })
        .collect();
    // Expired = the stored fingerprint differs from the current one (rows with no fingerprint were decided before this column existed; each is judged again). Human decisions are not re-judged
    let todo: Vec<&KindWordSignature> = sigs
        .iter()
        .filter(|s| match existing.get(&s.kind_word) {
            None => true,
            Some(b) => {
                b.decided_by != "person"
                    && b.basis.as_deref() != Some(considered[&s.kind_word].1.as_str())
            }
        })
        .collect();
    let attempted: HashSet<String> = todo.iter().map(|s| s.kind_word.clone()).collect();
    // Written in this round, or already decided by a person and not overwritten by the agent. Asked but not concluded does not count; see the wrap-up
    let mut settled: HashSet<String> = HashSet::new();
    tracing::info!(%kb_id, kind_words = sigs.len(), to_decide = todo.len(), classes = classes.len(), "类别词对齐开始");

    // No class to bind to: every word is "none", and proposed as suggestions; once classes appear the candidates change and the fingerprint mismatches, so they come back again
    if classes.is_empty() {
        for s in &todo {
            let (candidates, basis) = &considered[&s.kind_word];
            let accepted = type_bindings::decide_and_apply_if_current(
                pool,
                kb_id,
                &s.kind_word,
                &s.words,
                None,
                "none",
                &serde_json::json!({ "reason": "no classes" }),
                basis,
                candidates,
            )
            .await?;
            if accepted == Acceptance::Written {
                type_bindings::propose(
                    pool,
                    kb_id,
                    &s.kind_word,
                    s.words.first().unwrap_or(&s.kind_word),
                )
                .await?;
            }
        }
        return Ok(());
    }

    let (mut bound, mut none, mut undecided, mut skipped) = (0usize, 0usize, 0usize, 0usize);
    // Batches that failed on call or parse: skipped this round, and queued again by the job itself at the end, not waiting for the next document to queue it
    let mut failed = 0usize;
    // Words that were asked, and the model did answer, but did not answer both votes: with one vote missing, no conclusion is drawn
    let mut unanswered = 0usize;
    // Words whose candidate classes changed while the model was answering, so the reply was not accepted: they answered the old input, and at wrap-up another round is queued to ask again with the new one
    let mut moved = 0usize;
    for batch in todo.chunks(BATCH) {
        let cands: Vec<&[Uuid]> = batch
            .iter()
            .map(|s| considered[&s.kind_word].0.as_slice())
            .collect();
        // Two votes: the second one is given the candidates in reverse order, to keep an order preference like "pick the first" from passing as agreement
        let mut votes: Vec<(Vote, Vote)> = vec![(None, None); batch.len()];
        let mut answered = vec![(false, false); batch.len()];
        for pass in 0..2 {
            let items: Vec<KindWordItem<'_>> = batch
                .iter()
                .enumerate()
                .filter(|(i, _)| !cands[*i].is_empty())
                .map(|(i, s)| {
                    let mut ids: Vec<Uuid> = cands[i].to_vec();
                    if pass == 1 {
                        ids.reverse();
                    }
                    KindWordItem {
                        id: i as i64,
                        kind_word: &s.kind_word,
                        spellings: &s.words,
                        examples: &s.examples,
                        phrases: &s.phrases,
                        candidates: ids
                            .iter()
                            .filter_map(|id| by_id.get(id))
                            .map(|c| ClassCandidate {
                                key: &c.key,
                                label: &c.label,
                                description: &c.description,
                            })
                            .collect(),
                    }
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            let messages = build_kind_word_messages(&items);
            let reply =
                match chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0))
                    .await
                {
                    Ok(r) => r,
                    // An overdue balance fails the whole round once and raises the alert; it is not recorded as failed and queued again (same as phrase alignment, #1360)
                    Err(e) if utopia_llm::out_of_credit(&e).is_some() => return Err(e),
                    Err(e) => {
                        tracing::warn!(%kb_id, error = %e, "类别词对齐调用失败，这一批留到下次");
                        failed += 1;
                        continue;
                    }
                };
            let (choices, malformed) = match parse_kind_word_response(&reply.text, &items) {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(%kb_id, error = %e, "类别词对齐回复解析失败，这一批留到下次");
                    failed += 1;
                    continue;
                }
            };
            skipped += malformed;
            if choices.is_empty() {
                // Parsed out, but not one pair was read: the reply's shape is not one we recognize. This is the same as a parse failure, so it counts as failed and is left for next time.
                // Before, nothing was said here, and a KB went through a dozen-plus rounds without binding a single word; the log held only a string of "done bound=0".
                // The start of the reply must go into the log, so next time it is known what it looks like
                tracing::warn!(
                    %kb_id,
                    pass,
                    items = items.len(),
                    malformed,
                    finish_reason = ?reply.finish_reason,
                    repeating = reply.repeating,
                    chars = reply.text.chars().count(),
                    reply = %snippet(&reply.text),
                    "类别词对齐回复读不出一项，这一批留到下次"
                );
                failed += 1;
                continue;
            }
            for c in choices {
                let Ok(i) = usize::try_from(c.id) else {
                    continue;
                };
                if let Some(slot) = votes.get_mut(i) {
                    if pass == 0 {
                        slot.0 = c.key;
                        answered[i].0 = true;
                    } else {
                        slot.1 = c.key;
                        answered[i].1 = true;
                    }
                }
            }
        }
        for (i, s) in batch.iter().enumerate() {
            if cands[i].is_empty() {
                skipped += 1;
                continue;
            }
            let (a, b) = &votes[i];
            let (ans_a, ans_b) = answered[i];
            if !ans_a || !ans_b {
                // One vote unanswered: draw no conclusion, ask again next time
                unanswered += 1;
                continue;
            }
            let record = serde_json::json!({ "first": a, "second": b });
            let (type_id, status) = if !agree(a, b) {
                (None, "undecided")
            } else {
                match a.as_deref().and_then(|k| by_key.get(k)) {
                    Some(class) => (Some(class.id), "bound"),
                    None => (None, "none"),
                }
            };
            let (candidates, basis) = &considered[&s.kind_word];
            match type_bindings::decide_and_apply_if_current(
                pool,
                kb_id,
                &s.kind_word,
                &s.words,
                type_id,
                status,
                &record,
                basis,
                candidates,
            )
            .await?
            {
                Acceptance::Written => {
                    settled.insert(s.kind_word.clone());
                    match status {
                        "bound" => bound += 1,
                        "undecided" => undecided += 1,
                        _ => {
                            type_bindings::propose(
                                pool,
                                kb_id,
                                &s.kind_word,
                                s.words.first().unwrap_or(&s.kind_word),
                            )
                            .await?;
                            none += 1;
                        }
                    }
                }
                Acceptance::KeptPerson => {
                    settled.insert(s.kind_word.clone());
                }
                Acceptance::Moved => {
                    tracing::info!(%kb_id, kind_word = %s.kind_word, "类别词对齐：模型答题期间候选类变了，这份回复不收");
                    moved += 1;
                }
            }
        }
    }
    tracing::info!(%kb_id, bound, none, undecided, skipped, failed, unanswered, moved, "类别词对齐完成");
    if unanswered > 0 {
        tracing::warn!(%kb_id, unanswered, "类别词对齐有词模型没答到，这些词这轮没有结论");
    }
    if bound + none + undecided > 0 {
        state.emit_graph(kb_id);
    }
    // Same as phrase alignment: if new words that were never tried arrive, or words judged this round have expired, queue another round (built from scratch,
    // the new words get a new prompt). "Expired" is not limited to what was judged this round: classes changed while it was running must also make the old bindings be judged again. The comparison
    // is against the fingerprint computed from the **re-read** classes (candidates reuse the set retrieved when the run started). Words asked this round without reaching a conclusion
    // are not counted here: call failure, unreadable, omitted, or no candidates go through the capped retry below; if counted in,
    // a permanently erroring endpoint would have the job re-queue round after round at once
    let changed = moved > 0 || {
        let now = type_bindings::class_snapshot(pool, kb_id).await?;
        let decided: HashMap<String, Binding> = type_bindings::bindings(pool, kb_id)
            .await?
            .into_iter()
            .map(|b| (b.kind_word.clone(), b))
            .collect();
        type_bindings::signatures(pool, kb_id)
            .await?
            .iter()
            .any(|s| match decided.get(&s.kind_word) {
                None => !attempted.contains(&s.kind_word),
                Some(b) => {
                    b.decided_by != "person"
                        && (settled.contains(&s.kind_word) || !attempted.contains(&s.kind_word))
                        && considered.get(&s.kind_word).is_some_and(|(candidates, _)| {
                            b.basis.as_deref() != Some(now.basis(candidates).as_str())
                        })
                }
            })
    };
    finish(state, kb_id, reask, changed, failed, unanswered).await
}

/// Wrap-up of a round: if there is new work, queue another round at once; those not yet judged are asked again after a delay, with a capped number of times; queue
/// phrase alignment only when it does not queue itself.
async fn finish(
    state: &AppState,
    kb_id: Uuid,
    reask: u32,
    changed: bool,
    failed: usize,
    unanswered: usize,
) -> anyhow::Result<()> {
    let pool = &state.pool;
    // Whatever was not judged this round (call failed, reply unreadable, model omitted some ids) is queued again by itself, at most MAX_REASK times,
    // waiting a bit longer each time. Before, only failed batches were re-queued, and omitted words could only wait for the next document to queue them, so after the last
    // document there was no next one, and they never reached a conclusion; an omission writes no row, so the ontology page could not see it either
    let unfinished = failed > 0 || unanswered > 0;
    // If there is another round (new words arrived, something has expired, or undecided ones must be asked again), do not queue phrase alignment yet: the two ends' classes are still changing,
    // and a phrase signature judged now would have to be judged again once they change. In one 100-document run, 1010 phrase signatures judged in round one
    // included 713 re-judged because a category word changed class in round two. Only the last round (when it does not queue itself) queues it; and the branch where
    // the re-ask count is exhausted also queues it: the decisions will not change any more
    let mut another_round = false;
    if changed {
        utopia_store::jobs::enqueue_unless_queued(
            pool,
            "align_types",
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
        another_round = true;
    } else if unfinished && reask < MAX_REASK {
        let delay = std::time::Duration::from_secs(20 * u64::from(reask + 1));
        tracing::info!(%kb_id, failed, unanswered, reask = reask + 1, delay_secs = delay.as_secs(), "类别词对齐没判完，稍后再问");
        utopia_store::jobs::enqueue_unless_pending(
            pool,
            "align_types",
            serde_json::json!({ "kb_id": kb_id, "reask": reask + 1 }),
            delay,
        )
        .await?;
        another_round = true;
    } else if unfinished {
        tracing::warn!(%kb_id, failed, unanswered, reask, "类别词对齐问了几轮仍没判完，等下一篇文档或本体改动再问");
    }
    // Phrase signatures are only fixed once the classes at both ends are fixed: phrase alignment is queued after category-word alignment **finishes**
    if !another_round {
        utopia_store::jobs::enqueue_unless_queued(
            pool,
            "align_phrases",
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "type_alignment_tests.rs"]
mod tests;
