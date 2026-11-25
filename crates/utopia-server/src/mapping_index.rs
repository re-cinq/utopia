//! Retrieval of metric definitions (#574): for data questions, pick a few confirmed definitions by the question, rather than stuffing the first thirty in dictionary order.
//!
//! Two channels, merged by RRF and the top few kept, in the shape of chunk retrieval (`retrieval::hybrid`):
//!
//! - **Vector**: each definition's "name: description (unit)" is embedded into `concept_mappings.embedding`; the question
//!   is embedded once, and pgvector sorts. Without an embedding model this channel does not exist.
//! - **Lexical**: all confirmed definitions are fetched back and scored in Rust: the question is split into words (English on whitespace and punctuation,
//!   Chinese in two-character pairs), and the hits are counted. A library has tens to hundreds of them, which is cheaper than Chinese word segmentation in SQL,
//!   and pg_trgm is not needed.
//!
//! Fallback chain: vector + lexical → lexical only (no embedding model) → when both channels are empty, the first few in dictionary order
//! (today's behaviour). Each level is a little worse than the one above; no level means "no metric definitions".
//!
//! The index is **lazy**: `relevant` first embeds whatever is not yet embedded, then queries. A data question usually has nothing to fill;
//! it only happens right after a batch is confirmed, and then that one request waits for one more embedding call. Nobody has to remember to refresh.
//!
//! The question's vector goes through this turn's cache (`llm_util::EmbedCache`, follow-up to #971): the conversation embeds it here once,
//! and the later chunk retrieval and by-name entity lookup reuse the same vector, so one turn calls the embedding endpoint only once.

use crate::llm_util;
use crate::state::AppState;
use utopia_core::models::ConceptMapping;
use utopia_store::mappings::MappingText;
use uuid::Uuid;

/// How many definitions go into the prompt. A question needs at most two (average unit price = net amount ÷ order count), so eight is enough;
/// more would fall back to "neighbouring names interfere with each other": the question that 17/18 missed is exactly the one where three neighbouring names were all present
pub const DEFINITIONS_IN_PROMPT: usize = 8;
/// How many each channel returns before merging: the same number as chunk retrieval
const RECALL_PER_CHANNEL: usize = 24;
/// How many to embed in one go
const BATCH: usize = 32;
/// At most this many to fill in per question. Filling runs on the question's request path: the first question after switching embedding models should not
/// embed several hundred definitions in one go before answering. Whatever cannot be filled is topped up by the next question, and in the meantime the lexical channel covers those that are missing
const REFRESH_PER_TURN: usize = 64;

/// Fill in the confirmed definitions not yet embedded. Does nothing without an embedding model (the lexical channel still runs).
pub async fn refresh(state: &AppState, kb_id: Uuid, workspace_id: Uuid) -> anyhow::Result<usize> {
    let Some(settings) = utopia_store::settings::get(&state.pool, workspace_id).await? else {
        return Ok(0);
    };
    let (Some(client), Some(model)) = (
        llm_util::embed_client(&settings),
        settings.embed_model.as_deref().filter(|m| !m.is_empty()),
    ) else {
        return Ok(0);
    };
    let stale = utopia_store::mappings::needing_embedding(
        &state.pool,
        kb_id,
        model,
        REFRESH_PER_TURN as i64,
    )
    .await?;
    if stale.is_empty() {
        return Ok(0);
    }
    if stale.len() >= REFRESH_PER_TURN {
        tracing::info!(%kb_id, "口径向量这一问只补一部分，剩下的下一问接着补");
    }
    let mut done = 0usize;
    for batch in stale.chunks(BATCH) {
        let texts: Vec<String> = batch.iter().map(MappingText::embed_text).collect();
        let vectors = {
            let _permit = llm_util::acquire_embed(state, &settings).await;
            client.embed(&texts).await?
        };
        // If the counts do not match, abandon the whole batch: pairing is by position, so one missing item misaligns everything (the same point as in chunk retrieval)
        if vectors.len() != batch.len() {
            anyhow::bail!(
                "embedding returned {} vectors for {} texts",
                vectors.len(),
                batch.len()
            );
        }
        let items: Vec<(MappingText, Vec<f32>)> = batch.iter().cloned().zip(vectors).collect();
        utopia_store::mappings::set_embeddings(&state.pool, model, &items).await?;
        done += items.len();
    }
    tracing::info!(%kb_id, embedded = done, "口径向量已补齐");
    Ok(done)
}

/// The definitions relevant to this question, ranked by relevance.
pub async fn relevant(
    state: &AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: &str,
    k: usize,
    embeddings: Option<&mut llm_util::EmbedCache>,
) -> anyhow::Result<Vec<ConceptMapping>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(utopia_store::mappings::confirmed(&state.pool, kb_id, k as i64).await?);
    }
    if let Err(e) = refresh(state, kb_id, workspace_id).await {
        // If they cannot be filled, use the existing vectors and lexical matching, so that one failed embedding does not sink the question
        tracing::warn!(%kb_id, error = %e, "口径向量没补上，按已有的检索");
    }

    let texts = utopia_store::mappings::confirmed_texts(&state.pool, kb_id).await?;
    let lexical: Vec<String> = lexical_rank(query, &texts, RECALL_PER_CHANNEL)
        .into_iter()
        .map(|id| id.to_string())
        .collect();

    let vector: Option<Vec<String>> =
        match vector_channel(state, kb_id, workspace_id, query, embeddings).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%kb_id, error = %e, "口径向量检索失败，只用词面");
                None
            }
        };

    let mut lists = vec![lexical];
    if let Some(v) = vector {
        lists.push(v);
    }
    let fused = utopia_search::rrf_fuse(&lists, k);
    let ids: Vec<Uuid> = fused.iter().filter_map(|s| s.parse().ok()).collect();
    if ids.is_empty() {
        // Neither channel hits (no word in the question matches): fall back to today's behaviour; something beats nothing
        return Ok(utopia_store::mappings::confirmed(&state.pool, kb_id, k as i64).await?);
    }
    Ok(utopia_store::mappings::by_ids(&state.pool, kb_id, &ids).await?)
}

/// The vector channel. If the cache already holds this sentence's vector it is not embedded again (and takes no embedding-gate slot), but the model name must still be
/// read from settings: only definitions embedded by the same model are compared
async fn vector_channel(
    state: &AppState,
    kb_id: Uuid,
    workspace_id: Uuid,
    query: &str,
    cache: Option<&mut llm_util::EmbedCache>,
) -> anyhow::Result<Option<Vec<String>>> {
    let Some(settings) = utopia_store::settings::get(&state.pool, workspace_id).await? else {
        return Ok(None);
    };
    let (Some(client), Some(model)) = (
        llm_util::embed_client(&settings),
        settings.embed_model.as_deref().filter(|m| !m.is_empty()),
    ) else {
        return Ok(None);
    };
    let vector = match cache.as_deref().and_then(|c| c.get(query)).cloned() {
        Some(v) => v,
        None => {
            let mut vectors = {
                let _permit = llm_util::acquire_embed(state, &settings).await;
                client.embed(&[query.to_string()]).await?
            };
            if vectors.is_empty() {
                return Ok(None);
            }
            let v = vectors.remove(0);
            if let Some(cache) = cache {
                cache.insert(query.to_string(), v.clone());
            }
            v
        }
    };
    let ids = utopia_store::mappings::vector_search(
        &state.pool,
        kb_id,
        model,
        &vector,
        RECALL_PER_CHANNEL as i64,
    )
    .await?;
    Ok(Some(ids.into_iter().map(|id| id.to_string()).collect()))
}

/// Split the question into words: English is split on whitespace and punctuation and lowercased; Chinese in two-character pairs (a single character is too fragmented, three miss too much).
/// A single ASCII character does not count as a word ("a" and "x" would match everything).
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_cjk: Option<char> = None;
    for c in s.chars() {
        let cjk = ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c);
        if cjk {
            if word.len() > 1 {
                out.push(word.to_lowercase());
            }
            word.clear();
            if let Some(p) = prev_cjk {
                out.push(format!("{p}{c}"));
            }
            prev_cjk = Some(c);
            continue;
        }
        prev_cjk = None;
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            if word.len() > 1 {
                out.push(word.to_lowercase());
            }
            word.clear();
        }
    }
    if word.len() > 1 {
        out.push(word.to_lowercase());
    }
    out.sort();
    out.dedup();
    out
}

/// Lexical channel: for each definition, the count of the question's words its text hits; more hits come first, and a definition with no hits is not returned.
fn lexical_rank(query: &str, texts: &[MappingText], limit: usize) -> Vec<Uuid> {
    let qs = tokens(query);
    if qs.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, Uuid)> = texts
        .iter()
        .map(|t| {
            let hay = t.lexical_text().to_lowercase();
            (qs.iter().filter(|q| hay.contains(q.as_str())).count(), t.id)
        })
        .filter(|(n, _)| *n > 0)
        .collect();
    // More hits first; ties ordered by id, so the same question always gets the same order
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, id)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::{lexical_rank, tokens};
    use utopia_store::mappings::MappingText;
    use uuid::Uuid;

    fn t(id: u128, name: &str, summary: &str, expr: &str) -> MappingText {
        MappingText {
            id: Uuid::from_u128(id),
            concept_name: name.into(),
            summary: Some(summary.into()),
            unit: None,
            table_name: Some("dw.dwd_ord_dtl".into()),
            expr: Some(expr.into()),
        }
    }

    #[test]
    fn a_question_is_cut_into_words_in_either_language() {
        // Output is sorted and de-duplicated; the expected value is sorted the same way, rather than hand-sorted by code point
        let sorted = |v: &[&str]| {
            let mut v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
            v.sort();
            v
        };
        assert_eq!(
            tokens("What is our total GMV?"),
            sorted(&["what", "is", "our", "total", "gmv"])
        );
        // Chinese in two-character pairs; a single ASCII character does not count as a word
        assert_eq!(
            tokens("扣掉退款之后的净销售额是多少？"),
            sorted(&[
                "扣掉", "掉退", "退款", "款之", "之后", "后的", "的净", "净销", "销售", "售额",
                "额是", "是多", "多少",
            ])
        );
        assert_eq!(tokens("a x GMV"), vec!["gmv"]);
    }

    #[test]
    fn the_definition_that_shares_the_most_words_comes_first() {
        let texts = vec![
            t(1, "GMV", "已支付、非测试单的实付金额，元", "sum(amt_pay)"),
            t(2, "净销售额", "GMV 扣掉退款", "sum(amt_pay - amt_rfnd)"),
            t(3, "客单价", "净销售额除以有效订单数", "…"),
            t(4, "运费收入", "已支付订单的运费", "sum(amt_frght)"),
        ];
        let got = lexical_rank("扣掉退款之后的净销售额是多少", &texts, 8);
        assert_eq!(
            got[0],
            Uuid::from_u128(2),
            "净销售额那条命中「扣掉」「退款」「净销」「销售」「售额」"
        );
        assert!(
            got.contains(&Uuid::from_u128(3)),
            "客单价的说明里也有「净销售额」"
        );
        assert!(!got.contains(&Uuid::from_u128(4)), "运费一个词都不沾");
        // No word matches at all: empty, so the caller falls back to dictionary order
        assert!(lexical_rank("zzz", &texts, 8).is_empty());
    }
}
