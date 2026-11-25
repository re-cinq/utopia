//! The entry point for graph extraction tasks, plus the parts shared with the open write path (`extraction_open`).
//!
//! Extraction writes only to the open graph (0044 decision 2): every document, memory logs included, enters here via
//! `run_open`; the prompt has no ontology, and there is no second path that carries one. What is left here is what that path borrows:
//! rate-limited backoff for chat calls, the ceiling the source puts on confidence, snippet verification, the discard signal, the not-fully-extracted criterion,
//! and name-to-entity resolution (handles, same-name review pairs).
//! It is separate from the ingestion pipeline (two stages are available): once indexing completes, the document can be searched and asked about, while extraction runs slowly.
//! Ambiguous resolution cases only go into the review queue and trigger a separate batching adjudication task; LLM adjudication never blocks this task.

use crate::llm_util;
use crate::state::AppState;
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;
use utopia_core::models::Proposer;
use uuid::Uuid;

/// The maximum number of rate-limit backoff retries. **Only applies to 429**: a wrong key still fails after ten thousand retries.
const RATE_LIMIT_TRIES: u32 = 5;
/// Upper bound on a single backoff. The total wait is thus capped at just over two minutes, so an account whose quota is always full
/// fails outright rather than tying up the worker slot.
const RATE_LIMIT_CAP: Duration = Duration::from_secs(60);

/// Backoff jitter. **No `rand` crate**: all we need is "don't wake N chunks at the same moment", and a nanosecond clock
/// is enough to scatter them, while one more dependency would drag in its supply chain.
///
/// Takes half the interval (base/2 to base) rather than the full interval: backoff still grows monotonically, just staggered.
fn jitter(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let half = (base.as_millis() as u64) / 2;
    base / 2 + Duration::from_millis(if half == 0 { 0 } else { nanos % half })
}

/// Chat call for extraction, **backed off and retried on rate limits**. The caller sets the temperature (open extraction uses 0, see `extraction_open`).
///
/// The difference between rate limits and other failures is that they recover on their own, so the old phrase "skip the chunk" applied to them
/// turns a minute of waiting into a permanent data gap: measured, in one ingest of 1884 chunks,
/// 55 of 60 documents failed entirely, while the endpoint was healthy the whole time.
///
/// Two easy mistakes:
///
/// - **Do not hold the permit while backing off.** The permit covers only the real call and is returned before sleeping,
///   otherwise one waiting chunk would block another that could have gone through.
/// - **Most vendors don't send `Retry-After`**, so it is only "more accurate when present"; the criterion is the error type
///   itself; without it, exponential backoff applies.
pub(crate) async fn chat_retrying_rate_limits_at(
    state: &AppState,
    settings: &utopia_core::models::LlmSettings,
    client: &utopia_llm::LlmClient,
    messages: &[utopia_llm::ChatMessage],
    temperature: Option<f32>,
) -> anyhow::Result<utopia_llm::Reply> {
    let mut backoff = Duration::from_secs(2);
    for attempt in 1..=RATE_LIMIT_TRIES {
        // The permit covers only the call itself; it is returned as soon as the chunk leaves.
        //
        // **Streams**: the prompts on this path are long (extracting a whole chunk, aligning a batch of signatures); with reasoning on,
        // the model thinks for minutes before speaking, and without streaming those minutes look like total silence to the transport layer, so the read timeout
        // would kill a healthy call (measured: a chunk's first byte at 227 seconds, occasionally past 300 seconds). With streaming,
        // the reasoning itself is bytes, so the timeout only kills genuinely stuck requests. The return value is still the whole text
        let outcome = {
            let _permit = llm_util::acquire_chat(state, settings).await;
            client.chat_at_streaming(messages, temperature).await
        };
        let err = match outcome {
            Ok(reply) => return Ok(reply),
            Err(e) => e,
        };
        // The classes that recover on their own (rate limits, endpoint unavailable, request not delivered) back off and retry; everything else is thrown up as is
        let Some((what, retry_after)) = utopia_llm::transient(&err) else {
            return Err(err);
        };
        if attempt == RATE_LIMIT_TRIES {
            return Err(err.context(format!("{what} after {RATE_LIMIT_TRIES} attempts")));
        }
        let delay = jitter(retry_after.unwrap_or(backoff).min(RATE_LIMIT_CAP));
        tracing::warn!(
            attempt,
            delay_ms = delay.as_millis() as u64,
            from_header = retry_after.is_some(),
            why = what,
            "端点这次没答成，退避后重试"
        );
        tokio::time::sleep(delay).await;
        backoff = (backoff * 2).min(RATE_LIMIT_CAP);
    }
    unreachable!("循环内必定 return")
}

/// Facts the model derives from a chart description get their confidence pushed down to here (0040 decision 4): below the review's low-confidence threshold, so
/// they surface in the pending review. **"Never let it alone close a correct old value" is no longer enforced by this number**: after 0045 cut 3
/// the temporal engine reads the origin and start of a fact, so rows derived from a chart description still do not close, and a contradiction is logged for a person
/// (misreading one value in a bar chart is common, and no row in the store will say that this closing relied on a chart)
const DESCRIBED_CEILING: f32 = utopia_store::review::LOW_CONFIDENCE_BELOW - 0.05;

/// The cap this source of text puts on fact confidence
pub(crate) fn origin_ceiling(origin: &str, confidence: f32) -> f32 {
    if origin == "described" {
        confidence.min(DESCRIBED_CEILING)
    } else {
        confidence
    }
}

/// Whether a snippet is in the quote: case and whitespace do not matter
pub(crate) fn span_in_quote(span: &str, quote: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let (s, q) = (norm(span), norm(quote));
    !s.is_empty() && q.contains(&s)
}

/// Records a discard signal. The extractor has seven `continue`s, and each one is "a fact was extracted, got blocked,
/// and says nothing". A failure to write the signal must never take down extraction of a whole document, so the error is swallowed here.
pub(crate) async fn drop_signal(
    state: &AppState,
    kb_id: Uuid,
    document_id: Uuid,
    reason: &str,
    detail: &str,
    example: Option<&str>,
) {
    let _ = utopia_store::extraction_drops::record(
        &state.pool,
        kb_id,
        document_id,
        reason,
        detail,
        example,
    )
    .await;
}

/// Whether this round has finished extracting: if not, gives **the sentence to write into graph_error**.
///
/// The criterion is "everything extracted", not some ratio: any ratio is arbitrary, while here there is a criterion that needs no arbitrary choice:
/// finished means every chunk succeeded.
///
/// `attempted` is **the number of chunks taken this round**, not the document's total chunk count: a retry takes only
/// chunks with `extracted_at IS NULL`, so the second round's denominator is naturally smaller. The wording says "this round",
/// so readers don't think the document has only that many chunks.
pub(crate) fn incomplete_reason(unextracted: &[(i32, String)], attempted: usize) -> Option<String> {
    if unextracted.is_empty() {
        return None;
    }
    // Only the first three are named: the cause is often the same (if the vendor is unreachable, every chunk fails);
    // listing them all would just copy the same sentence twenty times
    let sample: Vec<String> = unextracted
        .iter()
        .take(3)
        .map(|(seq, why)| format!("#{seq} {why}"))
        .collect();
    let more = unextracted.len().saturating_sub(sample.len());
    let tail = if more > 0 {
        format!("; {more} more")
    } else {
        String::new()
    };
    Some(format!(
        "{} of {attempted} {} could not be extracted in this attempt: {}{tail}",
        unextracted.len(),
        if attempted == 1 { "chunk" } else { "chunks" },
        sample.join("; ")
    ))
}

/// `proposer`: if this document is a memory log, the extracted facts wait for a person's nod, and the record notes who said it:
/// the person, and, via MCP, the agent (the token of 0014). Documents in a bulk ingest pass the default: that path does not go through the pending-confirmation queue, so neither of those is used
/// The reason is stored with the status: an error that only goes to the log is no error at all
pub async fn extract_document(
    state: &AppState,
    document_id: Uuid,
    proposer: Proposer,
) -> anyhow::Result<()> {
    match run(state, document_id, proposer).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // The reason is stored with the status: an error that only goes to the log is no error at all
            let _ = utopia_store::documents::set_graph_failed(
                &state.pool,
                document_id,
                &format!("{e:#}"),
            )
            .await;
            if let Ok(doc) = utopia_store::documents::get(&state.pool, document_id).await {
                state.emit_document(doc.kb_id, document_id);
            }
            Err(e)
        }
    }
}

/// Resolve without the document's name cache. Handles use this path so two identities claimed
/// separately in one response cannot collapse before their fact refs are bound.
#[allow(clippy::too_many_arguments)]
async fn resolve_uncached(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Option<Uuid>,
    name: &str,
    ctx: Option<&[f32]>,
    name_vec: Option<&[f32]>,
    text: Option<&str>,
    exclude: &[Uuid],
    identity: bool,
    needs_adjudication: &mut bool,
) -> anyhow::Result<Uuid> {
    let r = match identity {
        true => {
            utopia_store::resolution::resolve_identity_name(
                pool, kb_id, type_id, name, ctx, text, exclude,
            )
            .await?
        }
        false => {
            utopia_store::resolution::resolve_mention(
                pool, kb_id, type_id, name, ctx, name_vec, text, exclude,
            )
            .await?
        }
    };
    // Suspected duplicate pairs (profile grey zone / type drift / same-name tie) enter the review queue. Most go to the batch adjudicator;
    // same-name ties (`ReviewStage::Human`) cannot tell who is who, so only a person can decide; they carry their own stage.
    for review in &r.reviews {
        utopia_store::resolution::create_review(
            pool,
            kb_id,
            r.entity_id,
            review.other_id,
            review.score,
            &review.reason,
            review.stage,
        )
        .await?;
        // Only pairs the batch can adjudicate trigger the adjudication task; pure human-review pairs should not wake the adjudicator
        if review.stage == utopia_store::resolution::ReviewStage::Adjudicating {
            *needs_adjudication = true;
        }
    }
    Ok(r.entity_id)
}

async fn create_namesake_reviews(
    pool: &PgPool,
    kb_id: Uuid,
    entity_id: Uuid,
    others: &[Uuid],
) -> anyhow::Result<bool> {
    let mut created = false;
    for other_id in others.iter().copied().filter(|id| *id != entity_id) {
        utopia_store::resolution::create_review(
            pool,
            kb_id,
            entity_id,
            other_id,
            1.0,
            "namesake",
            utopia_store::resolution::ReviewStage::Human,
        )
        .await?;
        created = true;
    }
    Ok(created)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_handle(
    pool: &PgPool,
    kb_id: Uuid,
    type_id: Option<Uuid>,
    name: &str,
    ctx: Option<&[f32]>,
    name_vec: Option<&[f32]>,
    text: Option<&str>,
    identity: bool,
    response_claims: &mut HashMap<String, Vec<Uuid>>,
    handled_by_name: &mut HashMap<String, Vec<Uuid>>,
    ambiguous_bare_cache: &mut HashMap<String, Uuid>,
    needs_adjudication: &mut bool,
    human_reviews_found: &mut bool,
) -> anyhow::Result<Uuid> {
    let normalized = utopia_store::resolution::name_key(name);
    let response_excluded = response_claims
        .get(&normalized)
        .cloned()
        .unwrap_or_default();
    let document_excluded = handled_by_name
        .get(&normalized)
        .filter(|ids| ids.len() > 1)
        .cloned()
        .unwrap_or_default();
    let mut excluded = response_excluded.clone();
    for id in &document_excluded {
        if !excluded.contains(id) {
            excluded.push(*id);
        }
    }
    // A later response may allocate a fresh e-handle for an otherwise bare ambiguous name
    // instead of choosing one of the supplied k-handles. That must still become/reuse C; a new
    // response-local spelling is not permission to guess A or B.
    let reuse_document_provisional = response_excluded.is_empty() && document_excluded.len() > 1;
    let id = match reuse_document_provisional
        .then(|| ambiguous_bare_cache.get(&normalized).copied())
        .flatten()
    {
        Some(id) => id,
        None => {
            let id = resolve_uncached(
                pool,
                kb_id,
                type_id,
                name,
                ctx,
                name_vec,
                text,
                &excluded,
                identity,
                needs_adjudication,
            )
            .await?;
            if reuse_document_provisional {
                ambiguous_bare_cache.insert(normalized.clone(), id);
            }
            id
        }
    };
    if create_namesake_reviews(pool, kb_id, id, &excluded).await? {
        *human_reviews_found = true;
    }
    response_claims
        .entry(normalized.clone())
        .or_default()
        .push(id);
    let document_claims = handled_by_name.entry(normalized).or_default();
    if !document_claims.contains(&id) {
        document_claims.push(id);
    }
    Ok(id)
}

/// Every document goes through the open graph (0044 decision 2): it writes only the document's own words, and the ontology does not enter the prompt.
/// Memory logs also enter here; only their statements first go to the pending-confirmation table to wait for a person's nod (0015), and become open statements only when approved
async fn run(state: &AppState, document_id: Uuid, proposer: Proposer) -> anyhow::Result<()> {
    let doc = utopia_store::documents::get(&state.pool, document_id).await?;
    // Deleted after being queued (#268): tombstones are not extracted, since the extracted facts would live on a deleted source
    if doc.deleted_at.is_some() {
        tracing::info!(document = %document_id, "skipping a deleted document");
        return Ok(());
    }
    let kb = utopia_store::kbs::get(&state.pool, doc.kb_id).await?;
    // Pushed statements (0054): the chunk is the contract, extraction resolves against the contract and does not ask the model, so it runs even without a chat model configured
    let source = match doc.source_id {
        Some(id) => Some(utopia_store::sources::get(&state.pool, id).await?),
        None => None,
    };
    let pushed = source.as_ref().is_some_and(|s| s.kind == "statements");
    let identity = source
        .as_ref()
        .is_some_and(utopia_core::models::Source::names_are_identities);
    // Pass settings if present: the push path does not ask the chat model, but the name-vector embedding model (#877) still comes from it
    let settings = utopia_store::settings::get(&state.pool, kb.workspace_id).await?;
    let client = if pushed {
        None
    } else {
        let settings = settings
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot extract"))?;
        Some(
            llm_util::chat_client(settings)
                .ok_or_else(|| anyhow::anyhow!("Chat model not configured; cannot extract"))?,
        )
    };

    // Ownership token: re-extraction bumps the epoch, and the task detects it has been taken over (see the chunk loop in `run_open`)
    let my_epoch = utopia_store::documents::extract_epoch(&state.pool, document_id).await?;
    utopia_store::documents::set_graph_status(&state.pool, document_id, "extracting").await?;
    state.emit_document(doc.kb_id, document_id);
    let await_nod = utopia_store::memory::is_memory_document(&state.pool, document_id).await?;
    let result = crate::extraction_open::run_open(
        state,
        &doc,
        &kb,
        settings.as_ref(),
        client.as_ref(),
        my_epoch,
        proposer,
        await_nod,
        pushed,
        identity,
    )
    .await;
    // Between multiple writes after the epoch check, a rebuild can still happen; an old task that has been taken over should not retry or report failure.
    // Only drop the error when a new epoch is explicitly read; if the query itself fails, keep the original error.
    if result.is_err()
        && utopia_store::documents::extract_epoch(&state.pool, document_id)
            .await
            .is_ok_and(|epoch| epoch != my_epoch)
    {
        tracing::info!(document = %document_id, "discarding an extraction error after its job was superseded");
        return Ok(());
    }
    result
}

#[cfg(test)]
mod origin_ceiling_tests {
    use super::*;

    #[test]
    fn a_described_fact_cannot_close_a_value_by_itself() {
        let ceiling = origin_ceiling("described", 0.95);
        assert!(ceiling < utopia_store::review::LOW_CONFIDENCE_BELOW);
        assert_eq!(ceiling, DESCRIBED_CEILING, "it still enters the graph");
        assert_eq!(origin_ceiling("described", 0.62), 0.62);
        for origin in ["stated", "ocr", "transcribed"] {
            assert_eq!(origin_ceiling(origin, 0.95), 0.95);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{incomplete_reason, resolve_handle, span_in_quote};
    use std::collections::HashMap;
    use uuid::Uuid;

    #[test]
    fn a_span_is_found_in_its_quote_regardless_of_case_and_spacing() {
        assert!(span_in_quote(
            "former openai   personnel",
            "Former OpenAI personnel have founded"
        ));
        assert!(!span_in_quote(
            "OpenAI staff",
            "Former OpenAI personnel have founded"
        ));
        assert!(!span_in_quote("", "anything"));
    }

    /// The reason this criterion exists: a network blip left only 12 of 60 chunks of six documents extracted,
    /// and all six **showed "extraction complete"**, with 80% of the content never reaching the graph, and nothing on screen said so.
    #[test]
    fn a_document_with_a_skipped_chunk_is_not_complete() {
        assert_eq!(incomplete_reason(&[], 23), None, "全抽完才算完成");
        let one = [(7, "request failed: timeout".to_string())];
        let msg = incomplete_reason(&one, 23).expect("有块没抽成就不该算完成");
        assert!(msg.contains("23"), "分母要说出来：{msg}");
        assert!(msg.contains("#7"), "得指得出是哪一块：{msg}");
    }

    /// The cause is often the same (if the vendor is unreachable, every chunk fails); naming three is enough,
    /// but **how many remain must be said**, or the reader would think only three chunks were broken.
    #[test]
    fn many_failures_are_summarised_without_hiding_the_count() {
        let many: Vec<(i32, String)> = (1..=20).map(|i| (i, "request failed".into())).collect();
        let msg = incomplete_reason(&many, 60).unwrap();
        assert!(msg.contains("20"), "总数要在：{msg}");
        assert!(msg.contains("; 17 more"), "省略掉的数量要说出来：{msg}");
        assert_eq!(msg.matches("request failed").count(), 3, "只举三个");
    }

    /// On retry, `chunks_for_extraction` takes only the chunks not yet extracted, so the denominator is **this round's** count,
    /// not the document's total chunk count. The wording must make that clear, so people don't think the document has only that many chunks.
    #[test]
    fn the_denominator_is_this_rounds_chunks_not_the_document() {
        let msg = incomplete_reason(&[(2, "x".into())], 3).unwrap();
        assert!(
            msg.starts_with("1 of 3 chunks could not be extracted in this attempt:"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn namesake_handles_keep_fact_attribution_and_a_fresh_handle_gets_c() -> anyhow::Result<()>
    {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(());
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'handle-runtime-test')")
            .bind(org)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'handle-runtime-test')",
        )
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'handle-runtime-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(&pool)
        .await?;
        let person = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'person', 'Person')",
        )
        .bind(person)
        .bind(kb)
        .execute(&pool)
        .await?;

        let run = async {
            let mut response_claims = HashMap::new();
            let mut document_claims = HashMap::new();
            let mut bare_cache = HashMap::new();
            let (mut needs_adjudication, mut human_reviews) = (false, false);
            let a = resolve_handle(
                &pool,
                kb,
                Some(person),
                "Zhang Wei",
                None,
                None,
                None,
                false,
                &mut response_claims,
                &mut document_claims,
                &mut bare_cache,
                &mut needs_adjudication,
                &mut human_reviews)
            .await?;
            let b = resolve_handle(
                &pool,
                kb,
                Some(person),
                "Zhang Wei",
                None,
                None,
                None,
                false,
                &mut response_claims,
                &mut document_claims,
                &mut bare_cache,
                &mut needs_adjudication,
                &mut human_reviews)
            .await?;
            assert_ne!(a, b);
            assert!(human_reviews);
            assert!(
                !needs_adjudication,
                "namesakes must not wake the LLM worker"
            );

            // Two handles in the same reply each bind their own entity: facts follow the handle, not the name
            let refs = HashMap::from([("e1", a), ("e2", b)]);
            let (finance, platform) = (Uuid::now_v7(), Uuid::now_v7());
            let works_at = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'works_at', 'works at')",
            )
            .bind(works_at)
            .bind(kb)
            .execute(&pool)
            .await?;
            for (id, name) in [(finance, "Finance"), (platform, "Platform Engineering")] {
                sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, $3)")
                    .bind(id)
                    .bind(kb)
                    .bind(name)
                    .execute(&pool)
                    .await?;
            }
            for (handle, object) in [("e1", finance), ("e2", platform)] {
                let subject = refs[handle];
                utopia_store::graph::insert_fact(
                    &pool,
                    kb,
                    subject,
                    Some(works_at),
                    object,
                    utopia_store::graph::Validity::default(),
                    0.9,
                )
                .await?;
            }
            let pairs: Vec<(Uuid, Uuid)> = sqlx::query_as(
                // Edges only: entities also carry name facts (0041), and those without an object entity
                "SELECT subject_id, object_id FROM facts
                  WHERE kb_id = $1 AND object_id IS NOT NULL ORDER BY subject_id",
            )
            .bind(kb)
            .fetch_all(&pool)
            .await?;
            assert!(pairs.contains(&(a, finance)));
            assert!(pairs.contains(&(b, platform)));
            assert!(!pairs.contains(&(a, platform)));
            assert!(!pairs.contains(&(b, finance)));
            utopia_store::resolution::refresh_disambiguators(&pool, kb, "Zhang Wei").await?;
            let labels: Vec<(Uuid, Option<String>)> = sqlx::query_as(
                "SELECT id, disambiguator FROM entities WHERE id = ANY($1) ORDER BY id",
            )
            .bind(vec![a, b])
            .fetch_all(&pool)
            .await?;
            assert!(labels.contains(&(a, Some("Finance".to_string()))));
            assert!(labels.contains(&(b, Some("Platform Engineering".to_string()))));

            // A later reply opened a new handle for the same name, without picking A or B: the text gives no basis for picking,
            // so it must not guess. The fact lands on a document-level C, with one manual review pair each against A and against B
            let mut later_response_claims = HashMap::new();
            let c = resolve_handle(
                &pool,
                kb,
                Some(person),
                "**Zhang Wei**",
                None,
                None,
                None,
                false,
                &mut later_response_claims,
                &mut document_claims,
                &mut bare_cache,
                &mut needs_adjudication,
                &mut human_reviews)
            .await?;
            assert_ne!(c, a);
            assert_ne!(c, b);
            utopia_store::graph::insert_fact(
                &pool,
                kb,
                c,
                Some(works_at),
                finance,
                utopia_store::graph::Validity::default(),
                0.9,
            )
            .await?;
            // A reply after that opens yet another handle, still that same C: a new spelling is no license to guess A or B
            let mut another_response_claims = HashMap::new();
            let c_again = resolve_handle(
                &pool,
                kb,
                None,
                "Zhang Wei",
                None,
                None,
                None,
                false,
                &mut another_response_claims,
                &mut document_claims,
                &mut bare_cache,
                &mut needs_adjudication,
                &mut human_reviews)
            .await?;
            assert_eq!(
                c_again, c,
                "a later response cannot evade A/B ambiguity by inventing a new e-handle"
            );
            utopia_store::graph::insert_fact(
                &pool,
                kb,
                c_again,
                Some(works_at),
                platform,
                utopia_store::graph::Validity::default(),
                0.9,
            )
            .await?;
            let c_objects: Vec<Uuid> = sqlx::query_scalar(
                "SELECT object_id FROM facts
                  WHERE kb_id = $1 AND subject_id = $2 AND object_id IS NOT NULL ORDER BY object_id",
            )
            .bind(kb)
            .bind(c)
            .fetch_all(&pool)
            .await?;
            assert_eq!(c_objects.len(), 2);
            assert!(c_objects.contains(&finance));
            assert!(c_objects.contains(&platform));

            let reviews = utopia_store::resolution::list_reviews(
                &pool,
                kb,
                utopia_store::resolution::TypeFilter::Any,
                10,
                0,
            )
            .await?;
            assert_eq!(reviews.len(), 3, "A/B, C/A and C/B");
            assert!(reviews.iter().all(|review| review.stage == "human"));
            assert!(
                utopia_store::resolution::pending_adjudications(&pool, kb, 10)
                    .await?
                    .is_empty()
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org)
            .execute(&pool)
            .await;
        run
    }

    /// #270: same-name ties across documents, decided by profile similarity rather than handles. The store already has two "张伟" whose profile centroids
    /// are identical (seeded from the same chunk). A new mention scores both the same. The old path, when the top score ≥ SIM_ATTACH,
    /// silently attaches to whichever was met first; with the fix, the handle path must instead create a third entity, two **manual** review pairs,
    /// and never wake the LLM adjudicator (otherwise two nearly identical profiles would be auto-merged, which is exactly what must be prevented).
    #[tokio::test]
    async fn a_profile_tie_across_documents_files_human_reviews_not_an_attach() -> anyhow::Result<()>
    {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(());
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'profile-tie-test')")
            .bind(org)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'profile-tie-test')",
        )
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'profile-tie-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(&pool)
        .await?;
        let person = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'person', 'Person')",
        )
        .bind(person)
        .bind(kb)
        .execute(&pool)
        .await?;

        let run = async {
            // Two people with the same name, with identical profile vectors
            let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
            for id in [a, b] {
                sqlx::query(
                    "INSERT INTO entities
                        (id, kb_id, type_id, canonical_name, profile_embedding, profile_n)
                     VALUES ($1, $2, $3, 'Zhang Wei', '[1,0,0]'::vector, 1)",
                )
                .bind(id)
                .bind(kb)
                .bind(person)
                .execute(&pool)
                .await?;
            }

            // A context matching both people's profiles: the cosine scored is the same, so they cannot be told apart
            let ctx: Vec<f32> = vec![1.0, 0.0, 0.0];
            let mut response_claims = HashMap::new();
            let mut document_claims = HashMap::new();
            let mut bare_cache = HashMap::new();
            let (mut needs_adjudication, mut human_reviews) = (false, false);
            let c = resolve_handle(
                &pool,
                kb,
                Some(person),
                "Zhang Wei",
                Some(&ctx),
                None,
                None,
                false,
                &mut response_claims,
                &mut document_claims,
                &mut bare_cache,
                &mut needs_adjudication,
                &mut human_reviews,
            )
            .await?;

            assert_ne!(c, a, "画像并列不该 attach 到 A——那是候选顺序掷出的硬币");
            assert_ne!(c, b, "画像并列不该 attach 到 B——那是候选顺序掷出的硬币");
            assert!(
                !needs_adjudication,
                "同名并列只能等人裁，绝不该唤醒 LLM 裁决器"
            );

            let reviews = utopia_store::resolution::list_reviews(
                &pool,
                kb,
                utopia_store::resolution::TypeFilter::Any,
                10,
                0,
            )
            .await?;
            assert_eq!(reviews.len(), 2, "对 A、对 B 各一条审核对");
            assert!(
                reviews.iter().all(|review| review.stage == "human"),
                "同名并列的审核对必须是人工阶段"
            );
            assert!(
                utopia_store::resolution::pending_adjudications(&pool, kb, 10)
                    .await?
                    .is_empty(),
                "人工审核对绝不能落进批量裁决队列，否则又会被自动并掉"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org)
            .execute(&pool)
            .await;
        run
    }
}
