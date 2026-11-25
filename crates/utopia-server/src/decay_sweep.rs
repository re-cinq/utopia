//! Daily decay sweep (RC-003): evicts sources with decay enabled back down to their `max_items`.
//!
//! `utopia_store::decay::plan_eviction` picks which items; they are deleted the same way a person deletes a document (recoverable),
//! and removed from the full-text index too, with an audit entry recorded as having no operator.

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
            // Someone deleted it first while the sweep was in progress
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
