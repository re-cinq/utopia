//! Building the LLM client from workspace settings, and the per-model concurrency gates.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use utopia_core::models::LlmSettings;
use utopia_llm::LlmClient;

use crate::state::AppState;

pub fn chat_client(s: &LlmSettings) -> Option<LlmClient> {
    if !s.chat_ready() {
        return None;
    }
    Some(
        LlmClient::new(
            s.chat_base_url.as_deref()?,
            s.chat_api_key.as_deref(),
            s.chat_model.as_deref()?,
        )
        .with_reasoning_effort(s.chat_reasoning_effort.clone()),
    )
}

/// Chat client for the settings page's "Test" button: same settings as [`chat_client`], with a much shorter timeout (#1354 review).
/// The task client tolerates five minutes of silence because reasoning models think before they answer; the test asks a single word, and an endpoint
/// that accepts the connection but never replies should not make someone wait on the settings page for a quarter of an hour.
pub fn chat_probe_client(s: &LlmSettings) -> Option<LlmClient> {
    if !s.chat_ready() {
        return None;
    }
    Some(
        LlmClient::with_timeouts(
            s.chat_base_url.as_deref()?,
            s.chat_api_key.as_deref(),
            s.chat_model.as_deref()?,
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(30),
        )
        .with_reasoning_effort(s.chat_reasoning_effort.clone()),
    )
}

/// Chat model settings: address, key, model, reasoning effort. If any of them changes, it is a different client.
type ChatConfig = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Chat clients per workspace, kept across turns.
///
/// The context window an endpoint reported is recorded on the client (`LlmClient::remember_context_window`). Creating a new one each turn
/// would forget it every turn, so the next turn hits the same 400 again (#964). When the model settings change, a new one is created: the learned window
/// belongs to that model, not to this workspace. Neither the key nor the window is shared across workspaces.
#[derive(Default)]
pub struct ConversationClients {
    inner: std::sync::Mutex<HashMap<uuid::Uuid, (ChatConfig, LlmClient)>>,
}

impl ConversationClients {
    pub fn get(&self, workspace: uuid::Uuid, s: &LlmSettings) -> Option<LlmClient> {
        let mut clients = self.inner.lock().unwrap();
        if !s.chat_ready() {
            clients.remove(&workspace);
            return None;
        }
        let config = (
            s.chat_base_url.clone(),
            s.chat_api_key.clone(),
            s.chat_model.clone(),
            s.chat_reasoning_effort.clone(),
        );
        if let Some((previous, client)) = clients.get(&workspace) {
            if previous == &config {
                return Some(client.clone());
            }
        }
        let client = chat_client(s)?;
        clients.insert(workspace, (config, client.clone()));
        Some(client)
    }
}

/// Text embedded during a turn, with its vectors (#971). Choosing the scope, chunking for retrieval, and looking up entities by name all need the question embedded as
/// vectors; within one turn it is the same sentence with the same model, so embedding it once is enough. The cache follows this turn's `ToolSink` and is not persisted;
/// only successes are recorded. Callers with no turn (HTTP retrieval, the scope page) pass `None` and each embeds on its own.
pub type EmbedCache = std::collections::HashMap<String, Vec<f32>>;

pub fn embed_client(s: &LlmSettings) -> Option<LlmClient> {
    if !s.embed_ready() {
        return None;
    }
    Some(LlmClient::new(
        s.embed_base_url.as_deref()?,
        s.embed_api_key.as_deref(),
        s.embed_model.as_deref()?,
    ))
}

/// Per-model semaphore registry: one per model, with limits adjusted in place when they change (#1302).
///
/// It used to swap in a new semaphore whenever a limit changed. Tasks waiting on the old one did not move over; each had to finish one call
/// with the old limit before switching to the new one. An extraction call takes several minutes, and eight workers queued behind one permit, so after the limit went
/// from 1 to 4 it still ran only one call at a time for hours, which looked like "the change did not take effect".
///
/// Raising uses `add_permits`, so tasks already waiting are let in at once. When lowering, free permits are reclaimed on the spot
/// (`forget_permits`); permits held by in-flight calls are recorded as debt, and collected by the waiting task that gets them. Acquiring the difference in a separate task
/// and then forgetting it does not work: the semaphore is first-come, first-served, so that task queues behind the tasks already waiting, and the returned permits go to them first.
/// They all get in above the lowered limit, and only then does it get one.
///
/// The limit is read in two places: when taking a permit, and when an admin changes it ([`ModelGates::refresh`]). Relying on the first alone
/// fails when all workers wait on the gate and the only call in flight is a repeat that never returns: nobody reads the new limit.
#[derive(Default)]
pub struct ModelGates {
    inner: std::sync::Mutex<HashMap<(String, String), Gate>>,
}

struct Gate {
    sem: Arc<Semaphore>,
    limit: usize,
    /// Number of permits that should still be reclaimed after lowering, which in-flight calls were holding at the time
    owed: usize,
}

impl ModelGates {
    async fn admit(
        &self,
        base_url: &str,
        model: &str,
        limit: usize,
    ) -> Option<OwnedSemaphorePermit> {
        let key = (base_url.to_string(), model.to_string());
        let sem = self.resize(&key, limit);
        loop {
            let permit = sem.clone().acquire_owned().await.ok()?;
            if !self.repay(&key) {
                return Some(permit);
            }
            permit.forget();
        }
    }

    /// Call each known model once with the limit stored in the DB: run when an admin changes a limit, so waiting tasks are let in or held back on the spot
    pub async fn refresh(&self, pool: &sqlx::PgPool) -> utopia_core::AppResult<()> {
        let keys: Vec<(String, String)> = self.inner.lock().unwrap().keys().cloned().collect();
        for key in keys {
            let limit = utopia_store::model_limits::limit_for(pool, &key.0, &key.1).await?;
            self.resize(&key, limit);
        }
        Ok(())
    }

    fn resize(&self, key: &(String, String), limit: usize) -> Arc<Semaphore> {
        let mut gates = self.inner.lock().unwrap();
        let gate = gates.entry(key.clone()).or_insert_with(|| Gate {
            sem: Arc::new(Semaphore::new(limit)),
            limit,
            owed: 0,
        });
        if limit > gate.limit {
            gate.sem.add_permits(limit - gate.limit);
        } else if limit < gate.limit {
            let lower = gate.limit - limit;
            gate.owed += lower - gate.sem.forget_permits(lower);
        }
        gate.limit = limit;
        gate.sem.clone()
    }

    /// If permits are still owed, reclaim the one just acquired
    fn repay(&self, key: &(String, String)) -> bool {
        match self.inner.lock().unwrap().get_mut(key) {
            Some(gate) if gate.owed > 0 => {
                gate.owed -= 1;
                true
            }
            _ => false,
        }
    }
}

/// Take a permit from the model before a background task calls it, and hold it until the call ends.
///
/// **Only for background tasks** (extraction, adjudication, ingest embedding, ontology suggestions). User chat and retrieval
/// do not go through here: if a person typing waited behind ten background extractions, the product would feel broken; and what really exhausts
/// a provider's rate limit is never one person typing.
///
/// If the limit cannot be read (the table does not exist yet, or the DB is briefly unreachable), **let it through**: concurrency limits are a protection,
/// and should not stall the whole pipeline just because the config cannot be read.
pub async fn acquire(
    state: &AppState,
    base_url: &str,
    model: &str,
) -> Option<OwnedSemaphorePermit> {
    let limit = utopia_store::model_limits::limit_for(&state.pool, base_url, model)
        .await
        .ok()?;
    state.model_gates.admit(base_url, model, limit).await
}

/// Convenience form of `acquire`: takes the chat model's identity straight from the workspace settings.
pub async fn acquire_chat(state: &AppState, s: &LlmSettings) -> Option<OwnedSemaphorePermit> {
    let (base, model) = (s.chat_base_url.as_deref()?, s.chat_model.as_deref()?);
    acquire(state, base, model).await
}

/// Convenience form of `acquire`: the embedding model.
pub async fn acquire_embed(state: &AppState, s: &LlmSettings) -> Option<OwnedSemaphorePermit> {
    let (base, model) = (s.embed_base_url.as_deref()?, s.embed_model.as_deref()?);
    acquire(state, base, model).await
}

#[cfg(test)]
#[path = "llm_util_tests.rs"]
mod tests;
