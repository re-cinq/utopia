//! Governance task (0025): when the switch is on, go through the waiting duplicate pairs first-in-first-out, reading the ledger before ruling.
//!
//! A pair a person has already ruled on (the same pair of entities, or the same pair of names with different spellings) follows the person's ruling and does not ask the model.
//! The remaining two layers. Layer one is batching: the head of the queue goes in one model call together with its cluster, and precedents are already in the prompt. Pairs that pass the gate
//! act on their own (merges can be undone, splits can be re-merged). Layer two (the second cut) takes only the pairs that layer one **could not decide**:
//! each one is looked at again with tools: all facts on one side, the source snippets, the person's decisions on this name in the ledger,
//! and other entities with the same name. After looking, it either decides, or defers, leaving a concrete question for a person. Pairs blocked by hard rules
//! (type conflicts, a retraction, or a person's decision on this pair going the other way) do not enter layer two: looking again will not change the rule.
//!
//! Between two clusters the switch is checked; if it is off, the task stops right there. "The queue terminates automatically once switched off" is exactly that line. When the model
//! is absent, the task ends successfully and changes nothing: a ruling with no basis is worse than none, and the queue simply waits.
//!
//! Division of labour with the batch adjudicator (`adjudication`): with the switch off, that tier still judges grey-zone pairs alone;
//! with it on, the queue that extraction schedules goes here, and grey-zone pairs and pairs awaiting a person all flow through this queue. This neither reads nor writes
//! the adjudication cache: the cache key has no precedents, while the answers here change with precedents.

use crate::llm_util;
use crate::state::AppState;
use chrono::{Duration, Utc};
use futures_util::future::join_all;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use utopia_core::models::{LlmSettings, ReviewItem, Role};
use utopia_core::AppError;
use utopia_extract::governor::{self, Step};
use utopia_llm::{tool_result_message, LlmClient};
use utopia_store::alerts;
use utopia_store::execution_gate;
use utopia_store::governance::{self as gov, Gate, NewDecision, Precedents, AUTO_CONF};
use uuid::Uuid;

/// How many pairs one model call carries: the head of the queue with its cluster
const BATCH_SIZE: i64 = 12;
/// A task runs at most this many rounds, after which another is queued to continue, so tasks for other libraries get a turn too
const MAX_ROUNDS: usize = 20;
/// Per library per day, layer two may spend at most this many model calls. When used up, pairs it could not decide are written as suggestions, as in the first cut.
/// 300 was used up in the first round of a 229-chunk import (132 pairs took 301 calls), so it is 2000
const LOOP_DAILY_CALLS: i64 = 2000;
/// Max characters of a tool result given to the model: source snippets and fact lists can both be long
const TOOL_OUTPUT_CHARS: usize = 3000;

/// Things carried everywhere during one round of the task
struct Ctx<'a> {
    state: &'a AppState,
    kb_id: Uuid,
    run_id: Uuid,
    client: &'a LlmClient,
    settings: &'a Option<LlmSettings>,
}

/// One view of a pair: from layer one, or revised after layer two has looked. The adjudicator (when governance is off) uses it too:
/// pairs it cannot decide go through the same layer two (0028)
pub(crate) struct Look {
    pub(crate) same: Option<bool>,
    pub(crate) conf: f32,
    pub(crate) why: Option<String>,
    /// The question left by a defer
    pub(crate) question: Option<String>,
    /// What layer two looked at
    pub(crate) trace: Vec<Value>,
    /// The model calls layer two spent
    pub(crate) calls: i32,
}

impl Look {
    pub(crate) fn from_batch(same: Option<bool>, conf: f32, why: Option<String>) -> Self {
        Look {
            same,
            conf,
            why,
            question: None,
            trace: Vec::new(),
            calls: 0,
        }
    }

    /// A person has ruled on this pair: follow the person's ruling, do not ask the model
    fn from_people(merged: bool) -> Self {
        Look {
            same: Some(merged),
            conf: 1.0,
            why: Some("a person decided this same pair before".into()),
            question: None,
            trace: Vec::new(),
            calls: 0,
        }
    }

    /// Layer one did not decide: no verdict, or confidence below the threshold
    fn uncertain(&self) -> bool {
        self.same.is_none() || self.conf < AUTO_CONF
    }

    fn action(&self) -> &'static str {
        match self.same {
            Some(true) => "merge",
            Some(false) => "keep",
            None => "unsure",
        }
    }
}

pub async fn govern(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    if !kb.governance {
        return Ok(());
    }
    let settings = utopia_store::settings::get(&state.pool, kb.workspace_id).await?;
    let Some(client) = settings.as_ref().and_then(llm_util::chat_client) else {
        tracing::info!(%kb_id, "治理：没有配聊天模型，队列原地等");
        return Ok(());
    };
    // One governance task at a time per library. One is queued after each document is extracted, and queue dedup only blocks pairs already queued, not ones being processed:
    // failing to get the lock means someone is already governing this library; it will drain the queue, and if backlog remains after that, it queues another. It cannot see pairs that arrive after it read the head of the queue,
    // so here it queues another one a minute later: at most one queued, and only one runs
    let Some(base_lock) = gov::try_lock_base(&state.pool, kb_id).await? else {
        tracing::info!(%kb_id, "治理：这个库已有任务在跑，一分钟后再看一眼");
        utopia_store::jobs::enqueue_unless_queued_after(
            &state.pool,
            "govern",
            json!({ "kb_id": kb_id }),
            std::time::Duration::from_secs(60),
        )
        .await?;
        return Ok(());
    };
    let ctx = Ctx {
        state,
        kb_id,
        run_id: Uuid::now_v7(),
        client: &client,
        settings: &settings,
    };

    // Release a lock left by an earlier task that stopped midway; release again after the run finishes (however it ended)
    gov::release_locks(&state.pool, kb_id).await?;
    let outcome = rounds(&ctx).await;
    if let Err(e) = gov::release_locks(&state.pool, kb_id).await {
        tracing::warn!(%kb_id, error = %e, "治理：放锁失败");
    }
    base_lock.release().await;
    state.emit_review(kb_id);
    let more = outcome?;

    // Rounds used up with backlog left: queue another, and the next round picks up from the head
    if more {
        utopia_store::jobs::enqueue_unless_queued(&state.pool, "govern", json!({ "kb_id": kb_id }))
            .await?;
    }
    Ok(())
}

/// How many clusters are ruled on in parallel in one round. Only clusters that share neither entities nor names can be in the same round: a merge inside one cluster
/// would change what another cluster sees. Concurrency toward the model is limited by llm_util's gate; here we decide only how many are dispatched at once
const CLUSTERS_PER_ROUND: usize = 3;
/// How many pairs to look at from the head of the queue when picking clusters
const HEADS_TO_SCAN: i64 = 60;

/// A cluster: pairs awaiting a person, each one's precedents, how it appears to the model, and the views already held (the fast path a person set)
struct Cluster {
    items: Vec<ReviewItem>,
    precedents: Vec<Precedents>,
    pairs: Vec<utopia_extract::AdjudicationPair>,
    looks: Vec<Option<Look>>,
}

/// Round after round until the queue is empty, the switch is off, or the round count is used up. Returns true = round count used up with backlog left
async fn rounds(ctx: &Ctx<'_>) -> anyhow::Result<bool> {
    let state = ctx.state;
    let kb_id = ctx.kb_id;
    let pool = &state.pool;
    for _ in 0..MAX_ROUNDS {
        // The queue terminates automatically once switched off: check before each round
        if !utopia_store::kbs::get(pool, kb_id).await?.governance {
            tracing::info!(%kb_id, "治理：开关已关，停在这里");
            return Ok(false);
        }

        // Pick non-overlapping clusters from the head, at most K; a head that intersects a cluster already taken is left for the next round
        let heads = gov::queue(pool, kb_id, HEADS_TO_SCAN).await?;
        if heads.is_empty() {
            return Ok(false);
        }
        let mut clusters: Vec<Vec<ReviewItem>> = Vec::new();
        let mut taken_ids: HashSet<Uuid> = HashSet::new();
        let mut taken_entities: HashSet<Uuid> = HashSet::new();
        let mut taken_names: HashSet<String> = HashSet::new();
        for head in heads {
            if clusters.len() >= CLUSTERS_PER_ROUND {
                break;
            }
            if taken_ids.contains(&head.id) {
                continue;
            }
            let mut items = vec![head];
            let siblings = gov::cluster_of(pool, kb_id, &items[0], BATCH_SIZE - 1).await?;
            items.extend(siblings);
            let overlaps = items.iter().any(|i| {
                taken_ids.contains(&i.id)
                    || taken_entities.contains(&i.left.id)
                    || taken_entities.contains(&i.right.id)
                    || taken_names.contains(&i.left.name.to_lowercase())
                    || taken_names.contains(&i.right.name.to_lowercase())
            });
            if overlaps {
                continue;
            }
            for i in &items {
                taken_ids.insert(i.id);
                taken_entities.insert(i.left.id);
                taken_entities.insert(i.right.id);
                taken_names.insert(i.left.name.to_lowercase());
                taken_names.insert(i.right.name.to_lowercase());
            }
            clusters.push(items);
        }
        // These clusters now belong to the agent: a person cannot rule on them for these few minutes, and the UI shows this
        let ids: Vec<Uuid> = taken_ids.into_iter().collect();
        gov::lock(pool, kb_id, &ids).await?;
        state.emit_review(kb_id);

        // Precedents and fast path (library operations, done in order)
        let mut round: Vec<Cluster> = Vec::with_capacity(clusters.len());
        for items in clusters {
            let mut precedents = Vec::with_capacity(items.len());
            for item in &items {
                precedents.push(gov::precedents_for(pool, kb_id, item).await?);
            }
            let pairs: Vec<utopia_extract::AdjudicationPair> = items
                .iter()
                .zip(&precedents)
                .map(|(item, p)| pair_of(item, p))
                .collect();
            let mut looks = Vec::with_capacity(items.len());
            for (idx, item) in items.iter().enumerate() {
                let prior = gov::decided_before(pool, kb_id, item.left.id, item.right.id).await?;
                looks.push(
                    gov::settled_by_people(
                        &item.left.name,
                        &item.right.name,
                        &precedents[idx],
                        prior,
                    )
                    .map(Look::from_people),
                );
            }
            round.push(Cluster {
                items,
                precedents,
                pairs,
                looks,
            });
        }

        // Batch: one call per cluster, several clusters dispatched at once. Any single failure fails the whole round, and the task retries with backoff;
        // after retries are exhausted these pairs stay in the queue, and a person can still rule on them
        let batches = join_all(round.iter().map(|c| batch_looks(ctx, c))).await;
        for (c, looks) in round.iter_mut().zip(batches) {
            for (idx, look) in looks? {
                c.looks[idx] = Some(look);
            }
        }

        // Layer two: pairs that could not be decided, and those with the same name but differing verdicts, are looked at again with tools, several pairs at once,
        // with concurrency on the model side limited by the gate
        let mut second: Vec<(usize, usize)> = Vec::new();
        for (ci, c) in round.iter().enumerate() {
            for (idx, look) in c.looks.iter().enumerate() {
                if let Some(look) = look {
                    if wants_second_look(&c.items[idx], &c.precedents[idx], look) {
                        second.push((ci, idx));
                    }
                }
            }
        }
        let seen = join_all(second.iter().map(|&(ci, idx)| {
            let c = &round[ci];
            second_look(
                ctx,
                &c.items[idx],
                &c.pairs[idx],
                c.looks[idx].as_ref().expect("a look"),
            )
        }))
        .await;
        for (&(ci, idx), look) in second.iter().zip(seen) {
            if let Some(look) = look {
                round[ci].looks[idx] = Some(look);
            }
        }

        // Apply: write to the store per cluster and per pair, in order; chained merges must go in order
        for c in round.iter_mut() {
            for idx in 0..c.items.len() {
                let look = c.looks[idx]
                    .take()
                    .unwrap_or_else(|| Look::from_batch(None, 0.0, None));
                apply(ctx, &c.items[idx], &c.precedents[idx], look).await?;
            }
        }
        state.emit_review(kb_id);
    }
    Ok(!gov::queue(pool, kb_id, 1).await?.is_empty())
}

/// The batch call for one cluster: pairs a person has ruled on are not asked; the rest are asked in one go; returns each pair's view
async fn batch_looks(ctx: &Ctx<'_>, c: &Cluster) -> anyhow::Result<Vec<(usize, Look)>> {
    let asked: Vec<usize> = (0..c.items.len())
        .filter(|&i| c.looks[i].is_none())
        .collect();
    if asked.is_empty() {
        return Ok(Vec::new());
    }
    let batch: Vec<utopia_extract::AdjudicationPair> =
        asked.iter().map(|&i| c.pairs[i].clone()).collect();
    let messages = utopia_extract::build_adjudication_messages(&batch);
    let reply = {
        let _permit = permit(ctx).await;
        ctx.client.chat(&messages).await?
    };
    let (verdicts, repaired) = utopia_extract::parse_adjudication_repairing(&reply)?;
    if repaired {
        // The model broke a line in the middle of a quoted reason: repaired, reads fine, but note which cluster it was (same as #894/#895)
        tracing::warn!(
            kb_id = %ctx.kb_id,
            run_id = %ctx.run_id,
            pairs = batch.len(),
            first_pair = %crate::adjudication::batch_name(&batch),
            "裁决回复的字符串里有裸控制字符，转义后才解开"
        );
    }
    let by_i: HashMap<usize, &utopia_extract::AdjudicationVerdict> =
        verdicts.iter().map(|v| (v.i, v)).collect();
    Ok(asked
        .iter()
        .enumerate()
        .map(|(n, &idx)| {
            let look = match by_i.get(&n) {
                Some(v) => Look::from_batch(
                    match v.verdict.as_str() {
                        "same" => Some(true),
                        "different" => Some(false),
                        _ => None,
                    },
                    v.confidence.unwrap_or(0.5).clamp(0.0, 1.0),
                    v.why.clone(),
                ),
                None => Look::from_batch(None, 0.0, None),
            };
            (idx, look)
        })
        .collect())
}

async fn permit(ctx: &Ctx<'_>) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match ctx.settings.as_ref() {
        Some(s) => llm_util::acquire_chat(ctx.state, s).await,
        None => None,
    }
}

fn pair_of(item: &ReviewItem, p: &Precedents) -> utopia_extract::AdjudicationPair {
    // Same-name pairs whose broad categories do not conflict: both sides write the same type label. The extractor gives two records of the same company
    // Store and Organization, and the model takes that as the reason they are "different". Take the crutch away, so it looks at the facts
    // Only share the label when both sides map to the same broad category (person, organization, place, event): Periodical vs Service,
    // VideoGame vs no type. Those labels carry information, so keep them
    let same_kind = gov::name_shape(&item.left.name, &item.right.name) == gov::NameShape::Identical
        && matches!(
            (
                item.left.type_label.as_deref().and_then(gov::type_family),
                item.right.type_label.as_deref().and_then(gov::type_family),
            ),
            (Some(a), Some(b)) if a == b
        );
    let shared = item
        .left
        .type_label
        .clone()
        .or_else(|| item.right.type_label.clone())
        .unwrap_or_else(|| "untyped".into());
    let side = |s: &utopia_core::models::ReviewSide| utopia_extract::AdjudicationSide {
        name: s.name.clone(),
        type_label: if same_kind {
            shared.clone()
        } else {
            s.type_label.clone().unwrap_or_else(|| "untyped".into())
        },
        facts: s.top_facts.clone(),
    };
    utopia_extract::AdjudicationPair {
        left: side(&item.left),
        right: side(&item.right),
        precedents: gov::render_lines(p),
        proposed_because: utopia_extract::proposed_because(
            utopia_core::review_reasons::name_vector_cosine(item.reason.as_deref()),
        ),
    }
}

/// Layer one did not decide (no verdict, or confidence below the threshold), or same name with broad categories not in conflict, yet the model says different. This is
/// the kind it most often gets wrong: each goes to layer two to look again with all facts and source text. Merges that the shape of the name makes suspicious (version
/// tails, a long string containing a name) are also looked at again: a name can only make someone suspicious, not decide whether it is the same thing. "Lease
/// Agreement dated May 16, 2016, as amended" and "Lease Agreement" are one sentence in shape, but in fact
/// the same lease. Pairs blocked by the hard type-conflict rule do not go in: looking again will not change the rule
fn wants_second_look(item: &ReviewItem, p: &Precedents, look: &Look) -> bool {
    let types_conflict = gov::types_conflict(
        item.left.type_label.as_deref(),
        item.right.type_label.as_deref(),
    );
    let shape = gov::name_shape(&item.left.name, &item.right.name);
    let doubted_split = shape == gov::NameShape::Identical
        && !types_conflict
        && look.same == Some(false)
        && look.calls == 0;
    let doubted_merge =
        name_doubts(shape) && !types_conflict && look.same == Some(true) && look.calls == 0;
    // Pairs that name vectors propose as "same" are first looked at with tools however confident the model is (the same rule as adjudication without governance,
    // `adjudication::batch_verdict_may_apply`); ones only layer one looked at are looked at again, but not ones layer two has already seen
    let similarity_same =
        !crate::adjudication::batch_verdict_may_apply(item, look.same) && look.calls == 0;
    ((gov::gate(look.same, look.conf, types_conflict, shape, p) == Gate::Propose
        && look.uncertain())
        || doubted_split
        || doubted_merge
        || similarity_same)
        && p.reverts.is_empty()
}

/// Merges that the name shape makes suspicious: version tails, or a long string containing another name
fn name_doubts(shape: gov::NameShape) -> bool {
    matches!(shape, gov::NameShape::Version | gov::NameShape::Phrase)
}

/// The name shape the gate looks at. A pair that layer two looked at with both sides' facts and source text, and still says is the same, has had its shape doubt
/// answered by the evidence, so it is no longer blocked by shape: what remains blocked is layer one, which has not seen the evidence
fn shape_for_gate(item: &ReviewItem, look: &Look) -> gov::NameShape {
    settled_shape(
        gov::name_shape(&item.left.name, &item.right.name),
        look.calls > 0,
    )
}

/// The shape doubt is answered once layer two has read the evidence
fn settled_shape(shape: gov::NameShape, evidence_read: bool) -> gov::NameShape {
    if evidence_read && name_doubts(shape) {
        gov::NameShape::Unrelated
    } else {
        shape
    }
}

/// The adjudicator's entry point (0028): with governance off, pairs the batch cannot decide are still looked at again with tools, the same loop,
/// the same budget. Each call gets one run_id: one adjudication task is one run
pub(crate) async fn look_again(
    state: &AppState,
    kb_id: Uuid,
    client: &LlmClient,
    settings: &Option<LlmSettings>,
    item: &ReviewItem,
    pair: &utopia_extract::AdjudicationPair,
    earlier: &Look,
) -> Option<Look> {
    let ctx = Ctx {
        state,
        kb_id,
        run_id: Uuid::now_v7(),
        client,
        settings,
    };
    second_look(&ctx, item, pair, earlier).await
}

/// Layer two looks at a pair: if the budget allows, it looks, and its view replaces layer one's; if it cannot (budget used up, model error), it returns None,
/// and layer one's view is acted on
async fn second_look(
    ctx: &Ctx<'_>,
    item: &ReviewItem,
    pair: &utopia_extract::AdjudicationPair,
    look: &Look,
) -> Option<Look> {
    let kb_id = ctx.kb_id;
    let spent = match gov::loop_calls_today(&ctx.state.pool, kb_id).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(%kb_id, error = %e, "治理：数循环预算失败");
            return None;
        }
    };
    if spent >= LOOP_DAILY_CALLS {
        tracing::info!(%kb_id, spent, "治理：今天的循环预算用完，判不定的只写建议");
        return None;
    }
    match investigate(ctx, item, pair, look).await {
        Ok(l) => Some(l),
        // A layer-two failure does not drag down layer one: the pair is written as a suggestion per layer one's view
        Err(e) => {
            tracing::warn!(%kb_id, review = %item.id, error = %e, "治理：第二层没跑完");
            None
        }
    }
}

/// Applying a pair's verdict: if it passes the gate, act and record applied; otherwise hand to a person and record proposed (with the question and trace)
async fn apply(ctx: &Ctx<'_>, item: &ReviewItem, p: &Precedents, look: Look) -> anyhow::Result<()> {
    let pool = &ctx.state.pool;
    let kb_id = ctx.kb_id;
    let types_conflict = gov::types_conflict(
        item.left.type_label.as_deref(),
        item.right.type_label.as_deref(),
    );
    let shape = shape_for_gate(item, &look);

    let action = look.action();
    let precedents = gov::precedents_json(p);
    let conf = look.conf;
    let decision = |status: &'static str, merge_id: Option<Uuid>| NewDecision {
        run_id: ctx.run_id,
        target_id: item.id,
        action,
        confidence: conf,
        reason: look.why.as_deref(),
        precedents: precedents.clone(),
        status,
        merge_id,
        question: look.question.as_deref(),
        trace: Value::Array(look.trace.clone()),
        calls: look.calls,
    };

    // Similar-name "same" verdicts where the second look did not run (budget used up, model error) fail the gate and go to a person;
    // the view is recorded as a suggestion: similar names are not evidence that they are the same thing, facts are
    if look.same == Some(true)
        && look.calls == 0
        && !crate::adjudication::batch_verdict_may_apply(item, look.same)
    {
        utopia_store::resolution::escalate_review(
            pool,
            item.id,
            crate::adjudication::SECOND_LOOK_UNAVAILABLE,
        )
        .await?;
        gov::record(
            pool,
            kb_id,
            NewDecision {
                reason: Some("held for a person: the names are similar, not the same string, and the second look did not run"),
                ..decision("proposed", None)
            },
        )
        .await?;
        return Ok(());
    }
    match gov::gate(look.same, look.conf, types_conflict, shape, p) {
        Gate::Apply if look.same == Some(true) => {
            let reason = format!("governed|{conf:.2}");
            // Chained merges in one cluster: after the previous pair merged, one side of this pair may already have been merged into someone else; merge the surviving one
            let (l, r) = (
                utopia_store::resolution::survivor(pool, kb_id, item.left.id).await?,
                utopia_store::resolution::survivor(pool, kb_id, item.right.id).await?,
            );
            if l == r {
                // Both sides are already the same entity: only the review row is left to close. If it cannot be closed, someone already closed it, so no further record is written
                let closed =
                    utopia_store::resolution::close_review_auto(pool, item.id, "merged", &reason)
                        .await?;
                if closed > 0 {
                    let id = gov::record(pool, kb_id, decision("applied", None)).await?;
                    audit(ctx, "review.merge", item, conf, id).await;
                }
                return Ok(());
            }
            // Execution gate (0027): a merge immediately sends things outside the graph (violations, derivations, answers), so it is left to a person;
            // the identity source is also left to a person (0071), and no action is taken however confident. The agent's view is recorded as a suggestion,
            // with the reason stated up front as coming from the gate
            if let Some(hold) = execution_gate::hold_for_merge(pool, kb_id, l, r).await? {
                utopia_store::resolution::escalate_review(pool, item.id, &hold.review_reason())
                    .await?;
                let held = match look.why.as_deref() {
                    Some(why) => format!("held for a person: {}; {why}", hold.explain()),
                    None => format!("held for a person: {}", hold.explain()),
                };
                gov::record(
                    pool,
                    kb_id,
                    NewDecision {
                        reason: Some(&held),
                        ..decision("proposed", None)
                    },
                )
                .await?;
                return Ok(());
            }
            let (target, source) = utopia_store::resolution::merge_direction(pool, l, r).await?;
            match utopia_store::resolution::merge_entities(
                pool, kb_id, source, target, None, &reason,
            )
            .await
            {
                Ok(merge_id) => {
                    utopia_store::resolution::close_review_auto(pool, item.id, "merged", &reason)
                        .await?;
                    let id = gov::record(pool, kb_id, decision("applied", Some(merge_id))).await?;
                    audit(ctx, "review.merge", item, conf, id).await;
                }
                // A chained merge in the same cluster has already absorbed one side: leave it to a person, and record an unsure so the next round does not hit it again
                Err(AppError::Conflict(_)) | Err(AppError::NotFound) => {
                    utopia_store::resolution::escalate_review(
                        pool,
                        item.id,
                        "escalate_entity_changed",
                    )
                    .await?;
                    gov::record(
                        pool,
                        kb_id,
                        NewDecision {
                            action: "unsure",
                            reason: Some("one side changed while this cluster was being decided"),
                            ..decision("proposed", None)
                        },
                    )
                    .await?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Gate::Apply => {
            let reason = format!("governed|{conf:.2}");
            // Closing failed: this pair is no longer pending (a person ruled, or another path got there first): no identical ruling is recorded again
            let closed =
                utopia_store::resolution::close_review_auto(pool, item.id, "kept", &reason).await?;
            if closed > 0 {
                let id = gov::record(pool, kb_id, decision("applied", None)).await?;
                audit(ctx, "review.keep", item, conf, id).await;
            }
        }
        Gate::Propose => {
            utopia_store::resolution::escalate_review(pool, item.id, "proposed").await?;
            gov::record(pool, kb_id, decision("proposed", None)).await?;
        }
    }
    Ok(())
}

/// Layer two: looks at each pair with tools. On each turn the model either queries something (feeding the result back), or
/// closes with decide / defer. The number of turns is capped; if the model closes without using a tool, it is reminded once, and if it still does not close, it is treated as undecided
async fn investigate(
    ctx: &Ctx<'_>,
    item: &ReviewItem,
    pair: &utopia_extract::AdjudicationPair,
    earlier: &Look,
) -> anyhow::Result<Look> {
    let tools = governor::tools();
    let mut messages = governor::messages(
        pair,
        &governor::EarlierLook {
            verdict: match earlier.same {
                Some(true) => "same",
                Some(false) => "different",
                None => "unsure",
            },
            confidence: earlier.conf,
            why: earlier.why.as_deref(),
        },
    );
    let mut trace: Vec<Value> = Vec::new();
    let mut calls = 0;
    let mut nudged = false;
    let mut walls = 0;
    let mut lookups = 0;

    // Turn limit = query count + two hits on the cap + one reminder + the closing one. The model usually queries one thing per turn,
    // but after MAX_STEPS queries it often still wants to query more: the turn that hits the cap must be counted outside the budget, or it never gets a chance to close
    // (on the identity bench, five of the nine "looked but did not close" second looks came from this)
    for _ in 0..(governor::MAX_STEPS + 4) {
        let turn = {
            let _permit = permit(ctx).await;
            ctx.client.chat_tools(&messages, &tools).await?
        };
        calls += 1;
        messages.push(turn.to_message());
        if turn.tool_calls.is_empty() {
            // Spoke without calling a tool: record what it said, so the trace shows where it got stuck next time
            trace.push(json!({ "said": turn.content.as_deref().unwrap_or("").chars().take(200).collect::<String>() }));
            if nudged {
                break;
            }
            nudged = true;
            messages.push(json!({ "role": "user", "content": governor::NUDGE }));
            continue;
        }
        for call in &turn.tool_calls {
            match governor::read_step(&call.name, &call.arguments) {
                Step::Decide {
                    same,
                    confidence,
                    why,
                } => {
                    return Ok(Look {
                        same: Some(same),
                        conf: confidence,
                        why: Some(why).filter(|w| !w.is_empty()),
                        question: None,
                        trace,
                        calls,
                    });
                }
                Step::Defer { question } => {
                    return Ok(Look {
                        same: None,
                        conf: 0.0,
                        why: None,
                        question: Some(question),
                        trace,
                        calls,
                    });
                }
                Step::Lookup { tool, args } => {
                    let out = if lookups >= governor::MAX_STEPS {
                        walls += 1;
                        trace.push(
                            json!({ "tool": tool, "args": args, "note": "refused: lookup limit" }),
                        );
                        governor::LIMIT_REACHED.to_string()
                    } else {
                        lookups += 1;
                        let (out, note) = lookup(ctx, item, &tool, &args).await?;
                        trace.push(json!({ "tool": tool, "args": args, "note": note }));
                        out
                    };
                    messages.push(tool_result_message(&call.id, &out));
                }
                Step::Unknown(problem) => {
                    messages.push(tool_result_message(&call.id, &problem));
                }
            }
        }
        // Still querying after hitting the cap twice: it will not close, so stop spending turns
        if walls >= 2 {
            break;
        }
    }
    // Looked, did not close: treated as undecided, and the trace is kept
    Ok(Look {
        same: None,
        conf: 0.0,
        why: Some("the agent looked but did not conclude".into()),
        question: None,
        trace,
        calls,
    })
}

/// Runs a tool: the text for the model, and one sentence written into the trace
async fn lookup(
    ctx: &Ctx<'_>,
    item: &ReviewItem,
    tool: &str,
    args: &Value,
) -> anyhow::Result<(String, String)> {
    let pool = &ctx.state.pool;
    let kb_id = ctx.kb_id;
    let side = |s: &str| {
        if s == "A" {
            &item.left
        } else {
            &item.right
        }
    };
    let clip = |s: String| {
        if s.chars().count() > TOOL_OUTPUT_CHARS {
            let mut t: String = s.chars().take(TOOL_OUTPUT_CHARS).collect();
            t.push_str("\n…");
            t
        } else {
            s
        }
    };
    let (out, note) = match tool {
        "facts" => {
            let s = args["side"].as_str().unwrap_or("A");
            let e = side(s);
            let lines = utopia_store::resolution::entity_fact_lines(pool, kb_id, e.id, 30).await?;
            let n = lines.len();
            let out = if lines.is_empty() {
                "(no recorded facts)".to_string()
            } else {
                lines.join("\n")
            };
            (out, format!("{n} facts of {s} \"{}\"", e.name))
        }
        "quotes" => {
            let s = args["side"].as_str().unwrap_or("A");
            let e = side(s);
            let rows = gov::quotes_of(pool, kb_id, e.id, 6).await?;
            let n = rows.len();
            let out = if rows.is_empty() {
                "(no source passages)".to_string()
            } else {
                rows.iter()
                    .map(|(doc, q)| format!("[{doc}] {q}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (out, format!("{n} passages about {s} \"{}\"", e.name))
        }
        "ledger" => {
            let q = args["query"].as_str().unwrap_or("");
            let rows = gov::ledger_search(pool, kb_id, q, 10).await?;
            let n = rows.len();
            let out = if rows.is_empty() {
                "(no decisions by people about this name)".to_string()
            } else {
                rows.iter()
                    .map(|x| {
                        let verb = match x.action.as_str() {
                            "merge.revert" => "merge reverted",
                            "review.keep" => "kept apart",
                            _ => "merged",
                        };
                        // The person's written reason follows: layer two looks up the ledger, and what it finds should be "on what grounds"
                        let because = x
                            .why
                            .as_deref()
                            .map(|w| format!("; they wrote: \"{w}\""))
                            .unwrap_or_default();
                        format!(
                            "\"{}\" ≟ \"{}\": {verb} by a person on {}{because}",
                            x.left,
                            x.right,
                            x.at.format("%Y-%m-%d")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (out, format!("{n} decisions about \"{q}\""))
        }
        // What a merge would affect (0028): the model first sees what the gate (0027) would see, then decides whether to rule or ask
        "consequences" => {
            let impact =
                execution_gate::impact_of(pool, kb_id, item.left.id, item.right.id).await?;
            let families = if gov::types_conflict(
                item.left.type_label.as_deref(),
                item.right.type_label.as_deref(),
            ) {
                "\n- the two types belong to different families; the rules never merge across families"
            } else {
                ""
            };
            let out = format!("{}{families}", impact.describe());
            let held = execution_gate::hold(&impact)
                .map(|h| h.to_string())
                .unwrap_or_else(|| "nothing held".into());
            (out, format!("what a merge would touch: {held}"))
        }
        "namesakes" => {
            let q = args["query"].as_str().unwrap_or("");
            let rows = gov::namesakes(pool, kb_id, q, 10).await?;
            let n = rows.len();
            let out = if rows.is_empty() {
                "(no other entity with such a name)".to_string()
            } else {
                rows.iter()
                    .map(|x| {
                        format!(
                            "\"{}\" ({}) · {} facts{}",
                            x.name,
                            x.type_label.as_deref().unwrap_or("untyped"),
                            x.facts,
                            if x.merged {
                                " · merged into another"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (out, format!("{n} entities named like \"{q}\""))
        }
        other => (
            format!("no tool named {other}"),
            format!("unknown tool {other}"),
        ),
    };
    Ok((clip(out), note))
}

/// The decision ledger: actor is empty (a machine), detail says it is governance, the confidence, and which agent decision it corresponds to
async fn audit(ctx: &Ctx<'_>, action: &str, item: &ReviewItem, conf: f32, id: Uuid) {
    let _ = utopia_store::audit::record_opt(
        &ctx.state.pool,
        Some(ctx.kb_id),
        None,
        action,
        "review",
        Some(item.id),
        json!({
            "left": item.left.name, "right": item.right.name, "score": item.score,
            "confidence": conf, "via": "governor", "decision": id,
        }),
    )
    .await;
}

/// Fuse (0025 decision 9): since it was turned on, within seven days, a person retracted the agent's automatic merges twice,
/// so the switch turns itself off, an alert is sent, and the ledger records it. It made two wrong calls on the person's behalf, so it should stop and wait for a person to turn it on again;
/// governance_since is updated when a person turns it on again, and earlier retractions no longer count
pub async fn fuse(state: &AppState, kb_id: Uuid) {
    let pool = &state.pool;
    let kb = match utopia_store::kbs::get(pool, kb_id).await {
        Ok(kb) if kb.governance => kb,
        _ => return,
    };
    let window = Utc::now() - Duration::days(gov::FUSE_WINDOW_DAYS);
    let since = kb.governance_since.map_or(window, |s| s.max(window));
    let reverts = match gov::reverts_since(pool, kb_id, since).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(%kb_id, error = %e, "治理：数撤回失败");
            return;
        }
    };
    if reverts < gov::FUSE_REVERTS {
        return;
    }
    match gov::trip(pool, kb_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(%kb_id, error = %e, "治理：关开关失败");
            return;
        }
    }
    tracing::warn!(%kb_id, reverts, "治理：保险丝跳了，开关已关");
    if let Err(e) = alerts::raise(
        pool,
        alerts::NewAlert {
            kb_id: Some(kb_id),
            severity: "warning",
            kind: alerts::kind::GOVERNANCE_TRIPPED,
            min_role: Role::Editor,
            subject_type: Some("kb"),
            subject_id: Some(kb_id),
            detail: json!({
                "kb": kb.name, "reverts": reverts, "window_days": gov::FUSE_WINDOW_DAYS,
            }),
        },
    )
    .await
    {
        tracing::warn!(%kb_id, error = %e, "上报 governance.tripped 失败");
    }
    let _ = utopia_store::audit::record_opt(
        pool,
        Some(kb_id),
        None,
        "kb.updated",
        "kb",
        Some(kb_id),
        json!({ "governance": false, "via": "fuse", "reverts": reverts }),
    )
    .await;
    state.emit_review(kb_id);
}

/// A person retracted a merge from the merge history: if the agent itself ruled it, record it as reverted, then check the fuse
pub async fn after_revert(state: &AppState, kb_id: Uuid, merge_id: Uuid, user_id: Uuid) {
    match gov::settle_by_merge(&state.pool, kb_id, merge_id, user_id).await {
        Ok(Some(_)) => fuse(state, kb_id).await,
        Ok(None) => {}
        Err(e) => tracing::warn!(%kb_id, merge = %merge_id, error = %e, "治理：记撤回失败"),
    }
}

/// After a person rules on a pair: the suggestion open on that pair is answered (same as the suggestion is accept; different is a revised ruling);
/// suggestions on same-name or same-entity pairs go stale and those pairs return to the queue; if the switch is on, a round is queued.
/// A person split three pairs of 张伟 in bulk; the agent follows the same cluster and applies the rest. The automatic handling #428 asks for is exactly this step.
/// `action` None means this pair's suggestion is not answered (the Agent queue path already answered it itself)
pub async fn after_human_decision(
    state: &AppState,
    kb_id: Uuid,
    review_ids: &[Uuid],
    action: Option<&str>,
    user_id: Uuid,
) {
    for &id in review_ids {
        if let Some(action) = action {
            if let Err(e) = gov::answer_open(&state.pool, kb_id, id, action, user_id).await {
                tracing::warn!(%kb_id, review = %id, error = %e, "治理：建议回答失败");
            }
        }
        if let Err(e) = gov::supersede_siblings(&state.pool, kb_id, id).await {
            tracing::warn!(%kb_id, review = %id, error = %e, "治理：建议作废失败");
        }
    }
    match utopia_store::kbs::get(&state.pool, kb_id).await {
        Ok(kb) if kb.governance => {
            if let Err(e) = utopia_store::jobs::enqueue_unless_queued(
                &state.pool,
                "govern",
                json!({ "kb_id": kb_id }),
            )
            .await
            {
                tracing::warn!(%kb_id, error = %e, "治理任务入队失败");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_doubtful_shape_stops_counting_once_the_evidence_was_read() {
        let lease = gov::name_shape(
            "Lease Agreement dated May 16, 2016, as amended",
            "Lease Agreement",
        );
        assert!(name_doubts(lease), "{lease:?}");
        assert_eq!(settled_shape(lease, false), lease);
        assert_eq!(settled_shape(lease, true), gov::NameShape::Unrelated);

        let version = gov::name_shape("Claude Mythos 5", "Claude Mythos");
        assert!(name_doubts(version), "{version:?}");
        assert_eq!(settled_shape(version, false), version);

        // A shape that was not suspicious to begin with: whether or not evidence was read, it stays as before
        let same = gov::name_shape("OpenAI", "OpenAI");
        assert!(!name_doubts(same));
        assert_eq!(settled_shape(same, true), same);
    }
}
