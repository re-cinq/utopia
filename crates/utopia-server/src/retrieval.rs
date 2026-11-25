//! Hybrid retrieval: BM25 (Tantivy) + vectors (pgvector), fused by RRF.
//! When embedding is not configured or the request fails, it silently degrades to pure BM25.
//!
//! **`as_of` is complete only for the vector and chunk-fetch steps** (0019 open question ②). The Tantivy index
//! holds only one version, "now": re-parsing replaces a document's chunks in one batch, and old versions are not in the index. So
//! when retrieving at a moment, what BM25 finds back **is correct** (then filtered by the liveness at that time), but it
//! cannot find the chunks that existed at that time and have since been pushed out: recall is missing a corner, and hits are never wrong.
//! Filling that corner means versioning the full-text index too, which is a separate job and not part of this change.
//!
//! **The two channels run in parallel, with BM25 on the blocking thread pool** (#515). `SearchIndex::search` is synchronous:
//! tokenising, walking mmap segments, fetching documents one by one, it holds a runtime thread for as long as it runs. The other six
//! Tantivy calls in the repo are wrapped in `spawn_blocking`, and this is the hottest one, run on every question. Previously it was not wrapped, and
//! on a two-core machine two people asking at once would stall SSE and health checks together. The two channels used to be chained in series: BM25 finished
//! before calling the embedding endpoint, and only after the embedding came back was the vector queried. Yet embedding is a network round-trip and the most expensive item in the function,
//! and it has no dependency on BM25. Running them together, latency is the larger of the two rather than their sum.

use crate::llm_util;
use crate::state::AppState;
use serde::Serialize;
use std::collections::HashMap;
use utopia_core::models::{ChunkView, SourcedChunk};
use utopia_core::AppResult;
use uuid::Uuid;

const RECALL_PER_CHANNEL: usize = 24;

/// How deep paging goes, to which result (#1271): any deeper and retrieval is being used as an export
pub const MAX_DEPTH: usize = 100;

pub struct SearchOpts {
    pub top_k: usize,
    pub offset: usize,
    pub as_of: Option<chrono::DateTime<chrono::Utc>>,
    pub filter: Option<SourceFilter>,
}

/// Retrieve only documents from one source; if `key_prefix` is given, only those whose `external_key` starts with it (#1271).
/// Each channel filters **within** recall: filtering after fusion means other hits in the library have already filled each channel's quota, so the page comes out short
pub struct SourceFilter {
    pub source_id: Uuid,
    pub key_prefix: Option<String>,
}

/// One hit together with its score (#1271). `score` is the RRF total, which only orders results within the same query;
/// it is not comparable across queries. Ranks count from 1; None means the hit did not appear in that channel
#[derive(Debug, Serialize)]
pub struct ScoredChunk {
    #[serde(flatten)]
    pub chunk: SourcedChunk,
    /// Only present for documents from a push source: the id the pushing party gave, so it can match its own record
    pub external_id: Option<String>,
    pub score: f64,
    /// From a source that decays and has not been searched for a long time: its score has already been multiplied by [`utopia_store::decay::STALE_SEARCH_WEIGHT`].
    /// For libraries without decay this cell is always false and is not in the response
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    pub bm25_rank: Option<u32>,
    pub vector_rank: Option<u32>,
}

#[derive(Debug)]
pub struct SearchPage {
    pub results: Vec<ScoredChunk>,
    pub has_more: bool,
    /// Whether the vector channel ran at all; false is the silent degradation the module header describes
    pub vector_used: bool,
}

pub async fn hybrid(
    state: &AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: &str,
    top_k: usize,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    embeddings: Option<&mut llm_util::EmbedCache>,
) -> AppResult<Vec<ChunkView>> {
    let (hits, _) = fused_hits(
        state,
        kb_id,
        workspace_id,
        query,
        Recall {
            per_channel: RECALL_PER_CHANNEL,
            as_of,
            documents: None,
        },
        embeddings,
    )
    .await?;
    Ok(hits
        .into_iter()
        .take(top_k)
        .map(|hit| hit.chunk.chunk)
        .collect())
}

/// One page of hits (#1271). Every page asks each channel for the same `MAX_DEPTH` items, so the pages slice
/// the same fused table: no repeats and no gaps between pages. The page that reaches `MAX_DEPTH` has no next page (paging further is a 422),
/// so `has_more` is false
pub async fn hybrid_scored(
    state: &AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: &str,
    opts: SearchOpts,
    embeddings: Option<&mut llm_util::EmbedCache>,
) -> AppResult<SearchPage> {
    let documents = match &opts.filter {
        Some(filter) => Some(
            utopia_store::documents::ids_in_source(
                &state.pool,
                kb_id,
                filter.source_id,
                filter.key_prefix.as_deref(),
                opts.as_of,
            )
            .await?,
        ),
        None => None,
    };
    let (hits, vector_used) = fused_hits(
        state,
        kb_id,
        workspace_id,
        query,
        Recall {
            per_channel: MAX_DEPTH,
            as_of: opts.as_of,
            documents: documents.as_deref(),
        },
        embeddings,
    )
    .await?;
    let end = opts.offset + opts.top_k;
    let has_more = end < MAX_DEPTH && hits.len() > end;
    let results = hits
        .into_iter()
        .skip(opts.offset)
        .take(opts.top_k)
        .collect();
    Ok(SearchPage {
        results,
        has_more,
        vector_used,
    })
}

/// How each channel recalls: how many items it takes, at which moment, and only from which documents (None means the whole library)
#[derive(Clone, Copy)]
struct Recall<'a> {
    per_channel: usize,
    as_of: Option<chrono::DateTime<chrono::Utc>>,
    documents: Option<&'a [Uuid]>,
}

/// The whole table after the two channels each take their share per `recall`, are fused by RRF, and are filtered by the record axis, together with whether the vector channel ran
async fn fused_hits(
    state: &AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: &str,
    recall: Recall<'_>,
    embeddings: Option<&mut llm_util::EmbedCache>,
) -> AppResult<(Vec<ScoredChunk>, bool)> {
    let bm25 = {
        let search = state.search.clone();
        let kb = kb_id.to_string();
        let query = query.to_string();
        let documents: Option<Vec<String>> = recall
            .documents
            .map(|ids| ids.iter().map(Uuid::to_string).collect());
        let limit = recall.per_channel;
        tokio::task::spawn_blocking(move || search.search(&kb, &query, limit, documents.as_deref()))
    };
    // `join!` is not `try_join!`: a failure in the vector channel is a degradation, not an error (see vector_channel),
    // while `try_join!` would turn a model failure into a retrieval error
    let (bm25, vector) = tokio::join!(
        bm25,
        vector_channel(state, kb_id, workspace_id, query, recall, embeddings)
    );
    let bm25 = bm25
        .map_err(|e| utopia_core::AppError::Other(anyhow::anyhow!("BM25 检索线程退出：{e}")))?
        .map_err(utopia_core::AppError::Other)?;
    let vector = vector?;
    let vector_used = vector.is_some();
    let lists = channel_lists(bm25.into_iter().map(|h| h.chunk_id).collect(), vector);

    // Keep the bounded channel candidates until their record-time and document
    // filters have run: newer hits must not consume a historical query's limit.
    let candidate_limit = lists.iter().map(Vec::len).sum();
    let fused: Vec<(Uuid, utopia_search::Fused)> =
        utopia_search::rrf_fuse_scored(&lists, candidate_limit)
            .into_iter()
            .filter_map(|f| Some((f.id.parse().ok()?, f)))
            .collect();
    let ids: Vec<Uuid> = fused.iter().map(|(id, _)| *id).collect();
    let live =
        utopia_store::documents::chunks_by_ids(&state.pool, kb_id, &ids, recall.as_of).await?;
    let mut fused: HashMap<Uuid, utopia_search::Fused> = fused.into_iter().collect();
    let mut hits: Vec<ScoredChunk> = live
        .into_iter()
        .filter_map(|chunk| {
            let f = fused.remove(&chunk.chunk.id)?;
            let external_id = chunk
                .source_kind
                .as_deref()
                .zip(chunk.external_key.as_deref())
                .and_then(|(kind, key)| crate::pushed_keys::pushed_identity(kind, key))
                .map(str::to_string);
            Some(ScoredChunk {
                chunk,
                external_id,
                score: f.score,
                stale: false,
                bm25_rank: position(&f.ranks, 0),
                vector_rank: position(&f.ranks, 1),
            })
        })
        .collect();
    // Stale ones are demoted before paging: only documents nobody has read for a long time can be pushed off this page, rather than merely moving within the page
    let document_ids: Vec<Uuid> = hits.iter().map(|hit| hit.chunk.chunk.document_id).collect();
    let stale = utopia_store::decay::stale_documents(&state.pool, &document_ids).await?;
    demote_stale(&mut hits, &stale);
    Ok((hits, vector_used))
}

/// Record one document handed out by a retrieval (RC-003). Written in the background: failures only log, and neither slows nor interrupts this read.
/// Only documents from a source with decay switched on actually get a row.
pub fn record_retrievals(state: &AppState, mut document_ids: Vec<Uuid>) {
    document_ids.sort_unstable();
    document_ids.dedup();
    let pool = state.pool.clone();
    tokio::spawn(async move {
        if let Err(e) = utopia_store::decay::record_retrievals(&pool, &document_ids).await {
            tracing::warn!(error = %e, "记录文档被检索失败");
        }
    });
}

/// Hits of stale documents are multiplied by [`utopia_store::decay::STALE_SEARCH_WEIGHT`] and then re-sorted. If none are stale nothing is changed,
/// and the sort is stable, so equal scores keep their original order
fn demote_stale(hits: &mut [ScoredChunk], stale: &std::collections::HashSet<Uuid>) {
    if stale.is_empty() {
        return;
    }
    for hit in hits.iter_mut() {
        if stale.contains(&hit.chunk.chunk.document_id) {
            hit.score *= utopia_store::decay::STALE_SEARCH_WEIGHT;
            hit.stale = true;
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
}

/// The rank within channel `channel`, counted from 1
fn position(ranks: &[Option<usize>], channel: usize) -> Option<u32> {
    ranks
        .get(channel)
        .copied()
        .flatten()
        .map(|rank| rank as u32 + 1)
}

/// The vector channel. With no embedding model configured, an embedding request that fails, or an empty reply, all give `Ok(None)`: retrieval carries on
/// with BM25 only, which is the silent degradation the module header describes. A failure of the vector query itself is still `Err`: that is the library's problem,
/// not the model's, and swallowing it would disguise a broken library as "no model configured"
///
/// If this turn has already embedded this sentence (picking metric definitions and by-name entity lookups both embed the question), that vector is used directly: no settings are
/// read again and the embedding endpoint is not called again (#971 follow-up); a newly embedded one that succeeds is recorded for the later steps
async fn vector_channel(
    state: &AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: &str,
    recall: Recall<'_>,
    cache: Option<&mut llm_util::EmbedCache>,
) -> AppResult<Option<Vec<String>>> {
    let vector = match cache.as_deref().and_then(|c| c.get(query)).cloned() {
        Some(v) => v,
        None => {
            let settings = utopia_store::settings::get(&state.pool, workspace_id).await?;
            let Some(client) = settings.as_ref().and_then(llm_util::embed_client) else {
                return Ok(None);
            };
            let mut embeddings = match client.embed(&[query.to_string()]).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "查询 embedding 失败，降级为纯 BM25");
                    return Ok(None);
                }
            };
            if embeddings.is_empty() {
                return Ok(None);
            }
            let v = embeddings.remove(0);
            if let Some(cache) = cache {
                cache.insert(query.to_string(), v.clone());
            }
            v
        }
    };
    let ids = utopia_store::documents::vector_search(
        &state.pool,
        kb_id,
        &vector,
        recall.per_channel as i64,
        recall.as_of,
        recall.documents,
    )
    .await?;
    Ok(Some(ids.into_iter().map(|id| id.to_string()).collect()))
}

/// The list fed into RRF: BM25 always first, the vector second if present. The order is fixed and does not depend on which returns first:
/// today's RRF ignores order, but if channels are ever weighted it would matter, and it must not depend on which network path was faster
fn channel_lists(bm25: Vec<String>, vector: Option<Vec<String>>) -> Vec<Vec<String>> {
    let mut lists = vec![bm25];
    if let Some(vector) = vector {
        lists.push(vector);
    }
    lists
}

#[cfg(test)]
mod tests {
    use super::channel_lists;

    #[test]
    fn the_channels_keep_their_places_whichever_answered_first() {
        let lists = channel_lists(vec!["b".into()], Some(vec!["v".into()]));
        assert_eq!(lists, vec![vec!["b".to_string()], vec!["v".to_string()]]);
    }

    #[test]
    fn a_missing_vector_channel_leaves_bm25_alone() {
        let lists = channel_lists(vec!["b".into()], None);
        assert_eq!(lists, vec![vec!["b".to_string()]]);
    }
}

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod retrieval_tests;
