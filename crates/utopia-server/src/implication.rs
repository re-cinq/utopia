//! Two server-side jobs for implication rules (0044 decision 3, fifth slice): at the end of alignment, propose rules to the model;
//! the `read_phrases` job adds the readings the approved rules need to the cache, then queues materialization.
//!
//! Materialization itself does not call the model (`utopia_store::materialize`); a round whose readings are not yet in the cache is skipped.

use std::collections::HashMap;
use utopia_core::models::RelationTypeView;
use utopia_extract::implication::{
    build_reading_messages, build_rule_messages, parse_reading_response, parse_rule_response,
    ReadingItem, RuleItem,
};
use utopia_extract::phrase_align::PropertyCandidate;
use utopia_store::implication_rules::{self, Proposal, READINGS};
use utopia_store::phrase_bindings::{self, PhraseSignature};
use utopia_store::type_bindings::KindWordSignature;
use uuid::Uuid;

use crate::extraction::chat_retrying_rate_limits_at;
use crate::llm_util;
use crate::state::AppState;

const BATCH: usize = 12;

/// Input for one rule proposal: the signature (with the properties and candidates it binds to) and the category words (with their candidates).
pub struct RuleAsk<'a> {
    pub phrase: Option<&'a PhraseSignature>,
    pub kind_word: Option<&'a KindWordSignature>,
    pub bound_to: Option<&'a str>,
    pub candidates: Vec<&'a RelationTypeView>,
    pub basis: &'a str,
}

/// Propose rules to the model, turning the answers into proposals (for a person to approve) or the agent's rejections (nothing implied; recorded so it is not asked again).
/// Returns (number of proposals, failed batches). A shape with no candidates is not asked about
pub async fn propose_rules(
    state: &AppState,
    kb_id: Uuid,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
    asks: &[RuleAsk<'_>],
    class_key: &HashMap<Uuid, &str>,
    by_key: &HashMap<&str, &RelationTypeView>,
) -> anyhow::Result<(usize, usize)> {
    let pool = &state.pool;
    let keys_of = |ids: &[Uuid]| -> Vec<&str> {
        ids.iter()
            .filter_map(|id| class_key.get(id).copied())
            .collect()
    };
    let (mut proposed, mut failed) = (0usize, 0usize);
    let asks: Vec<&RuleAsk<'_>> = asks.iter().filter(|a| !a.candidates.is_empty()).collect();
    // Batches run in parallel (same reason as phrase alignment: waiting on the model one after another for 20 rounds is ten minutes)
    {
        use futures_util::StreamExt;
        let (keys_of, by_key) = (&keys_of, &by_key);
        let futures: Vec<_> = asks
            .chunks(BATCH)
            .map(|batch| async move {
                let mut proposed = 0usize;
        let items: Vec<RuleItem<'_>> = batch
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let candidates = a
                    .candidates
                    .iter()
                    .map(|p| PropertyCandidate {
                        key: &p.key,
                        label: &p.label,
                        description: &p.description,
                        kind: &p.kind,
                        temporal: &p.temporal,
                        domains: keys_of(&p.domains),
                        ranges: keys_of(&p.ranges),
                        via: Vec::new(),
                    })
                    .collect();
                match (a.phrase, a.kind_word) {
                    (Some(s), _) => RuleItem {
                        id: i as i64,
                        trigger: "phrase",
                        phrase: &s.phrase,
                        subject_class: s.subject_type_key.as_deref(),
                        object_class: s.object_type_key.as_deref(),
                        object_is_value: s.object_is_value,
                        bound_to: a.bound_to,
                        examples: &s.examples,
                        candidates,
                    },
                    (None, Some(k)) => RuleItem {
                        id: i as i64,
                        trigger: "kind_word",
                        phrase: &k.kind_word,
                        subject_class: None,
                        object_class: None,
                        object_is_value: false,
                        bound_to: None,
                        examples: &k.examples,
                        candidates,
                    },
                    (None, None) => unreachable!("an ask is a phrase or a kind word"),
                }
            })
            .collect();
        let messages = build_rule_messages(&items, READINGS);
        let reply =
            match chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0)).await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(%kb_id, error = %e, "rule-proposal call failed, this batch is deferred to next time");
                    return Ok::<_, anyhow::Error>((0usize, 1usize));
                }
            };
        let (choices, malformed) = match parse_rule_response(&reply.text, &items, READINGS) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(%kb_id, error = %e, "failed to parse rule-proposal reply, this batch is deferred to next time");
                return Ok::<_, anyhow::Error>((0usize, 1usize));
            }
        };
        if malformed > 0 {
            tracing::info!(%kb_id, malformed, "the rule-proposal reply contains bad items");
        }
        for c in choices {
            let Ok(i) = usize::try_from(c.id) else {
                continue;
            };
            let Some(a) = batch.get(i) else { continue };
            let (trigger, phrase, subject, object, value, count, examples) =
                match (a.phrase, a.kind_word) {
                    (Some(s), _) => (
                        "phrase",
                        s.phrase.as_str(),
                        s.subject_type_id,
                        s.object_type_id,
                        s.object_is_value,
                        s.count,
                        s.examples.as_slice(),
                    ),
                    (None, Some(k)) => (
                        "kind_word",
                        k.kind_word.as_str(),
                        None,
                        None,
                        false,
                        k.count,
                        k.examples.as_slice(),
                    ),
                    (None, None) => continue,
                };
            match c.implies {
                Some((key, reading)) => {
                    let Some(p) = by_key.get(key.as_str()) else {
                        continue;
                    };
                    let votes =
                        serde_json::json!({ "agent": { "property": key, "reading": reading } });
                    if implication_rules::propose(
                        pool,
                        kb_id,
                        &Proposal {
                            trigger,
                            phrase,
                            subject_type_id: subject,
                            object_type_id: object,
                            object_is_value: value,
                            conclude_property_id: p.id,
                            reading: reading.as_deref(),
                            status: "proposed",
                            votes: &votes,
                            basis: a.basis,
                            statement_count: count,
                            examples,
                        },
                    )
                    .await?
                    .is_some()
                    {
                        proposed += 1;
                    }
                }
                None => {
                    // "Nothing implied" must also be written down, or it is asked every round. It is written as one row of agent rejection:
                    // the property column cannot be empty, so the shape itself is recorded here, with a property bound to the signature or the first candidate as the placeholder
                    let placeholder = a
                        .bound_to
                        .and_then(|k| by_key.get(k))
                        .or_else(|| a.candidates.first())
                        .map(|p| p.id);
                    let Some(property) = placeholder else {
                        continue;
                    };
                    let votes = serde_json::json!({ "agent": null, "reason": "nothing_implied" });
                    implication_rules::propose(
                        pool,
                        kb_id,
                        &Proposal {
                            trigger,
                            phrase,
                            subject_type_id: subject,
                            object_type_id: object,
                            object_is_value: value,
                            conclude_property_id: property,
                            reading: None,
                            status: "rejected",
                            votes: &votes,
                            basis: a.basis,
                            statement_count: count,
                            examples,
                        },
                    )
                    .await?;
                }
            }
        }

                Ok::<_, anyhow::Error>((proposed, 0usize))
            })
            .collect();
        let mut results = futures_util::stream::iter(futures).buffer_unordered(4);
        while let Some(r) = results.next().await {
            let (p, f) = r?;
            proposed += p;
            failed += f;
        }
    }
    Ok((proposed, failed))
}

/// `read_phrases` job: readings the approved rules need that are not yet in the cache; ask the model once and store them in the cache;
/// names are resolved to entities in the KB (creating a named one if none exists); unreadable ones are also recorded so they are not asked again.
/// After filling, queue one materialization pass, where implied rows are computed
pub async fn read_phrases(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    let pool = &state.pool;
    let pending = implication_rules::pending_readings(pool, kb_id).await?;
    if pending.is_empty() {
        tracing::info!(%kb_id, "nothing pending to read");
    } else {
        let kb = utopia_store::kbs::get(pool, kb_id).await?;
        let settings = utopia_store::settings::get(pool, kb.workspace_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot read phrases"))?;
        let client = llm_util::chat_client(&settings)
            .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot read phrases"))?;
        let (mut answered, mut failed) = (0usize, 0usize);
        for batch in pending.chunks(BATCH * 2) {
            let items: Vec<ReadingItem<'_>> = batch
                .iter()
                .enumerate()
                .map(|(i, p)| ReadingItem {
                    id: i as i64,
                    reading: &p.reading,
                    phrase: &p.phrase,
                })
                .collect();
            let messages = build_reading_messages(&items, READINGS);
            let reply = match chat_retrying_rate_limits_at(
                state,
                &settings,
                &client,
                &messages,
                Some(0.0),
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(%kb_id, error = %e, "reading call failed, this batch is deferred to next time");
                    failed += 1;
                    continue;
                }
            };
            let (answers, malformed) = match parse_reading_response(&reply.text, &items) {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(%kb_id, error = %e, "failed to parse reading reply, this batch is deferred to next time");
                    failed += 1;
                    continue;
                }
            };
            if malformed > 0 {
                tracing::info!(%kb_id, malformed, "the reading reply contains bad items");
            }
            for a in answers {
                let Ok(i) = usize::try_from(a.id) else {
                    continue;
                };
                let Some(p) = batch.get(i) else { continue };
                let entity = match &a.name {
                    Some(name) => {
                        Some(implication_rules::resolve_or_create_named(pool, kb_id, name).await?)
                    }
                    None => None,
                };
                let value = a.value.as_ref().map(|v| serde_json::json!({ "value": v }));
                implication_rules::record_reading(
                    pool,
                    kb_id,
                    &p.reading,
                    &p.phrase,
                    entity,
                    value.as_ref(),
                )
                .await?;
                answered += 1;
            }
        }
        tracing::info!(%kb_id, pending = pending.len(), answered, failed, "reading cache fill complete");
        if failed > 0 {
            anyhow::bail!("{failed} reading batches failed; the job retries");
        }
    }
    utopia_store::jobs::enqueue_unless_queued(
        pool,
        phrase_bindings::MATERIALIZE_KIND,
        serde_json::json!({ "kb_id": kb_id }),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "implication_tests.rs"]
mod tests;
