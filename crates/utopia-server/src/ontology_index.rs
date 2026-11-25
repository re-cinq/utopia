//! Vector index for the ontology: embeds the `label + description` of classes, relations and properties as vectors.
//!
//! It serves not one feature but every place that has to ask "which entry in the ontology does this phrase correspond to":
//! ontology proposals (today all 1949 keys are inlined in full, and only the keys, without descriptions), predicate resolution,
//! and type resolution. Only after candidates are retrieved are they handed to the model to decide, which is what decouples the prompt size from the ontology size.
//!
//! **Self-healing, no hooks.** Backfill decides by checking whether "the text that was embedded then and the model name match what is there now",
//! so any write point that changes a description does not need to remember to notify here: a missing hook rots silently,
//! while comparing the original text does not.

use crate::{llm_util, AppState};
use futures_util::{stream, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use utopia_store::ontology::TypeToEmbed;
use uuid::Uuid;

/// Backfill for the same KB runs only one at a time (#282).
///
/// Creating a KB installs three packs, and each queues its own `embed_ontology` job; with worker concurrency 64, the three jobs
/// start at once, read the same batch of 3752 pending entries, and each embeds them: embedding calls triple while the result is identical.
/// So jobs are queued per KB: a later one waits for the earlier one to finish before entering, and on entry checks the pending set again,
/// usually only the few entries that were installed after the earlier one read the set remain. **Wait rather than skip**: skipping would mean
/// entries the earlier one did not see wait for some later backfill at an unknown time.
///
/// The table is not cleaned up: one lock per KB, and the entry count is per KB, so adding a reclaim round for dozens or hundreds of locks is not worth it.
static PER_KB: LazyLock<Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn lock_for(kb_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
    PER_KB
        .lock()
        .expect("per-kb refresh lock table poisoned")
        .entry(kb_id)
        .or_default()
        .clone()
}

/// How many entries to send for embedding at once.
///
/// **64 is the number measured to hold up.** It was once reduced to 16 on the reasoning of "faster per entry", measured on synthetic short text;
/// but real class text averages 151 characters, so that data did not count. After the reduction, the request count
/// quadrupled, each paying a fixed overhead of about 2.5 seconds, and overall throughput dropped by half.
///
/// Before changing the batch size, measure with the **real `embedded_text`**, not with made-up short sentences.
const BATCH: usize = 64;
/// Maximum number of embedding batches in flight at the same time.
///
/// **The point is not speed; it is not to hog the gate.** This is a background backfill job, and the embedding model's concurrency gate
/// (`model_concurrency`, default 10) is shared globally: document chunk ingestion goes through it too.
///
/// It used to be serial, one permit at a time, released between batches, so document processing could slip in. After it was changed to
/// `join_all` with 31 batches at once, it became a standing 10-deep queue, each batch holding
/// a permit for 60 seconds: **five documents all stuck in the `embedding` state, with extraction making no progress for 15 minutes**.
/// The API was not down; this job had simply filled the shared resource.
///
/// Hence the cap here must be **clearly smaller** than the gate, leaving room for other work. Four is the value measured to saturate throughput
/// with margin to spare: four concurrent batches of 16 entries, wall clock 18.5 seconds (serial would take 50 seconds).
const EMBED_JOBS: usize = 4;

/// Fill in the stale ontology vectors in this KB. If no embedding model is configured, return `Ok(0)` directly:
/// retrieval is an enhancement and should not block a deployment that has no model.
pub async fn refresh(state: &AppState, kb_id: Uuid) -> anyhow::Result<usize> {
    refresh_scoped(state, kb_id, None).await
}

/// Backfills only half. Callers who know which half they want use this: type resolution uses only classes,
/// so waiting for relations to finish embedding would be wasted time. The gap-filling half is covered by a background job.
pub async fn refresh_scoped(
    state: &AppState,
    kb_id: Uuid,
    only: Option<utopia_store::ontology::TypeKind>,
) -> anyhow::Result<usize> {
    // Serial within the same KB: see PER_KB. The lock is held in this future until the function returns
    let lock = lock_for(kb_id);
    let _serial = lock.lock().await;
    // The KB may already have been deleted: the job was queued at import time, minutes earlier.
    // This is not a failure, just nothing to do; if treated as an error it would retry three times before giving up
    let Ok(kb) = utopia_store::kbs::get(&state.pool, kb_id).await else {
        return Ok(0);
    };
    let Some(settings) = utopia_store::settings::get(&state.pool, kb.workspace_id).await? else {
        return Ok(0);
    };
    let (Some(client), Some(model)) = (
        llm_util::embed_client(&settings),
        // Take an owned copy: the futures below move settings into an Arc, and borrowing it would no longer be possible
        settings.embed_model.clone(),
    ) else {
        return Ok(0);
    };

    let stale =
        utopia_store::ontology::types_needing_embedding(&state.pool, kb_id, &model, only).await?;
    if stale.is_empty() {
        return Ok(0);
    }
    let mut done = 0usize;
    let mut failed = 0usize;
    // **All owned data**: these futures are driven concurrently across await points, and a borrow cannot satisfy the Send bound
    let client = std::sync::Arc::new(client);
    let model = std::sync::Arc::new(model);
    let settings = std::sync::Arc::new(settings);
    let batches: Vec<Vec<TypeToEmbed>> = stale.chunks(BATCH).map(<[_]>::to_vec).collect();
    let mut jobs = stream::iter(batches.into_iter().map(|batch| {
        let (client, model, settings) = (client.clone(), model.clone(), settings.clone());
        async move {
            let texts: Vec<String> = batch.iter().map(|t| t.text.clone()).collect();
            let _permit = llm_util::acquire_embed(state, &settings).await;
            let vectors = client.embed(&texts).await?;
            // If the counts do not match, abandon the whole batch: pairing is by position, and a misalignment would write person's vector onto
            // organization, and once that is committed the error can never be detected again
            if vectors.len() != batch.len() {
                anyhow::bail!("嵌入返回 {} 条，送去的是 {} 条", vectors.len(), batch.len());
            }
            let n = batch.len();
            let items: Vec<(TypeToEmbed, Vec<f32>)> = batch.into_iter().zip(vectors).collect();
            utopia_store::ontology::set_type_embeddings(&state.pool, &model, &items).await?;
            Ok::<usize, anyhow::Error>(n)
        }
    }))
    .buffer_unordered(EMBED_JOBS);
    // **One failed batch does not sink the rest**: this is a backfill job, and what is missing is healed next round (the criterion is "the
    // original text embedded then still matches", not a timestamp). Bailing the whole thing would waste a rerun of what was already embedded
    while let Some(r) = jobs.next().await {
        match r {
            Ok(n) => done += n,
            Err(e) => {
                failed += 1;
                tracing::warn!(%kb_id, error = %e, "一批本体向量没嵌上，留给下一轮");
            }
        }
    }
    // **The failure count must appear on this line.** Before, it only reported how many were backfilled, so in a run where all 24 batches failed,
    // the log still said "backfilled", and whoever read the log thought all was well
    if failed > 0 {
        tracing::warn!(%kb_id, count = done, failed, "本体向量补了一部分，其余留给下一轮");
        // "Left for the next round" requires a next round to exist: before, this also reported success, no one queued it again, and properties missing vectors stayed missing
        // (alignment's shortlist fell back to all candidates, and the stage timed out). Returning an error lets the job retry within its budget
        anyhow::bail!("{failed} batches of ontology vectors failed; the job retries");
    }
    tracing::info!(%kb_id, count = done, "本体向量已补齐");
    Ok(done)
}

/// For a batch of phrases, which relations/properties in the ontology each phrase most resembles.
///
/// **Embed all of them first, then query the DB one at a time**, rather than sending one embedding request per phrase: an ontology proposal round handles
/// a dozen to several dozen phrases, and sending them one by one would be a dozen to several dozen round trips.
pub async fn nearest_for_each(
    state: &AppState,
    kb_id: Uuid,
    queries: &[String],
    limit: i64,
    target: Target,
) -> anyhow::Result<Vec<Vec<utopia_core::models::TypeCandidate>>> {
    let empty = || vec![Vec::new(); queries.len()];
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    let Some(settings) = utopia_store::settings::get(&state.pool, kb.workspace_id).await? else {
        return Ok(empty());
    };
    let Some(client) = llm_util::embed_client(&settings) else {
        return Ok(empty());
    };
    let mut vectors = Vec::with_capacity(queries.len());
    for batch in queries.chunks(BATCH) {
        let _permit = llm_util::acquire_embed(state, &settings).await;
        let got = client.embed(batch).await?;
        if got.len() != batch.len() {
            anyhow::bail!("嵌入返回 {} 条，送去的是 {} 条", got.len(), batch.len());
        }
        vectors.extend(got);
    }
    let mut out = Vec::with_capacity(vectors.len());
    for v in &vectors {
        out.push(match target {
            Target::Class => {
                utopia_store::ontology::nearest_entity_types(&state.pool, kb_id, v, limit, false)
                    .await?
            }
            Target::ClassLabel => {
                utopia_store::ontology::nearest_entity_types(&state.pool, kb_id, v, limit, true)
                    .await?
            }
            Target::Predicate(kind) => {
                utopia_store::ontology::nearest_relation_types(&state.pool, kb_id, v, limit, kind)
                    .await?
            }
        });
    }
    Ok(out)
}

/// Which half of the ontology the search covers.
///
/// **Must be kept apart.** Classes go in `entity_types`, relations and properties in `relation_types`, and the descriptions of the two
/// describe completely different things ("what kind of entity belongs here" vs "what this assertion says").
/// Searching the relations with a class name that is outside the vocabulary returns relations that are only loosely related, guaranteed.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    /// Full-text index (label + description). Long profiles go this way
    Class,
    /// **Label-only index** (see `entity_types.label_embedding`). Short phrases (`district. place`) go this way.
    /// Short phrases get taken over by a tautological class more readily than by the full index: the description of the `Map` entry is just
    /// "A map.", winning on length rather than meaning
    ClassLabel,
    /// `Some("attribute")` finds only properties, `Some("relation")` only relations, `None` finds both.
    /// A fact with a literal object wants a property; one with an entity object wants a relation
    Predicate(Option<&'static str>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queued backfills for the same KB are serialized; different KBs are independent. This guards the #282 rule that the three jobs
    /// queued by three packs must not read the same batch of pending entries at once
    #[tokio::test]
    async fn refreshes_of_one_base_take_turns() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let first = lock_for(a);
        let held = first.lock().await;
        // Same KB: the second one does not get the lock
        assert!(
            lock_for(a).try_lock().is_err(),
            "同一个库的第二次补齐应当排队"
        );
        // Other KBs: unaffected
        assert!(lock_for(b).try_lock().is_ok(), "别的库不该被这把锁挡住");
        drop(held);
        assert!(lock_for(a).try_lock().is_ok(), "前一个放开后下一个才进");
    }
}
