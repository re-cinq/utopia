//! Ingestion pipeline: parse → chunk → full-text index → embedding (optional) → ready.
//! Each step is idempotent: a re-run first clears the old chunks and old index entries.

use crate::llm_util;
use crate::state::AppState;
use futures_util::{stream, StreamExt};
use utopia_core::models::{LlmSettings, Proposer};
use utopia_llm::LlmClient;
use utopia_store::documents::{self, ProcessingInput};
use uuid::Uuid;

/// Whether this document's source should be extracted. Documents with no source (direct uploads, memory fragments) are still extracted as before.
/// The kind of source the document belongs to; None for no source (upload). Extraction asks the same question (0054)
pub(crate) async fn source_kind(
    state: &AppState,
    source_id: Option<Uuid>,
) -> anyhow::Result<Option<String>> {
    let Some(id) = source_id else {
        return Ok(None);
    };
    Ok(Some(
        utopia_store::sources::get(&state.pool, id).await?.kind,
    ))
}

async fn source_extracts(state: &AppState, source_id: Option<Uuid>) -> anyhow::Result<bool> {
    let Some(id) = source_id else {
        return Ok(true);
    };
    Ok(utopia_store::sources::get(&state.pool, id)
        .await?
        .extracts())
}

/// How many items to send to embedding each time.
///
/// Unlike ontology_index's 64, this has not been measured, so leave it alone: that code's comment says batch size has to be measured with real text;
/// chunk text is much longer than class labels, so 16 may not be wrong. Do not turn the concurrency and batch-size knobs in one go, or the result cannot be read.
const EMBED_BATCH: usize = 16;

/// Cap on embedding batches in flight at once (#513).
///
/// The embedding model's concurrency gate (`model_concurrency`, default 10) is shared across the deployment: ontology filling
/// (ontology_index, cap 4), type resolution and per-question query embeddings all go through it. Ingestion is foreground work the user waits to see,
/// so it has a stronger claim on the gate than background filling; but it is also the path that can bring in thousands of chunks at once, and without a cap
/// it starves filling, resolution and retrieval alike. ontology_index recorded an incident where one `join_all` held the gate and five documents
/// sat in embedding for fifteen minutes. 4 is the same as filling: both paths together total 8, leaving the gate 2 for others.
const EMBED_JOBS: usize = 4;

pub async fn process_document(state: &AppState, document_id: Uuid) -> anyhow::Result<()> {
    let input = documents::processing_input(&state.pool, document_id).await?;
    let doc = &input.document;
    match run(state, &input).await {
        Ok(()) => Ok(()),
        // Waiting for the service that reads the characters to finish: the document stays as parsing, and the task comes back to ask later
        Err(e) if utopia_core::is_deferred(&e).is_some() => {
            if documents::processing_is_current(&state.pool, &input).await? {
                Err(e)
            } else {
                Ok(())
            }
        }
        Err(e) => {
            // The characters must be read by a model, and that kind of model is not configured (0040): **degrade**. The file is kept, the document stays failed,
            // and which kind is missing is recorded, with one library-level alert raised; configuring a model reprocesses it by itself. Retrying does not help,
            // since a model will not get configured on its own within two minutes, so it is parked as Terminal.
            // A transcript that comes back without speaker labels is treated as if none were configured (decision 5): try again with a model that labels speakers
            let waiting = e
                .downcast_ref::<utopia_ingest::NeedsReader>()
                .map(|n| n.reader)
                .or_else(|| {
                    e.downcast_ref::<utopia_ingest::NoSpeakers>()
                        .map(|_| utopia_ingest::Reader::Transcribe)
                });
            if !documents::set_failed_if_current(
                &state.pool,
                &input,
                &e.to_string(),
                waiting.map(|reader| reader.as_str()),
            )
            .await?
            {
                // A stale model result can be discarded, but a database failure must still be visible to the queue.
                if e.is::<sqlx::Error>()
                    || matches!(
                        e.downcast_ref::<utopia_core::AppError>(),
                        Some(utopia_core::AppError::Db(_))
                    )
                {
                    return Err(e);
                }
                tracing::info!(%document_id, "discarding a superseded document failure");
                return Ok(());
            }
            if let Some(reader) = waiting {
                crate::alerting::observe_document_needs_reader(
                    state,
                    doc.kb_id,
                    document_id,
                    &doc.filename,
                    reader,
                    &e.to_string(),
                )
                .await;
                state.emit_document(doc.kb_id, document_id);
                return Err(e.context(utopia_core::Terminal));
            }
            state.emit_document(doc.kb_id, document_id);
            // Formats that cannot be read (video, binary, empty files) fail the same however many times they are tried
            if e.downcast_ref::<utopia_ingest::Unreadable>().is_some() {
                return Err(e.context(utopia_core::Terminal));
            }
            Err(e)
        }
    }
}

async fn run(state: &AppState, input: &ProcessingInput) -> anyhow::Result<()> {
    let doc = &input.document;
    let document_id = doc.id;
    // Deleted after being queued (#268): a tombstone does not rebuild chunks or go back into the index; what was purged no longer even has its raw file
    if doc.deleted_at.is_some() {
        tracing::info!(document = %document_id, "skipping a deleted document");
        return Ok(());
    }

    // 1. Parse (CPU-heavy, run on a blocking thread)
    if !documents::set_status_if_current(&state.pool, input, "parsing").await? {
        return Ok(());
    }
    state.emit_document(doc.kb_id, document_id);
    let bytes = state.blob.get(&doc.sha256).await?;
    let filename = doc.filename.clone();
    let (parsed, bytes) =
        tokio::task::spawn_blocking(move || (utopia_ingest::parse(&filename, &bytes), bytes))
            .await?;
    let kb_row = utopia_store::kbs::get(&state.pool, doc.kb_id).await?;
    let settings = utopia_store::settings::get(&state.pool, kb_row.workspace_id).await?;
    let pushed_statements =
        source_kind(state, doc.source_id).await?.as_deref() == Some("statements");

    // 2. Chunk and store
    let (text, pieces, warnings) = match parsed {
        Ok(parsed) => {
            let warnings = parsed.warnings;
            // Parsed body text may contain NULs (common in PDF text layers); strip them before storing (#611), which the memory
            // path shares via `utopia_core::without_nul` (#665). The stripping must happen before lengths are computed and before chunking: from then on
            // text_len, chunk offsets, the full-text index and embedding all read this one copy, so they agree with each other
            let text = utopia_core::without_nul(&parsed.text).into_owned();
            // Statements pushed (0054): the payload is the contract, and the whole thing is one chunk. The chunk budget is set for the model's attention,
            // and this path has no model reading it; splitting it would make the contract impossible to parse back
            let pieces = if pushed_statements {
                vec![utopia_ingest::ChunkPiece {
                    seq: 0,
                    char_start: 0,
                    char_end: text.chars().count() as i32,
                    heading: None,
                    provenance: utopia_ingest::Provenance::stated(),
                    text: text.clone(),
                }]
            } else {
                utopia_ingest::chunk_with_budget(&text, state.chunk_tokens)
            };
            (text, pieces, warnings)
        }
        // Scanned pages and images without a text layer: if the workspace has a layout-recognition service configured, hand it over (0040 second slice),
        // chunk by page with the page number and box recorded. Recordings go to the speaker-labelling transcription model (third slice),
        // each chunk recording its start and end times and speaker. If none is configured, the error is raised as before, and the outside degrades it
        Err(e) => {
            let Some(needs) = e.downcast_ref::<utopia_ingest::NeedsReader>() else {
                return Err(e);
            };
            let reading = match (needs.reader, settings.as_ref()) {
                (utopia_ingest::Reader::Ocr, Some(s)) => {
                    match crate::readers::Ocr::from_settings(s) {
                        Some(ocr) => ocr.read(state, doc, bytes).await?,
                        None => return Err(e),
                    }
                }
                (utopia_ingest::Reader::Transcribe, Some(s)) => {
                    match crate::readers::Transcriber::from_settings(s) {
                        Some(t) => t.read(doc, bytes).await?,
                        None => return Err(e),
                    }
                }
                (_, None) => return Err(e),
            };
            if reading.text.trim().is_empty() {
                return Err(utopia_ingest::Unreadable(
                    "The reader found no text in this file".into(),
                )
                .into());
            }
            let pieces = reading.chunk(state.chunk_tokens);
            (reading.text, pieces, Vec::new())
        }
    };
    let text_len = text.chars().count() as i32;
    let Some(chunk_pairs) =
        documents::replace_chunks_if_current(&state.pool, input, &pieces).await?
    else {
        // The source document may have been updated or deleted while it was being read: discard the stale result,
        // and do not rewrite the index, status and extraction queue of a newer task
        tracing::info!(%document_id, "discarding a superseded document read");
        return Ok(());
    };
    let chunk_count = chunk_pairs.len() as i32;
    // Only the current version gets an alert: if this read was superseded, the warning is stale.
    if !warnings.is_empty() {
        crate::alerting::observe_document_truncated(
            state,
            doc.kb_id,
            document_id,
            &doc.filename,
            &warnings,
        )
        .await;
    }

    // 3. Full-text index (Tantivy)
    if !documents::set_status_if_current(&state.pool, input, "indexing").await? {
        return Ok(());
    }
    state.emit_document(doc.kb_id, document_id);
    let search = state.search.clone();
    let kb = doc.kb_id.to_string();
    let did = document_id.to_string();
    tokio::task::spawn_blocking(move || search.reindex_document(&kb, &did, &chunk_pairs)).await??;

    // 4. Embedding (done only when the workspace has an embedding model configured; with none it is still ready, and BM25 search is available)
    // A statements single chunk keeps the full JSON contract, which may exceed the embedding model's input limit;
    // deterministic extraction should not be blocked by that (#1375)
    if !pushed_statements {
        if let Some((settings, client)) = embedder(settings.as_ref()) {
            if !documents::set_status_if_current(&state.pool, input, "embedding").await? {
                return Ok(());
            }
            state.emit_document(doc.kb_id, document_id);
            embed_pending(state, settings, &client, input).await?;
        }
    }

    // Where the source says not to extract, stop here: searchable and askable, but not in the graph
    //
    // Schema documents are this kind (0035 decision 7): they are corpus for data questions to look up table structure, and extraction on them
    // would have the extractor treat each column name as an entity (on a wide-table corpus, 28 of 40 concept entities were column names, #553).
    // Status is recorded as `skipped` rather than left as `none`, since `none` reads in the Library as
    // "not yet reached", and it never would be
    // Two stages: once the index is ready, if a dialogue model is configured, queue graph extraction (it does not block search and asking).
    // Statements pushed here do not ask the model (0054), so they are queued even without one
    let graph_status = if !source_extracts(state, doc.source_id).await? {
        Some("skipped")
    } else if pushed_statements || settings.as_ref().is_some_and(|s| s.chat_ready()) {
        Some("queued")
    } else {
        None
    };
    if !documents::finish_processing_if_current(
        &state.pool,
        input,
        text_len,
        chunk_count,
        graph_status,
    )
    .await?
    {
        tracing::info!(%document_id, "discarding a superseded document completion");
        return Ok(());
    }
    state.emit_document(doc.kb_id, document_id);

    tracing::info!(%document_id, chunks = chunk_count, "document processing finished");
    Ok(())
}

/// Memory ingestion (the second half of the fast path for episodes): new episode chunks get embeddings filled,
/// the full-text index is rebuilt, and incremental extraction is triggered (only new chunks with empty extracted_at get extracted).
/// No parsing or chunking: an episode is already a chunk when stored.
///
/// `proposer`: the person who said this sentence, and, when via MCP, the agent. It is carried all the way to extraction and lands in
/// `pending_facts.proposed_by` / `proposed_token` (0015, 0026)
pub async fn memory_ingest(
    state: &AppState,
    document_id: Uuid,
    proposer: Proposer,
) -> anyhow::Result<()> {
    let doc = utopia_store::documents::get(&state.pool, document_id).await?;
    if doc.deleted_at.is_some() {
        tracing::info!(document = %document_id, "skipping a deleted document");
        return Ok(());
    }
    let kb_row = utopia_store::kbs::get(&state.pool, doc.kb_id).await?;
    let settings = utopia_store::settings::get(&state.pool, kb_row.workspace_id).await?;

    // Embed one sentence at a time: a sentence that cannot be embedded is left without a vector and everything else proceeds. The log is a document that keeps growing;
    // previously all chunks lacking vectors were embedded together and one error failed the whole task, so a sentence that could not be embedded blocked every
    // new memory from then on: the index was not rebuilt, extraction not queued, and `remember` still answered "recorded" (#1187)
    if let Some((settings, client)) = embedder(settings.as_ref()) {
        embed_each(state, settings, &client, document_id).await?;
    }

    let chunks = utopia_store::documents::chunks_full(&state.pool, document_id).await?;
    let pairs: Vec<(String, String)> = chunks
        .iter()
        .map(|c| (c.id.to_string(), c.text.clone()))
        .collect();
    let search = state.search.clone();
    let kb = doc.kb_id.to_string();
    let did = document_id.to_string();
    tokio::task::spawn_blocking(move || search.reindex_document(&kb, &did, &pairs)).await??;

    if settings.as_ref().is_some_and(|s| s.chat_ready()) {
        utopia_store::documents::set_graph_status(&state.pool, document_id, "queued").await?;
        utopia_store::jobs::enqueue_extraction(
            &state.pool,
            serde_json::json!({
                "document_id": document_id,
                "proposed_by": proposer.user_id,
                "proposed_token": proposer.token_id,
            }),
            3,
        )
        .await?;
    }
    state.emit_document(doc.kb_id, document_id);
    Ok(())
}

/// Embedding for the memory log: one request per chunk; a failure only logs and is left to retry next time; returns how many were embedded.
/// The opposite trade-off from [`embed_pending`]: there a misaligned batch would write vectors onto other chunks, so the whole document fails;
/// here it is one at a time, with no misalignment possible, and one failure should not drag down the later ones
async fn embed_each(
    state: &AppState,
    settings: &LlmSettings,
    client: &LlmClient,
    document_id: Uuid,
) -> anyhow::Result<usize> {
    let pending =
        utopia_store::documents::chunks_pending_embedding(&state.pool, document_id).await?;
    let mut done = 0;
    for (chunk_id, text) in pending {
        let embedded = {
            let _permit = llm_util::acquire_embed(state, settings).await;
            client.embed(std::slice::from_ref(&text)).await
        };
        match embedded {
            Ok(mut vectors) if vectors.len() == 1 => {
                let vector = vectors.remove(0);
                utopia_store::documents::set_embeddings(&state.pool, &[(chunk_id, vector)]).await?;
                done += 1;
            }
            Ok(vectors) => {
                tracing::warn!(%document_id, %chunk_id, got = vectors.len(), "memory vector count mismatch, this sentence is not embedded for now");
            }
            Err(e) => {
                tracing::warn!(%document_id, %chunk_id, error = %e, "this memory sentence was not embedded, the rest proceed as usual");
            }
        }
    }
    Ok(done)
}

/// A client exists only when the workspace has an embedding model configured; settings and the client are handed out together, since the gate permit must be taken from the settings.
fn embedder(settings: Option<&LlmSettings>) -> Option<(&LlmSettings, LlmClient)> {
    let s = settings?;
    llm_util::embed_client(s).map(|c| (s, c))
}

/// Embed all of this document's chunks that still lack a vector; returns how many were embedded. Shared by document ingestion and memory ingestion:
/// previously each of the two places copied the same loop, and each waited for a batch to finish before sending the next.
///
/// Batches do not depend on each other (vectors are paired by position, only within one batch); `EMBED_JOBS` batches are in flight at once,
/// each holding a gate permit only until it has written its own.
///
/// **If the count does not match, abandon the whole batch.** Pairing is by position: one missing item misaligns everything and writes one item's vector onto another;
/// once stored this is never visible again: the text is still there, and the vector belongs to someone else.
///
/// **If any batch fails, the whole document fails.** When serial, the error unwinds naturally; under concurrency it must be passed out explicitly, so that the caller
/// marks the document failed rather than leaving it stuck in embedding. Vectors already written stay; a re-run only fills the gaps
/// (`chunks_pending_embedding` picks only those with empty embeddings).
async fn embed_pending(
    state: &AppState,
    settings: &LlmSettings,
    client: &LlmClient,
    input: &ProcessingInput,
) -> anyhow::Result<usize> {
    let pending = documents::chunks_pending_embedding(&state.pool, input.document.id).await?;
    // Batches are first turned into owned data: borrowed slices captured into a concurrently driven future cannot cross the Send boundary
    //(the same point as in ontology_index)
    let batches: Vec<Vec<(Uuid, String)>> =
        pending.chunks(EMBED_BATCH).map(<[_]>::to_vec).collect();
    let mut batches = stream::iter(batches)
        .map(|batch| async move {
            let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
            let _permit = llm_util::acquire_embed(state, settings).await;
            let embeddings = client.embed(&texts).await?;
            if embeddings.len() != batch.len() {
                anyhow::bail!(
                    "嵌入返回 {} 条，送去的是 {} 条",
                    embeddings.len(),
                    batch.len()
                );
            }
            let items: Vec<(Uuid, Vec<f32>)> =
                batch.iter().map(|(id, _)| *id).zip(embeddings).collect();
            if !documents::set_embeddings_if_current(&state.pool, input, &items).await? {
                return Ok(0);
            }
            Ok::<usize, anyhow::Error>(items.len())
        })
        .buffer_unordered(EMBED_JOBS);
    let mut done = 0;
    while let Some(written) = batches.next().await {
        done += written?;
    }
    Ok(done)
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod pipeline_tests;
