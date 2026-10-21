use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utopia_core::AppError;
use uuid::Uuid;

use crate::auth::SessionOrToken;
use crate::error::ApiResult;
use crate::retrieval;
use crate::state::AppState;

/// 翻页最深到第几条：每路召回随页深加长，再深就是在拿检索当导出用（#1271）
const MAX_DEPTH: usize = 100;
/// 前缀最多几条、每条多长：它们进的是一条 SQL 的数组参数，不是让人塞一份清单
const MAX_PREFIXES: usize = 8;
const MAX_PREFIX_BYTES: usize = 256;

#[derive(Deserialize)]
pub struct SearchReq {
    pub q: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    /// 记录轴（0019）：按**那一刻库里有的东西**检索。YYYY-MM-DD 或 RFC3339。
    /// 命中的是当时活着的块、当时还没被删的文档
    #[serde(default)]
    pub as_of: Option<String>,
    /// 只检索这个来源的文档（必须属于本库，否则 404）
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// 只检索 `external_id` 以其中之一开头的文档；前缀是相对 `source_id` 说的，
    /// 所以没有 `source_id` 不收
    #[serde(default)]
    pub external_id_prefixes: Option<Vec<String>>,
    /// 这次检索算不算「用过」（RC-003）：默认算。探测、评测、健康检查传 false，不留痕
    #[serde(default = "record_by_default")]
    pub record: bool,
}

fn record_by_default() -> bool {
    true
}

fn check_prefixes(req: &SearchReq) -> Result<(), AppError> {
    let Some(prefixes) = &req.external_id_prefixes else {
        return Ok(());
    };
    if req.source_id.is_none() {
        return Err(AppError::invalid(
            "prefixes_need_source",
            "external_id_prefixes requires source_id",
        ));
    }
    if prefixes.is_empty() || prefixes.len() > MAX_PREFIXES {
        return Err(AppError::invalid(
            "bad_prefixes",
            format!("external_id_prefixes takes 1 to {MAX_PREFIXES} entries"),
        ));
    }
    if prefixes
        .iter()
        .any(|p| p.is_empty() || p.len() > MAX_PREFIX_BYTES)
    {
        return Err(AppError::invalid(
            "bad_prefixes",
            format!("each external_id prefix is 1 to {MAX_PREFIX_BYTES} bytes"),
        ));
    }
    Ok(())
}

pub async fn search(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(kb_id): Path<Uuid>,
    Json(req): Json<SearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let q = req.q.trim();
    if q.is_empty() {
        return Err(AppError::invalid("empty_query", "Search query cannot be empty").into());
    }
    super::documents_routes::require_reader_kb(&state, &reader, kb_id).await?;
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;

    let top_k = req.top_k.unwrap_or(10).min(50);
    if req.offset.saturating_add(top_k) > MAX_DEPTH {
        return Err(AppError::invalid(
            "page_too_deep",
            format!("offset + top_k may not exceed {MAX_DEPTH}"),
        )
        .into());
    }
    check_prefixes(&req)?;
    let as_of = super::graph_routes::parse_instant("as_of", req.as_of.as_deref())?;
    let documents = match req.source_id {
        Some(source_id) => {
            super::sources_routes::source_in_kb(&state, kb_id, source_id).await?;
            Some(
                utopia_store::documents::ids_in_source(
                    &state.pool,
                    kb_id,
                    source_id,
                    req.external_id_prefixes.as_deref(),
                    as_of,
                )
                .await?,
            )
        }
        None => None,
    };
    let opts = retrieval::SearchOpts {
        top_k,
        offset: req.offset,
        as_of,
        documents,
    };
    let page = retrieval::hybrid_scored(&state, kb_id, kb.workspace_id, q, opts, None).await?;
    if req.record {
        let documents = page.results.iter().map(|hit| hit.chunk.chunk.document_id);
        retrieval::record_retrievals(&state, documents.collect());
    }
    Ok(Json(json!({
        "results": page.results,
        "offset": req.offset,
        "top_k": top_k,
        "has_more": page.has_more,
        "vector_used": page.vector_used,
    })))
}

#[cfg(test)]
mod tests {
    use super::SearchReq;
    use serde_json::json;

    #[test]
    fn a_search_counts_as_a_use_unless_it_says_otherwise() {
        let plain: SearchReq = serde_json::from_value(json!({ "q": "x" })).unwrap();
        let probe: SearchReq =
            serde_json::from_value(json!({ "q": "x", "record": false })).unwrap();
        assert!(plain.record);
        assert!(!probe.record);
    }
}
