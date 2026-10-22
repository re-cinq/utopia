//! 衰减的每日扫描（RC-003）：把打开了衰减的源淘汰回它的 `max_items`。
//!
//! 选谁由 `utopia_store::decay::plan_eviction` 定；这里照人删文档的办法删（可恢复），
//! 同时把文档从全文索引里拿掉，并记一笔无人操作的审计。

use crate::state::AppState;
use serde_json::json;
use utopia_core::AppError;
use uuid::Uuid;

pub async fn sweep_source(state: &AppState, source_id: Uuid) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let Some(eviction) = utopia_store::decay::plan_eviction(&state.pool, source_id, now).await?
    else {
        return Ok(());
    };
    let mut evicted = 0;
    for id in &eviction.documents {
        match utopia_store::documents::delete(&state.pool, eviction.kb_id, *id, None).await {
            Ok(_) => {}
            // 扫描进行当中被人先删了
            Err(AppError::NotFound) => continue,
            Err(e) => return Err(e.into()),
        }
        utopia_store::decay::mark_evicted(&state.pool, *id).await?;
        let search = state.search.clone();
        let document_id = id.to_string();
        tokio::task::spawn_blocking(move || search.delete_document(&document_id)).await??;
        evicted += 1;
    }
    utopia_store::audit::record_opt(
        &state.pool,
        Some(eviction.kb_id),
        None,
        "decay.evict",
        "source",
        Some(source_id),
        json!({ "evicted": evicted, "lowest_score": eviction.lowest_score }),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "decay_sweep_tests.rs"]
mod decay_sweep_tests;
