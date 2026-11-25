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

/// Unrecognised fields answer 422: silently ignoring a misspelt filter would make the caller think it took effect
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchReq {
    pub q: String,
    #[serde(default)]
    pub top_k: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    /// Record axis (0019): search by **what the library held at that moment**. YYYY-MM-DD or RFC3339.
    /// Hits are the chunks alive at that time and the documents not yet deleted at that time
    #[serde(default)]
    pub as_of: Option<String>,
    /// Search only this library's documents from this source; sources from another library answer 404
    #[serde(default)]
    pub source_id: Option<Uuid>,
    /// Search only documents whose `external_id` starts with it. identity comes from a push source, so it needs `source_id`
    /// and it must be a push source, otherwise 422
    #[serde(default)]
    pub external_id_prefix: Option<String>,
    /// Whether this search counts as "used" (RC-003): by default it does. Probes, evaluations and health checks pass false and leave no trace
    #[serde(default = "record_by_default")]
    pub record: bool,
}

fn record_by_default() -> bool {
    true
}

async fn source_filter(
    state: &AppState,
    kb_id: Uuid,
    req: &SearchReq,
) -> ApiResult<Option<retrieval::SourceFilter>> {
    let source = match req.source_id {
        Some(source_id) => {
            Some(super::sources_routes::source_in_kb(state, kb_id, source_id).await?)
        }
        None => None,
    };
    let Some(prefix) = &req.external_id_prefix else {
        return Ok(source.map(|source| retrieval::SourceFilter {
            source_id: source.id,
            key_prefix: None,
        }));
    };
    let Some(source) = source.filter(|source| crate::pushed_keys::is_push_kind(&source.kind))
    else {
        return Err(AppError::invalid(
            "prefix_needs_push_source",
            "external_id_prefix needs the source_id of an API or Statements source",
        )
        .into());
    };
    Ok(Some(retrieval::SourceFilter {
        source_id: source.id,
        key_prefix: Some(crate::pushed_keys::pushed_key(&source, prefix)),
    }))
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
    let kb = super::documents_routes::require_reader_kb(&state, &reader, kb_id).await?;

    let top_k = req.top_k.unwrap_or(10).min(50);
    if req.offset.saturating_add(top_k) > retrieval::MAX_DEPTH {
        return Err(AppError::invalid(
            "page_too_deep",
            format!("offset + top_k may not exceed {}", retrieval::MAX_DEPTH),
        )
        .into());
    }
    let as_of = super::graph_routes::parse_instant("as_of", req.as_of.as_deref())?;
    let opts = retrieval::SearchOpts {
        top_k,
        offset: req.offset,
        as_of,
        filter: source_filter(&state, kb_id, &req).await?,
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
