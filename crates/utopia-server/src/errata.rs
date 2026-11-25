//! Errata agent (0044 decision 7, sixth slice): after extraction, re-review the typed graph against the document.
//!
//! Per document, once: facts the structure flagged are sent for review first, and the rest are sampled; one request carries at most [`FACTS_PER_REQUEST`] facts,
//! and a document gets at most [`REQUESTS_PER_DOCUMENT`] requests. This is the "per-document budget". The model uses the JSON action
//! protocol (`utopia_extract::errata`), and each decision is recorded in the store as an action (`utopia_store::errata`). Retract, change,
//! and add carry the document's own wording; anything that affects things outside the graph is left to a person (the gate of 0027).
//!
//! Metrics are recorded on `errata_runs`: how many facts were looked at, how many questions were asked, and the tokens the endpoint reported. How much precision
//! was gained and how many wrong retractions were made is worked out by the typed benchmark from these numbers and the judgments.

use std::collections::HashMap;
use utopia_extract::errata::{
    build_confirm_messages, build_errata_messages, parse_confirm_response, parse_errata_response,
    ErrataFact, ErrataProperty, Verdict,
};
use utopia_store::errata::{self, ActionInput, Candidate, Proposed};
use uuid::Uuid;

use crate::extraction::chat_retrying_rate_limits_at;
use crate::llm_util;
use crate::state::AppState;

/// Maximum documents a job looks at; any left over are queued again
pub const DOCS_PER_JOB: i64 = 20;
/// Maximum facts sent for one document: those the structure flagged take the slots first, and the sample fills up to this number
pub const FACTS_PER_DOCUMENT: usize = 40;
/// Maximum number of sampled facts (those not flagged)
pub const SAMPLE: usize = 10;
/// Maximum facts per request
pub const FACTS_PER_REQUEST: usize = 20;
/// Maximum questions asked per document (the second vote on retract/change is counted separately, at most once per batch)
pub const REQUESTS_PER_DOCUMENT: usize = 2;
/// Maximum characters of body text carried; longer documents are truncated, and the cut-off part is not visible in this round
pub const DOC_CHARS: usize = 16_000;

/// The tally for a document once reviewed
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DocumentOutcome {
    pub reviewed: usize,
    pub applied: usize,
    pub held: usize,
    pub refused: usize,
    pub requests: usize,
}

/// `errata_review` job: documents with typed rows not yet reviewed are reviewed one at a time; if not all are done, it is queued again
pub async fn review(state: &AppState, kb_id: Uuid) -> anyhow::Result<()> {
    let pool = &state.pool;
    let due = errata::documents_due(pool, kb_id, DOCS_PER_JOB).await?;
    if due.is_empty() {
        tracing::info!(%kb_id, "no documents pending errata");
        return Ok(());
    }
    let kb = utopia_store::kbs::get(pool, kb_id).await?;
    let settings = utopia_store::settings::get(pool, kb.workspace_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot review errata"))?;
    let client = llm_util::chat_client(&settings)
        .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot review errata"))?;
    let props = utopia_store::ontology::relation_type_views(pool, kb_id).await?;
    let classes = utopia_store::graph::entity_types(pool, kb_id).await?;
    let class_key: HashMap<Uuid, &str> = classes.iter().map(|c| (c.id, c.key.as_str())).collect();
    let glossary: Vec<ErrataProperty<'_>> = props
        .iter()
        .map(|p| ErrataProperty {
            key: &p.key,
            label: &p.label,
            description: &p.description,
            kind: &p.kind,
            domains: p
                .domains
                .iter()
                .filter_map(|d| class_key.get(d).copied())
                .collect(),
            ranges: p
                .ranges
                .iter()
                .filter_map(|r| class_key.get(r).copied())
                .collect(),
            datatype: p.datatype.as_deref(),
        })
        .collect();
    let mut failed = 0usize;
    let (mut applied, mut held) = (0usize, 0usize);
    for document_id in &due {
        match review_document(state, &settings, &client, kb_id, *document_id, &glossary).await {
            Ok(o) => {
                applied += o.applied;
                held += o.held;
            }
            Err(e) => {
                tracing::warn!(%kb_id, %document_id, error = %e, "errata for this document did not finish, the task will come back on retry");
                failed += 1;
            }
        }
    }
    if applied > 0 || held > 0 {
        let _ = utopia_store::audit::record(
            pool,
            Some(kb_id),
            Uuid::nil(),
            "errata.reviewed",
            "knowledge_base",
            Some(kb_id),
            serde_json::json!({ "documents": due.len(), "applied": applied, "held": held }),
        )
        .await;
        state.emit_review(kb_id);
        state.emit_graph(kb_id);
    }
    if failed > 0 {
        anyhow::bail!("{failed} documents failed errata review; the job retries");
    }
    if !errata::documents_due(pool, kb_id, 1).await?.is_empty() {
        utopia_store::jobs::enqueue_unless_queued(
            pool,
            errata::JOB_KIND,
            serde_json::json!({ "kb_id": kb_id }),
        )
        .await?;
    }
    Ok(())
}

/// One document: pick what to look at, ask within budget, and record each vote
pub async fn review_document(
    state: &AppState,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
    kb_id: Uuid,
    document_id: Uuid,
    glossary: &[ErrataProperty<'_>],
) -> anyhow::Result<DocumentOutcome> {
    let pool = &state.pool;
    let all = errata::candidates(pool, kb_id, document_id).await?;
    let chosen = choose(&all);
    let mut outcome = DocumentOutcome::default();
    // No rows to look at: the first time a document is seen, send an empty list (the agent can only add); documents already reviewed are not sent again
    if chosen.is_empty() && errata::runs_of(pool, document_id).await? > 0 {
        return Ok(outcome);
    }
    let flagged = chosen.iter().filter(|c| c.flag.is_some()).count();
    let run = errata::start_run(
        pool,
        kb_id,
        document_id,
        flagged as i32,
        (chosen.len() - flagged) as i32,
    )
    .await?;
    let text = errata::document_text(pool, document_id).await?;
    let document = truncate(&text, DOC_CHARS);
    // The property table is trimmed to this document: carrying the whole ontology each time is 4k of the 4.7k tokens in an errata request (bench README,
    // 2026-09-24). The document's vectors pick the nearest few dozen, plus the properties already used by facts in the list; with no vectors, all are carried
    let glossary = shortlist_glossary(state, settings, kb_id, document, glossary, &chosen).await;
    let glossary = &glossary[..];
    let (mut prompt_tokens, mut completion_tokens, mut saw_usage) = (0u64, 0u64, false);
    let mut error = None;
    let batches: Vec<&[Candidate]> = if chosen.is_empty() {
        vec![&chosen[..]]
    } else {
        chosen
            .chunks(FACTS_PER_REQUEST)
            .take(REQUESTS_PER_DOCUMENT)
            .collect()
    };
    for batch in batches {
        let items: Vec<ErrataFact<'_>> = batch
            .iter()
            .enumerate()
            .map(|(i, c)| ErrataFact {
                id: i as i64,
                subject: &c.subject,
                subject_class: c.subject_class.as_deref(),
                property: &c.property,
                object: &c.object,
                object_class: c.object_class.as_deref(),
                flag: c.flag.as_deref(),
                quote: c.quote.as_deref(),
            })
            .collect();
        let messages = build_errata_messages(document, glossary, &items);
        outcome.requests += 1;
        let reply =
            match chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0)).await
            {
                Ok(r) => r,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            };
        if let Some(u) = reply.usage {
            saw_usage = true;
            prompt_tokens += u.prompt_tokens;
            completion_tokens += u.completion_tokens;
        }
        let parsed = match parse_errata_response(&reply.text, &items) {
            Ok(p) => p,
            Err(e) => {
                error = Some(anyhow::anyhow!("errata reply unreadable: {e}"));
                break;
            }
        };
        if parsed.malformed > 0 || parsed.additions_refused > 0 {
            tracing::info!(%kb_id, %document_id, malformed = parsed.malformed,
                additions_refused = parsed.additions_refused, "勘误回复里有坏项");
        }
        // Second vote: for facts the structure flagged that the agent wants to retract or change, ask again whether the document says so; any that did not get not_stated
        // are changed to keep (the ones not flagged are left to a person by the library, so they are not asked about)
        let mut verdicts = parsed.verdicts;
        let doubted: Vec<ErrataFact<'_>> = verdicts
            .iter()
            .filter(|v| v.verdict != Verdict::Keep)
            .filter_map(|v| usize::try_from(v.id).ok().and_then(|i| items.get(i)))
            .filter(|f| f.flag.is_some())
            .cloned()
            .collect();
        if !doubted.is_empty() {
            let ids: Vec<i64> = doubted.iter().map(|f| f.id).collect();
            let messages = build_confirm_messages(document, &doubted);
            outcome.requests += 1;
            let reply =
                match chat_retrying_rate_limits_at(state, settings, client, &messages, Some(0.0))
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                };
            if let Some(u) = reply.usage {
                saw_usage = true;
                prompt_tokens += u.prompt_tokens;
                completion_tokens += u.completion_tokens;
            }
            let not_stated = match parse_confirm_response(&reply.text, &ids) {
                Ok(x) => x,
                Err(e) => {
                    error = Some(anyhow::anyhow!("errata second vote unreadable: {e}"));
                    break;
                }
            };
            for v in verdicts.iter_mut() {
                if v.verdict != Verdict::Keep && ids.contains(&v.id) && !not_stated.contains(&v.id)
                {
                    v.verdict = Verdict::Keep;
                    v.reason = format!("second vote: stated ({})", v.reason);
                    v.quote = None;
                }
            }
        }
        for v in &verdicts {
            let Some(c) = usize::try_from(v.id).ok().and_then(|i| batch.get(i)) else {
                continue;
            };
            let proposed = match &v.verdict {
                Verdict::Keep => Proposed::Keep,
                Verdict::Retract => Proposed::Retract,
                Verdict::Revise { property, object } => Proposed::Revise {
                    property: property.clone(),
                    object: object.clone(),
                },
            };
            let r = errata::record(
                pool,
                kb_id,
                ActionInput {
                    run_id: run,
                    document_id,
                    candidate: Some(c),
                    proposed,
                    reason: &v.reason,
                    quote: v.quote.as_deref(),
                    document_text: &text,
                },
            )
            .await?;
            outcome.reviewed += 1;
            count(&mut outcome, r.status, &v.verdict);
        }
        for a in &parsed.additions {
            let r = errata::record(
                pool,
                kb_id,
                ActionInput {
                    run_id: run,
                    document_id,
                    candidate: None,
                    proposed: Proposed::Add {
                        subject: a.subject.clone(),
                        property: a.property.clone(),
                        object: a.object.clone(),
                    },
                    reason: &a.reason,
                    quote: Some(&a.quote),
                    document_text: &text,
                },
            )
            .await?;
            count(&mut outcome, r.status, &Verdict::Retract);
        }
    }
    errata::finish_run(
        pool,
        run,
        outcome.requests as i32,
        saw_usage.then_some(prompt_tokens as i64),
        saw_usage.then_some(completion_tokens as i64),
    )
    .await?;
    tracing::info!(%kb_id, %document_id, reviewed = outcome.reviewed, applied = outcome.applied,
        held = outcome.held, refused = outcome.refused, requests = outcome.requests, "勘误看完一份文档");
    match error {
        Some(e) => Err(e),
        None => Ok(outcome),
    }
}

/// Number of properties carried per errata request (when vectors are present)
const GLOSSARY_NEAREST: i64 = 24;

/// Properties this document can see: the nearest [`GLOSSARY_NEAREST`] by vector, plus those already used by facts in the list.
/// With no embedding model, no vectors on the properties, or embedding failure: all of them
async fn shortlist_glossary<'a>(
    state: &AppState,
    settings: &utopia_core::models::LlmSettings,
    kb_id: Uuid,
    document: &str,
    all: &[ErrataProperty<'a>],
    chosen: &[Candidate],
) -> Vec<ErrataProperty<'a>> {
    let Some(client) = crate::llm_util::embed_client(settings) else {
        return all.to_vec();
    };
    let vector = {
        let _permit = crate::llm_util::acquire_embed(state, settings).await;
        match client.embed(&[document.to_string()]).await {
            Ok(mut v) if v.len() == 1 => v.remove(0),
            _ => return all.to_vec(),
        }
    };
    let near = match utopia_store::ontology::nearest_relation_types(
        &state.pool,
        kb_id,
        &vector,
        GLOSSARY_NEAREST,
        None,
    )
    .await
    {
        Ok(n) if !n.is_empty() => n,
        _ => return all.to_vec(),
    };
    let keep: std::collections::HashSet<&str> = near
        .iter()
        .map(|t| t.key.as_str())
        .chain(chosen.iter().map(|c| c.property.as_str()))
        .collect();
    all.iter()
        .filter(|p| keep.contains(p.key))
        .cloned()
        .collect()
}

/// A keep being applied does not count as "touching the graph"; retract, change and add being applied does
fn count(o: &mut DocumentOutcome, status: &str, verdict: &Verdict) {
    match status {
        "applied" if *verdict != Verdict::Keep => o.applied += 1,
        "held" => o.held += 1,
        "refused" => o.refused += 1,
        _ => {}
    }
}

/// All that the structure flagged (up to the cap), then the first few not flagged to fill the rest
fn choose(all: &[Candidate]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = all
        .iter()
        .filter(|c| c.flag.is_some())
        .take(FACTS_PER_DOCUMENT)
        .cloned()
        .collect();
    let room = FACTS_PER_DOCUMENT.saturating_sub(out.len()).min(SAMPLE);
    out.extend(all.iter().filter(|c| c.flag.is_none()).take(room).cloned());
    out
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
#[path = "errata_tests.rs"]
mod tests;
