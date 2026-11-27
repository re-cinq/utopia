use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use utopia_core::config::AppConfig;
use utopia_search::SearchIndex;
use uuid::Uuid;

/// In-service event (SSE pushed to the frontend for partial refresh).
#[derive(Clone, Debug, serde::Serialize)]
pub struct AppEvent {
    /// None = belongs to no KB. The alert badge spans KBs, and system-level alerts have no KB at all
    pub kb_id: Option<Uuid>,
    /// document = document ingest/extraction status changed; review = review queue changed;
    /// alert = the alert center has changed
    pub kind: &'static str,
    pub document_id: Option<Uuid>,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Chat/MCP tool calls and graph reads run here, so they can never hold more than its size of the main pool
    pub interactive_pool: PgPool,
    pub jwt_secret: String,
    pub search: Arc<SearchIndex>,
    /// In-memory index of the Charter (built-in documents): used by the chat's search_docs tool
    pub docs: Arc<utopia_search::DocsIndex>,
    /// Seam for storing and fetching raw file bytes (content-addressed, key = sha256); the current implementation is local disk
    pub blob: Arc<dyn crate::blob::BlobStore>,
    pub open_registration: bool,
    pub password_login: bool,
    /// Force Secure cookies (config option); when not forced, judged per request from the X-Forwarded-Proto header
    pub cookie_secure: bool,
    /// Chunk budget, see `AppConfig::chunk_tokens`
    pub chunk_tokens: usize,
    /// See `AppConfig::extract_known_in_prompt`
    pub extract_known_in_prompt: bool,
    /// Worker concurrency: the scheduling loop re-reads it on every round, so a system settings change takes effect at once
    pub worker_concurrency: Arc<std::sync::atomic::AtomicUsize>,
    /// Chat clients per workspace. The context window an endpoint reported is recorded on the client, so keeping the client lets the next round remember it (#964)
    pub chat_clients: Arc<crate::llm_util::ConversationClients>,
    /// Concurrency gates per model: background tasks take a permit before calling the LLM. Limits are stored in the DB and take effect at once when changed
    pub model_gates: Arc<crate::llm_util::ModelGates>,
    pub events: broadcast::Sender<AppEvent>,
    /// Answers currently being generated, looked up by session. **Can still be reattached after a page refresh** (see `live`)
    pub live: Arc<crate::live::Registry>,
    /// Connection string of the table owner: only exists when the migration identity and the runtime identity are separate (`UTOPIA_MIGRATION_URL`).
    /// At runtime only building the vector index uses it, and it is closed as soon as it is done (`vector_index::build_as_owner`, #1120)
    pub owner_database_url: Option<String>,
}

impl AppState {
    pub fn with_interactive_pool(self, pool: PgPool) -> Self {
        Self {
            interactive_pool: pool,
            ..self
        }
    }

    pub fn for_interactive_reads(&self) -> AppState {
        Self {
            pool: self.interactive_pool.clone(),
            ..self.clone()
        }
    }

    /// `jwt_secret` is resolved by the entry point: if the environment variable is set, that is it; otherwise it is the one in the DB (generated on first start).
    /// It is not taken from cfg, because by this point it must already be a definite value, not an Option.
    pub fn new(
        pool: PgPool,
        cfg: &AppConfig,
        search: Arc<SearchIndex>,
        jwt_secret: String,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        let data_dir = PathBuf::from(&cfg.data_dir);
        let blob = Arc::new(crate::blob::LocalBlobStore::new(data_dir.join("files")));
        Self {
            interactive_pool: pool.clone(),
            pool,
            jwt_secret,
            search,
            docs: Arc::new(crate::docs_corpus::build_index()),
            blob,
            open_registration: cfg.open_registration,
            password_login: cfg.password_login,
            cookie_secure: cfg.cookie_secure,
            chunk_tokens: cfg.chunk_tokens,
            extract_known_in_prompt: cfg.extract_known_in_prompt,
            // Matches the column default of `deployment_settings.worker_concurrency` (migration 0011):
            // access::worker_concurrency falls back to 64 when the row is missing, and main.rs falls back to 64 when
            // the function itself errors. This is the value used **before the first read after the service starts**; if it is wrong,
            // the service runs at the reduced concurrency that migration 0011 was meant to avoid, for the short window after startup.
            worker_concurrency: Arc::new(std::sync::atomic::AtomicUsize::new(64)),
            chat_clients: Arc::new(crate::llm_util::ConversationClients::default()),
            model_gates: Arc::new(crate::llm_util::ModelGates::default()),
            events,
            live: Arc::new(crate::live::Registry::default()),
            owner_database_url: cfg.migration_url.clone(),
        }
    }

    /// When there are no subscribers, send returns Err: normal, silently ignored.
    pub fn emit_document(&self, kb_id: Uuid, document_id: Uuid) {
        let _ = self.events.send(AppEvent {
            kb_id: Some(kb_id),
            kind: "document",
            document_id: Some(document_id),
        });
    }

    pub fn emit_review(&self, kb_id: Uuid) {
        let _ = self.events.send(AppEvent {
            kb_id: Some(kb_id),
            kind: "review",
            document_id: None,
        });
    }

    /// A memory extraction produced facts waiting for a nod (0015). The confirmation card in the conversation refreshes on this:
    /// extraction is asynchronous, so the card can appear only when the task completes, not at the moment the assistant replies
    pub fn emit_pending(&self, kb_id: Uuid) {
        let _ = self.events.send(AppEvent {
            kb_id: Some(kb_id),
            kind: "pending",
            document_id: None,
        });
    }

    /// The graph changed. After reasoning adds edges to the graph, fire this once: it does not go through the document pipeline,
    /// while the `document` event is for the document pipeline only
    pub fn emit_graph(&self, kb_id: Uuid) {
        let _ = self.events.send(AppEvent {
            kb_id: Some(kb_id),
            kind: "graph",
            document_id: None,
        });
    }

    pub fn emit_source(&self, kb_id: Uuid) {
        let _ = self.events.send(AppEvent {
            kb_id: Some(kb_id),
            kind: "source",
            document_id: None,
        });
    }

    /// Alerts changed. **Carries no data and does not check permissions**: whoever receives it refetches the list,
    /// and "who can see what" is decided once, in the list query.
    ///
    /// The cost is that someone without permission also gets woken to refetch once, and still gets an empty list. What it buys is that the push path
    /// has no permission logic at all, so there is no way for push and list to decide differently.
    pub fn emit_alert(&self) {
        let _ = self.events.send(AppEvent {
            kb_id: None,
            kind: "alert",
            document_id: None,
        });
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
