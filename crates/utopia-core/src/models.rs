use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub org_id: Uuid,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub display_name: String,
    /// System administrator (the deployment's first registered user)
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

/// Workspace member view (for the member management page).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MemberView {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub is_admin: bool,
}

/// Deployment-wide user list (for the member picker when adding members).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OrgUser {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Workspace {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// Member role, sorted from highest to lowest permission. Stored as lowercase text in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
    Owner,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Editor => "editor",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            "editor" => Some(Role::Editor),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Document {
    pub id: Uuid,
    pub kb_id: Uuid,
    pub source_id: Option<Uuid>,
    pub filename: String,
    pub mime: String,
    pub size_bytes: i64,
    pub sha256: String,
    /// pending → parsing → indexing → embedding → ready | failed
    pub status: String,
    pub error: Option<String>,
    pub doc_time: Option<DateTime<Utc>>,
    /// Where `doc_time` comes from: `content` (read from the body) / `source` (given by the source system: publish time,
    /// archive date) / `none` (no date). Neither the upload time nor the file modification time is the document's date
    /// (0045 decision 3, #714). The legacy values `upload_time` / `file_mtime` are read as no date, see
    /// [`Document::dated_at`]
    pub doc_time_source: String,
    /// The document's time context (0045 decision 3): its own date, the period it defines and calendar, and anchors set by the narrative.
    /// Filled in by the server as it extracts; `time_context_at` is the moment it was last written
    pub time_context: Option<serde_json::Value>,
    pub time_context_at: Option<DateTime<Utc>>,
    /// Graph extraction status: none → queued → extracting → done | failed
    pub graph_status: String,
    /// Extraction failure reason (only present when failed). Kept apart from error: that column belongs to the parse pipeline,
    /// set_status clears it, and sharing one column would wipe each other out.
    pub graph_error: Option<String>,
    pub text_len: i32,
    pub chunk_count: i32,
    pub tags: Vec<String>,
    /// Logical identity within the source (relative path / url / rss guid / api external_id); NULL for uploads
    pub external_key: Option<String>,
    /// watch_folder sync found that the source file has disappeared (documents are kept by default, only marked)
    pub missing_since: Option<DateTime<Utc>>,
    /// Tombstone (#268): deletion is an event on the epistemic axis. Rows, chunks, evidence and raw files are all kept,
    /// they just no longer count as live. Undo, sync encountering it, or re-uploading the same content can all revive it
    pub deleted_at: Option<DateTime<Utc>>,
    /// Hard delete (#268, second half): the content has been wiped and cannot be recovered. The row stays as a tombstone
    pub purged_at: Option<DateTime<Utc>>,
    /// Reading this file's text needs a model of a kind that is not configured yet: `ocr` / `transcribe` (0040).
    /// The document is failed at this point; once configured, it is re-queued for processing
    pub reader_needed: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Document {
    /// The document's own date: only body text or source-given dates count (`content` / `source`). Upload, sync,
    /// and extraction times are record time, not the document's date (0045 decision 3, #714). Other sources are always `None`
    pub fn dated_at(&self) -> Option<DateTime<Utc>> {
        match self.doc_time_source.as_str() {
            "content" | "source" => self.doc_time,
            _ => None,
        }
    }
}

/// Ingestion source ("the source is the folder": container + scheduled sync).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Source {
    pub id: Uuid,
    pub kb_id: Uuid,
    /// upload | watch_folder | url | rss | api
    pub kind: String,
    pub name: String,
    /// Kind-specific config: watch_folder {path} / url {urls:[..]} / rss {feed_url}
    pub config: serde_json::Value,
    /// lucide icon name (when NULL the frontend picks a default by kind)
    pub icon: Option<String>,
    /// NULL = manual sync only (mutually exclusive with sync_cron)
    pub sync_interval_minutes: Option<i32>,
    /// Standard 5-field cron (server local timezone; mutually exclusive with sync_interval_minutes)
    pub sync_cron: Option<String>,
    /// NULL = documents do not decay; the object's shape is in `utopia_store::decay::DecaySettings` (RC-003)
    pub decay: Option<serde_json::Value>,
    pub last_sync_at: Option<DateTime<Utc>>,
    /// never | queued | running | ok | failed
    pub last_sync_status: String,
    pub last_sync_error: Option<String>,
    pub last_sync_added: i32,
    /// Where the ticket source has read up to (ADR 0072). NULL = no cursor: first sync, or this kind of
    /// source does not use a cursor.
    ///
    /// **Says "how far we've read", not "when this round ran"**. The latter is `last_sync_at`.
    /// It is common for a round not to finish reading (one update timestamp may have a thousand tickets pending). The cursor says
    /// "consumed up to this update timestamp, and the whole group at that moment has been read". The next round resumes from there,
    /// rather than skipping what this round did not finish (#1212)
    pub sync_checkpoint: Option<DateTime<Utc>>,
    /// Push secret for an api source (plaintext; viewing goes through a dedicated Editor-permission endpoint, list responses omit it)
    #[serde(skip_serializing)]
    pub ingest_token: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Source {
    /// Whether documents under this source go into extraction.
    ///
    /// **The default is yes.** Only skipped if config explicitly says `{"extract": false}`. Schema
    /// documents are like this (0035 decision 7): they are corpus for retrieval of table structure, not a source of facts.
    /// Extracting them would turn each column name into an entity. The switch is recorded on the source rather than the document, because
    /// "retrieve only, don't learn" is a property of the whole source. The source's name is not used as a rule either, which would be
    /// a naming convention masquerading as a type guarantee (0009).
    ///
    /// Non-boolean values are treated as unset: a typo should not silently stop extraction for a whole source.
    /// The SQL in `documents::queue_extraction` decides the same thing, so changing one means changing both.
    pub fn extracts(&self) -> bool {
        self.config
            .get("extract")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    }

    /// For a statements source whose config is `{"names": "identity"}`, each name it pushes points at
    /// a definite thing (0071), merged literally without name-vector matching. Other sources are unchanged.
    pub fn names_are_identities(&self) -> bool {
        self.kind == "statements"
            && self.config.get("names").and_then(serde_json::Value::as_str) == Some("identity")
    }
}

#[cfg(test)]
mod source_extracts_tests {
    use super::Source;

    fn with(config: serde_json::Value) -> Source {
        Source {
            id: uuid::Uuid::nil(),
            kb_id: uuid::Uuid::nil(),
            kind: "folder".into(),
            name: "x".into(),
            config,
            icon: None,
            sync_interval_minutes: None,
            sync_cron: None,
            decay: None,
            last_sync_at: None,
            last_sync_status: "never".into(),
            last_sync_error: None,
            last_sync_added: 0,
            sync_checkpoint: None,
            ingest_token: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn only_an_explicit_false_turns_extraction_off() {
        // Legacy source configs are `{}`, and watch_folder's is `{"path": …}`: both keep extracting as before
        assert!(with(serde_json::json!({})).extracts());
        assert!(with(serde_json::json!({ "path": "/x" })).extracts());
        assert!(with(serde_json::json!({ "extract": true })).extracts());
        // Non-boolean values are treated as unset, not as false
        assert!(with(serde_json::json!({ "extract": "no" })).extracts());
        assert!(with(serde_json::json!({ "extract": 0 })).extracts());
        assert!(!with(serde_json::json!({ "extract": false })).extracts());
    }

    fn of_kind(kind: &str, config: serde_json::Value) -> Source {
        Source {
            kind: kind.into(),
            ..with(config)
        }
    }

    #[test]
    fn only_a_statements_source_with_the_string_identity_has_identity_names() {
        let identity = serde_json::json!({ "names": "identity" });
        assert!(of_kind("statements", identity.clone()).names_are_identities());
        assert!(!of_kind("api", identity).names_are_identities());
        assert!(!of_kind("statements", serde_json::json!({})).names_are_identities());
        assert!(!of_kind("statements", serde_json::json!({ "names": true })).names_are_identities());
        assert!(
            !of_kind("statements", serde_json::json!({ "names": "prose" })).names_are_identities()
        );
    }
}

/// The keys in a source's config that are **used for authentication**. Credentials go in but never come out: list and create/update responses strip them.
/// On update, if the client omits them or sends an empty string, the stored original value is kept, and they are not written to the audit log either.
///
/// **One table, shared by four places.** Previously the rule held only for the single `auth_header` key, while the object storage,
/// WebDAV and Notion secrets were sent as-is to every Viewer (#246). When adding a connector, **add it here first**,
/// then write the code that reads it. `username` / `account_name` / `access_key_id` are identifiers: alone they cannot
/// authenticate, and are kept so the UI can show which account this is.
pub const SOURCE_SECRET_KEYS: &[&str] = &[
    "auth_header",
    "token",
    "password",
    "secret_access_key",
    "account_key",
    "service_account_key",
    // Google Drive OAuth refresh token (0074): it can be exchanged for read-only access to that person's entire Drive
    "refresh_token",
];

impl Source {
    /// This source with credentials removed. Any `Source` returned to clients passes through here
    pub fn without_secrets(mut self) -> Self {
        if let Some(obj) = self.config.as_object_mut() {
            for key in SOURCE_SECRET_KEYS {
                obj.remove(*key);
            }
        }
        self
    }
}

/// The kinds of source. **Defined in one place, consumed in three**: the allowlist at creation, the dispatch at sync time (exhaustive enum match,
/// adding one means deciding how it syncs), and the frontend dropdown (`web/src/sourceKinds.ts`, with
/// the `utopia-store` tests checking the table).
///
/// Previously the backend's two hand-written lists evolved separately: five connectors got sync branches and showed up in the UI, but not in the
/// creation allowlist, so the UI let you pick them and creation failed with "kind must be one of…" (#247). Variant order is
/// the order in the dialog. The string form is generated by strum in snake_case, no longer hand-written
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumIter,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum SourceKind {
    Folder,
    Url,
    Rss,
    GithubIssues,
    GithubRepo,
    /// Files from a gitlab.com project's default branch (#1327); self-hosted GitLab not verified
    GitlabRepo,
    /// Issues and merge requests for gitlab.com projects (#1327); self-hosted GitLab not verified
    GitlabIssues,
    JiraIssues,
    S3,
    AzureBlob,
    Gcs,
    Webdav,
    Notion,
    /// One person authorizes, syncing the folders they choose (0074)
    GoogleDrive,
    Api,
    /// What gets pushed is not a document but the statement itself (0054): the request body is the open extraction contract, and extraction does not ask a model
    Statements,
    Custom,
    /// Each library's own memory source; cannot be created or deleted (0015)
    Memory,
    /// Default value of `sources.kind` for old data; there is no corresponding UI
    Upload,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    pub fn all() -> impl Iterator<Item = Self> {
        <Self as strum::IntoEnumIterator>::iter()
    }

    /// Those a person can create from the UI: everything except `memory` and `upload`
    pub fn creatable_by_hand(self) -> bool {
        !matches!(self, Self::Memory | Self::Upload)
    }

    pub fn creatable() -> impl Iterator<Item = Self> {
        Self::all().filter(|k| k.creatable_by_hand())
    }
}

/// Source sync run record (channel audit history).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SyncRun {
    pub id: Uuid,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// running | ok | failed
    pub status: String,
    pub created_docs: i32,
    pub updated_docs: i32,
    pub error: Option<String>,
}

/// Extraction output view of a chunk (document viewer right column: what this chunk extracted).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChunkFactView {
    pub chunk_id: Uuid,
    pub fact_id: Uuid,
    pub subject_id: Uuid,
    pub subject: String,
    /// When the ontology did not recognize this relation, fall back to the wording in the original text; None when neither can provide it (older historical data looks like this)
    pub predicate: Option<String>,
    pub inferred: bool,
    /// A live open statement can supply a human-confirmed regression expectation.
    pub is_open_statement: bool,
    pub object_id: Option<Uuid>,
    pub object: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub confidence: f32,
}

/// Source list view (with document count).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SourceView {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub config: serde_json::Value,
    pub icon: Option<String>,
    pub sync_interval_minutes: Option<i32>,
    pub sync_cron: Option<String>,
    pub decay: Option<serde_json::Value>,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_sync_status: String,
    pub last_sync_error: Option<String>,
    pub last_sync_added: i32,
    /// Where the ticket source has read up to (ADR 0072). See [`Source::sync_checkpoint`]
    pub sync_checkpoint: Option<DateTime<Utc>>,
    pub doc_count: i64,
    /// Number of documents marked "not in the source" (produced by url full-set reconciliation / custom tombstones)
    pub missing_count: i64,
    /// The full-text backfill block. **Not NULL as a whole for RSS sources** (0033 decision 2 / #417).
    /// Previously these were eight flat columns, and "not applicable" was written as NULL in three of them and as 0 in the other five:
    /// a folder source would report `queued_count: 0`, which speaks of a queue it does not have at all.
    /// Applicability is now decided by whether this cell is present
    #[sqlx(json(nullable))]
    pub rss_full_content: Option<RssFullContentSummary>,
}

/// Full-text backfill progress for an RSS source's current generation.
///
/// The five counts only make sense together (the Library status bar reads them in one go), so they travel together.
/// `state` is the tier the server computed; callers need not derive it again from kind and content_mode.
///
/// **`generation` and `baseline_count` are not here** (0033 decision 2): the generation number is internal state,
/// and the baseline batch does not belong to the five "how much work is left" numbers. It is a starting point, not progress.
/// Places that need them read `rss_full_content::counts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RssFullContentSummary {
    /// `pending` (baseline not yet built) | `active` | `disabled` (is RSS, but full text is not enabled)
    pub state: String,
    pub pending: i64,
    /// queued and hydrating are merged into one cell
    pub queued: i64,
    pub retrying: i64,
    pub complete: i64,
    /// terminal, deleted and superseded are merged into one cell
    pub terminal: i64,
}

/// Audit event view (with actor display name; NULL after the account is deleted). Purely for audit display.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AuditEventView {
    pub id: Uuid,
    pub action: String,
    pub target_kind: String,
    pub target_id: Option<Uuid>,
    pub detail: serde_json::Value,
    /// NULL = the engine did it automatically (reconciliation merges, consistency checks, inference materialization...). The UI uses it to tell
    /// "no one" apart from "the person was removed": the latter keeps actor_id, only the display name cannot be looked up
    pub actor_id: Option<Uuid>,
    pub actor_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Account-level "my knowledge bases" row info (the membership row may be empty: an open library is entered with the deployment identity, with no matrix record).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MyKbInfo {
    pub kb_id: Uuid,
    pub member_role: Option<String>,
    pub joined_at: Option<DateTime<Utc>>,
    pub added_by_name: Option<String>,
    pub doc_count: i64,
    pub member_count: i64,
}

/// Chat session row (left column list).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConversationView {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: i64,
}

/// Chat message (including the persisted action trail and citations, used for history playback).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConversationMessage {
    pub id: Uuid,
    pub role: String,
    pub content: String,
    pub steps: serde_json::Value,
    pub sources: serde_json::Value,
    pub stopped: bool,
    pub created_at: DateTime<Utc>,
}

/// Chunk view for search results (with document info).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChunkView {
    pub id: Uuid,
    pub document_id: Uuid,
    pub seq: i32,
    pub text: String,
    pub filename: String,
}

/// A chunk carries which source it came from, together with the source's kind and the document's `external_key`. For a push source the
/// `external_id` is reverse-derived from these two (`sources_routes::pushed_identity`, #1271)
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SourcedChunk {
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub chunk: ChunkView,
    pub source_id: Option<Uuid>,
    #[serde(skip)]
    pub source_kind: Option<String>,
    #[serde(skip)]
    pub external_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct LlmSettings {
    pub workspace_id: Uuid,
    pub chat_base_url: Option<String>,
    #[serde(skip_serializing)]
    pub chat_api_key: Option<String>,
    pub chat_model: Option<String>,
    pub embed_base_url: Option<String>,
    #[serde(skip_serializing)]
    pub embed_api_key: Option<String>,
    pub embed_model: Option<String>,
    pub embed_dim: Option<i32>,
    pub updated_at: DateTime<Utc>,
    /// Layout recognition service for reading scanned documents and images (MinerU, 0040); empty = not configured, those file types degrade
    pub ocr_base_url: Option<String>,
    #[serde(skip_serializing)]
    pub ocr_api_key: Option<String>,
    pub ocr_backend: Option<String>,
    /// Which protocol the service reading scanned documents speaks (0065): `mineru` submits a job and polls, `ark` sends each page to a vision model.
    /// Existing rows default to `mineru`
    pub ocr_provider: String,
    /// Vision model name for the Ark path; MinerU does not use it
    pub ocr_model: Option<String>,
    /// Transcription model that labels speakers (OpenAI `/audio/transcriptions` + `diarized_json`, 0040)
    pub transcribe_base_url: Option<String>,
    #[serde(skip_serializing)]
    pub transcribe_api_key: Option<String>,
    pub transcribe_model: Option<String>,
    /// Reasoning effort for the chat model (`reasoning_effort` on the OpenAI-compatible interface): none | minimal | low | medium |
    /// high; empty = field not sent. **Every call uses it:** extraction, alignment, rule drafting, the ontology proxy, errata. No step
    /// defines its own. Which value turns thinking off depends on the endpoint: some accept minimal, others (DeepSeek) accept only none
    pub chat_reasoning_effort: Option<String>,
}

impl LlmSettings {
    pub fn chat_ready(&self) -> bool {
        self.chat_base_url.is_some() && self.chat_model.is_some()
    }
    pub fn embed_ready(&self) -> bool {
        self.embed_base_url.is_some() && self.embed_model.is_some()
    }
    /// An address for MinerU is enough; Ark also needs a model name and a key (it is a paid model API)
    pub fn ocr_ready(&self) -> bool {
        self.ocr_base_url.is_some()
            && match self.ocr_provider.as_str() {
                "mineru" => true,
                "ark" => {
                    self.ocr_model
                        .as_deref()
                        .is_some_and(|m| !m.trim().is_empty())
                        && self.ocr_api_key.as_deref().is_some_and(|k| !k.is_empty())
                }
                _ => false,
            }
    }
    pub fn transcribe_ready(&self) -> bool {
        self.transcribe_base_url.is_some() && self.transcribe_model.is_some()
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EntityType {
    pub id: Uuid,
    pub kb_id: Uuid,
    pub key: String,
    pub label: String,
    pub color: String,
    /// Graph node shape: circle | square
    pub shape: String,
    pub builtin: bool,
    /// subClassOf hierarchy (lit up by axiomatic reasoning P4; the editor maintains the data first)
    /// All parent classes (subClassOf may have several: FOAF's Person is both an Agent and a SpatialThing)
    pub parents: Vec<Uuid>,
    /// Which branch the class hangs under when the tree is drawn in the left column. It has no semantic role, only for display
    pub primary_parent: Option<Uuid>,
    /// Global identity of an OWL import. Hand-built classes are NULL; re-import matches on it, not on key,
    /// because upstream changing an rdfs:label once changes the derived key, and matching by key would create the same class as a new one
    pub iri: Option<String>,
    /// Semantic guidance: injected into the extraction prompt (what counts as this class, with examples)
    pub description: String,
}

/// Ontology editor: entity instance rows under a class (used for the instance list in the detail pane).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EntityInstance {
    pub id: Uuid,
    pub name: String,
    pub fact_count: i64,
}

/// Ontology editor view: type + usage (for delete protection and UX hints).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EntityTypeView {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub color: String,
    pub shape: String,
    pub builtin: bool,
    /// All parent classes (subClassOf may have several: FOAF's Person is both an Agent and a SpatialThing)
    pub parents: Vec<Uuid>,
    /// Which branch the class hangs under when the tree is drawn in the left column. It has no semantic role, only for display
    pub primary_parent: Option<Uuid>,
    /// Classes mutually exclusive with this class: **declares "cannot both be"**. The consistency check uses it to report
    /// unsatisfiable classes. A class inheriting two mutually exclusive ancestors can never have instances (0002)
    pub disjoint: Vec<Uuid>,
    pub description: String,
    /// How the type asks to be shown (RC-008): a flat object of string values, the
    /// `utopia:*` literals its ontology file declared. `{}` when it says nothing
    pub display_hints: serde_json::Value,
    pub usage: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RelationTypeView {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub temporal: String,
    pub functional: bool,
    pub inverse_functional: bool,
    /// The other four OWL axioms. **All of the reasoner's criteria live here** (0002). Previously they could only be
    /// brought in by importing OWL, so a person building an ontology in the UI could never turn that machine on
    pub is_transitive: bool,
    pub is_symmetric: bool,
    pub is_asymmetric: bool,
    pub is_irreflexive: bool,
    /// The two that point at another relation. **They must be returned to the UI**: the dropdown must show who is currently selected,
    /// otherwise every time the form opens it is empty, and one edit wipes out what was already declared
    pub inverse_of: Option<Uuid>,
    pub sub_property_of: Option<Uuid>,
    pub builtin: bool,
    pub description: String,
    /// relation (object is an entity) | attribute (object is a literal value)
    pub kind: String,
    /// Classes that can be subjects. attribute requires at least one; relation may be empty (unset = unrestricted)
    pub domains: Vec<Uuid>,
    /// Classes that can be objects. **Only meaningful for relation**: an attribute's value range is a literal datatype,
    /// which lives on the datatype
    pub ranges: Vec<Uuid>,
    /// Which qualifier properties this relation's edges can carry (0037): the property definition ids.
    /// **The ontology API never returned this cell.** The first cut of 0037 added it to `graph::relation_types`
    /// and the frontend types but missed this view, so when the ontology page opened a relation the frontend read undefined
    /// and threw directly (`rel.qualifiers.length`).
    pub qualifiers: Vec<Uuid>,
    /// attribute only: text | number | date | bool
    pub datatype: Option<String>,
    pub unit: Option<String>,
    /// How the property asks to be shown (RC-008): a flat object of string values
    pub display_hints: serde_json::Value,
    pub usage: i64,
}

/// A property value attached to one edge (0037). Either `value` or `entity`:
/// amounts, ratios and dates are literal values; the C in "matched through C" is an entity (this cell is not written in this cut, the slot is kept)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FactQualifier {
    pub qualifier_type_id: Uuid,
    pub key: String,
    pub label: String,
    /// Same shape as `facts.object_value`: {"value": …, "unit": …}
    pub value: Option<serde_json::Value>,
    pub entity_id: Option<Uuid>,
    pub entity_name: Option<String>,
}

/// A property on an open statement (0044 first cut, 0061): recorded by the document's role word, not by ontology property. There is no
/// `qualifier_type_id`, so it does not join [`FactQualifier`] (RDF export builds IRIs from that cell).
/// Either a value or an entity; the entity cell carries its display name. Typed rows materialized from it carry a copy of that
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatementQualifier {
    /// The document's own role word ("amount", "buyer")
    pub role: String,
    /// Same shape as [`FactQualifier::value`]: {"value": …}
    pub value: Option<serde_json::Value>,
    pub entity_id: Option<Uuid>,
    pub entity_name: Option<String>,
    /// The chunks that support this value (0070): two live chunks may phrase it differently, each gets its own row
    pub chunk_ids: Vec<Uuid>,
}

/// Record of one OWL import. The source text is stored content-addressed in a blob; this row is just the ledger.
/// `summary` records what that projection did, including **axioms not yet projected**. When a consumer is added later,
/// it is known from this which imported values are worth re-running.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OntologyImportView {
    pub id: Uuid,
    pub filename: String,
    pub format: String,
    pub byte_size: i64,
    pub summary: serde_json::Value,
    pub imported_at: DateTime<Utc>,
    pub imported_by_name: Option<String>,
}

/// Concurrency cap for a model. The constraint comes from the vendor's rate limit, which is computed per (base_url, model);
/// using the same number for a local Ollama and a hosted API was wrong from the start.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ModelLimit {
    pub base_url: String,
    pub model: String,
    pub max_concurrent: i32,
}

/// Extraction drop signal: facts were extracted but could not land, and the reason why.
/// Kept separate from `OntologyMiss`. That one says "your ontology lacks these" (the reader is the ontology maintainer);
/// this one says "these facts did not land" (the reader is the person who uploaded the document).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ExtractionDrop {
    pub document_id: Uuid,
    pub reason: String,
    pub detail: String,
    pub count: i32,
    pub example: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RelationType {
    pub id: Uuid,
    pub kb_id: Uuid,
    pub key: String,
    pub label: String,
    /// state | event | eternal
    pub temporal: String,
    /// Unique on the subject side: at any moment a subject has at most one object
    pub functional: bool,
    /// Unique on the object side: at any moment an object has at most one subject (e.g. a project has only one person leading it)
    pub inverse_functional: bool,
    pub builtin: bool,
    /// Semantic guidance: injected into the extraction prompt
    pub description: String,
    /// Global identity of an OWL import; hand-built ones are NULL. Re-import matches on it, not on key,
    /// because upstream changing an rdfs:label once changes the derived key, and matching by key would treat the same one as new
    pub iri: Option<String>,
    /// relation (object is an entity) | attribute (object is a literal value, goes through facts.object_value)
    pub kind: String,
    /// Classes that can be subjects (multi-valued: in OWL it is normal for a property to have several rdfs:domain)
    pub domains: Vec<Uuid>,
    /// Classes that can be objects. Only meaningful for relation
    pub ranges: Vec<Uuid>,
    /// attribute only: text | number | date | bool
    pub datatype: Option<String>,
    pub unit: Option<String>,
    /// **Which qualifier properties this relation's edges can carry** (0037): points to rows with kind='attribute'.
    /// The "amount" on `A invested B` is a property of the edge itself, not a second object. The amount's
    /// datatype / unit / conversion all reuse the property definition; only its domain is a relation rather than a class
    pub qualifiers: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Entity {
    pub id: Uuid,
    pub kb_id: Uuid,
    /// `None` = not yet determined. See `docs/decisions/0009`. It is not a class:
    /// it is the state "the extractor pulled something out, but the ontology has no corresponding class"
    pub type_id: Option<Uuid>,
    pub canonical_name: String,
    pub merged_into: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Graph render node.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct GraphNode {
    pub id: Uuid,
    pub name: String,
    /// Type key. **May be absent** (0009: undetermined means NULL),
    /// so the frontend shows "uncategorized" instead of inventing a name
    pub type_key: Option<String>,
    pub type_label: Option<String>,
    pub color: String,
    /// Type shape: circle | square
    pub shape: String,
    pub degree: i64,
    /// Display disambiguation suffix for same-named entities side by side (e.g. the owning organization's name)
    pub disambiguator: Option<String>,
}

/// Graph render edge (= one live fact).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct GraphEdge {
    pub id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    /// When the ontology has no corresponding relation, falls back to the wording in the original text (see `facts.predicate_id`).
    /// None when neither source can provide it. That is old data from before add_evidence recorded the original wording
    pub predicate: Option<String>,
    pub label: Option<String>,
    /// The original statement wording of the open statement this typed edge was derived from (0044 decision 1, #755).
    /// The canvas draws one edge per statement: if there is a typed row it draws that, labelled with the property's name. The original wording must not
    /// disappear from the picture, so it rides in this cell. When several statements produce the same row, it is their wordings, deduplicated and joined
    pub said_as: Option<String>,
    /// true = this edge's name comes from the original text, not a relation the ontology recognized. The UI must show the difference
    pub inferred: bool,
    /// true = this edge is **derived**, not asserted by anyone (R1, lives in `derived_facts`).
    ///
    /// **Not the same as `inferred`**, though the two words are close: that flag says "the name came from the source text
    /// rather than the ontology"; this one says "this edge is not anyone's claim at all, the engine inferred it"
    pub derived: bool,
    /// Properties on the edge (0037): the canvas needs this to write the amount on the edge label
    #[sqlx(skip)]
    pub qualifiers: Vec<FactQualifier>,
    /// Properties recorded by the document's role words (0061, #1267): kept apart from `qualifiers`, empty list when absent
    #[sqlx(skip)]
    pub statement_qualifiers: Vec<StatementQualifier>,
    /// The rule that inferred it (`transitive` / `symmetric` / `inverse` / `sub_property`);
    /// None for asserted edges.
    ///
    /// **The UI needs to tell `inverse` apart**: `A works_at B` and the `B employs A` it infers are
    /// two wordings of the same thing; drawing two edges just draws the redundancy twice. Whereas `sub_property`
    /// infers a fact of a different granularity, which should be drawn separately
    pub rule: Option<String>,
    /// Premise facts used to infer it (in proof order). Empty for asserted edges.
    ///
    /// **Edge merging in the UI depends on this to identify the source correctly.** Matching only by "same pair of nodes" would attach `contains`
    /// to an `allied_with` that happens to connect the same two points. That statement belongs to `part_of`,
    /// and the wrong attachment looks perfectly normal, which is the hardest kind to spot
    pub premises: Vec<Uuid>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    /// **Read-out** interval (0022): a fact with no start begins at the earliest evidence; one with no known end
    /// ends at the document that first stated it. The slider filters on these two and no longer interprets NULL itself.
    /// The two above are what the text said, used only for display
    pub holds_from: Option<DateTime<Utc>>,
    pub holds_to: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// Disputed (0017 §3): an open axiom violation or temporal conflict points at it. The whole edge is drawn in
    /// warning colour: the ring is on the node, the edge is still grey, and peripheral vision cannot tell them apart
    pub contested: bool,
    /// Ghost edge (0017 §3): a **non-landed** derivation, inferred but colliding with an assertion. `id` is the id of that
    /// `derived_contradiction` violation, not any fact; `derived` is also true,
    /// so it follows the derived toggle
    pub blocked: bool,
}

/// One name of an entity (0041): a value fact on `known_as`, shown in its own column, not mixed into the fact rows.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct NameView {
    pub fact_id: Uuid,
    pub name: String,
    /// The name shown in the UI (`entities.canonical_name`)
    pub canonical: bool,
    pub recorded_at: DateTime<Utc>,
    /// World axis: a former name has an end here
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    pub document_ids: Vec<Uuid>,
    pub evidence_count: i64,
}

/// Fact rows for the entity detail page (timeline).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EntityFact {
    pub id: Uuid,
    /// The original statement wording of the open statement this typed fact was derived from (#755)
    pub said_as: Option<String>,
    pub recorded_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub supersedes: Option<Uuid>,
    pub document_ids: Vec<Uuid>,
    /// out = this entity is the subject; in = it is the object
    pub direction: String,
    /// When the ontology did not recognize this relation, fall back to the wording in the original text; None when neither can provide it (older historical data looks like this)
    pub predicate_key: Option<String>,
    pub predicate_label: Option<String>,
    /// true = this fact's name comes from the original text, not a relation the ontology recognized. The UI must show the difference
    pub inferred: bool,
    /// Temporal category of the relation (point/state/eternal). Meaningless without a predicate, so None
    pub temporal: Option<String>,
    pub other_id: Option<Uuid>,
    pub other_name: Option<String>,
    /// Type label of the other-end entity; None for attribute facts, which have no other end
    pub other_type: Option<String>,
    /// Literal value object (attribute facts / question-answering mapping): {"value":…,"unit":…} or {"summary":…}
    pub object_value: Option<serde_json::Value>,
    /// Properties on the edge (0037). Not in the row: they live in a separate `fact_qualifiers` table, filled in by fact id after loading
    #[sqlx(skip)]
    pub qualifiers: Vec<FactQualifier>,
    /// Properties recorded by document role words (0061, #1267): the `statement_qualifiers` table, likewise filled in after loading
    #[sqlx(skip)]
    pub statement_qualifiers: Vec<StatementQualifier>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Precision describes the granularity of **those dates the fact has**. None when neither end has a date.
    /// Previously this was NOT NULL DEFAULT day, so facts with no date also claimed day-level precision (see `facts.valid_from_precision`)
    /// Granularity of the start end: year | month | day. None when there is no valid_from
    pub valid_from_precision: Option<String>,
    /// Granularity of the end, plus one extra `unknown`: **the text says it ended, but not which day**.
    /// Only when both `valid_to` and this are None is it "still ongoing" (see `facts.valid_to_precision`)
    pub valid_to_precision: Option<String>,
    /// **Read-out** interval (0022), same meaning as `GraphEdge`: the panel judges "holds right now" by it,
    /// rather than interpreting NULL as open itself
    pub holds_from: Option<DateTime<Utc>>,
    pub holds_to: Option<DateTime<Utc>>,
    /// The witness comes from a date in the document, quoted in the document's wording (0064): "submitted 4 September 2026". For a fact with no start,
    /// `holds_from` is this witness's date. It is "as of", not a start
    #[sqlx(default)]
    pub attested_by: Option<String>,
    pub confidence: f32,
    pub evidence_count: i64,
    /// All evidence rests on an old version of the source document (not confirmed by current content; does not mean the fact is invalid)
    pub stale: bool,
    /// Correction row (on the supersedes chain): the interval closure came from engine reconciliation or a manual ruling, not from the extracted text
    pub corrected: bool,
    /// The end is derived from the timeline (`facts.end_derived`): when the next fact starts, the engine closes this one; the text did not say
    /// that day (#970)
    pub end_derived: bool,
    /// `valid_to` is still the stored boundary; true says the text talks about the period that ended before it (`facts.end_after_period`,
    /// #994). "Left in 2024" stores 2025-01-01, while the text says 2024
    pub end_after_period: bool,
    /// The interval was edited by a person (#970): `corrected_ends` has a value. Unlike `corrected`, which is set by any
    /// rewrite (automatic chaining, review ruling, manual edit)
    pub time_corrected: bool,
    /// Which end a person edited: `start` / `end` / `both` (`facts.corrected_ends`, written with the correction row). When the end was later
    /// derived from the timeline, it no longer counts as an end edited by a person
    pub corrected_ends: Option<String>,
    /// The row attached when the timeline closed it (#970 step two): only present when `end_derived` and it can be found
    pub closed_by_id: Option<Uuid>,
    /// The new value of the end that changed on that row: the successor, or the name of the new object entity
    pub closed_by: Option<String>,
    /// The new value of the attached row (for attribute facts)
    pub closed_by_value: Option<serde_json::Value>,
    /// Note a person wrote when editing the interval: the most recent correction on the chain
    pub correction_note: Option<String>,
    /// Latest document time in the evidence set: the "last confirmed time" of an open fact (transparency of recency)
    pub last_evidence_time: Option<DateTime<Utc>>,
    /// Disputed (0017 §3): `{ kind, ref_id, derived? }`: which kind (the violation's kind, or
    /// `temporal_conflict`), the id of that item in Review, and the sentence inferred when a derivation collides with an assertion.
    /// Only the latest is reported in one place; the row is **not dimmed**, the assertion is still alive
    pub contested: Option<serde_json::Value>,
}

/// One epistemic change to an entity (an event on the record-time axis, orthogonal to the valid-time axis of EntityFact).
///
/// The ledger is append-only, so "how we once thought" is fully retained: one fact row produces at most two
/// events, the write (asserted / corrected) and the retraction (rejected, only when there is no successor correction row;
/// if there is a successor, this death is already explained by that corrected event, so it is not recorded again).
///
/// **Not every event comes from a fact.** Retypes (`retyped` / `retype_reverted`) come from
/// `entity_retypes`: it has no predicate, no counterpart, no direction, so those fields are nullable.
/// Previously only fact events existed, so "the class changed" was entirely invisible in entity history. 0001 P3a noted
/// "revocable is not the same as will be revoked": a mistake does not surface by itself.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EntityHistoryEvent {
    /// Only fact events have it. Retype events are None
    pub fact_id: Option<Uuid>,
    /// Record-time moment of the event (write = recorded_at, retraction = invalidated_at,
    /// retype = entity_retypes.created_at / reverted_at)
    pub at: DateTime<Utc>,
    /// asserted (first assertion) | corrected (interval edited) | rejected (belief overturned)
    /// | merged (merged into another assertion) | retyped (class changed) | retype_reverted (retype undone)
    pub kind: String,
    pub direction: Option<String>,
    pub predicate_label: Option<String>,
    pub other_name: Option<String>,
    pub object_value: Option<serde_json::Value>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Precision describes the granularity of **those dates the fact has**. None when neither end has a date.
    /// Previously this was NOT NULL DEFAULT day, so facts with no date also claimed day-level precision (see `facts.valid_from_precision`)
    /// Granularity of the start end: year | month | day. None when there is no valid_from
    pub valid_from_precision: Option<String>,
    /// Granularity of the end, plus one extra `unknown`: **the text says it ended, but not which day**.
    /// Only when both `valid_to` and this are None is it "still ongoing" (see `facts.valid_to_precision`)
    pub valid_to_precision: Option<String>,
    pub confidence: Option<f32>,
    /// Human operator; NULL = the engine did it automatically (extraction writes / temporal reconciliation closure / high-confidence retype)
    pub actor_name: Option<String>,
    /// Audit action that triggered this change (fact.close / conflict.close_old / fact.reject …)
    pub action: Option<String>,
    pub document_id: Option<Uuid>,
    pub filename: Option<String>,
    pub quote: Option<String>,
    /// The two ends of a retype event. The start may be empty: after 0009, "from no class to a class" is the most common retype
    pub from_type_label: Option<String>,
    pub to_type_label: Option<String>,
}

/// Epistemic changes that happened across the whole library within a **record-time** window.
///
/// Same batch of events as `EntityHistoryEvent`, differing in two ways:
/// 1. the window is on the **epistemic axis** (recorded_at / invalidated_at), not locked to a single entity:
///    "what changed last quarter" has no a-priori entity to ask about;
/// 2. subject and object are both filled in (`direction` is a concept for "centred on an entity", and there is no centre here).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct GraphChange {
    pub fact_id: Uuid,
    /// Moment the event lands on the epistemic axis (write = recorded_at, retraction = invalidated_at)
    pub at: DateTime<Utc>,
    /// asserted (new assertion) | corrected (corrected the previous one) | rejected (overturned) | merged (folded into another)
    pub kind: String,
    pub subject_id: Uuid,
    pub subject_name: String,
    pub predicate_label: Option<String>,
    pub object_name: Option<String>,
    pub object_value: Option<serde_json::Value>,
    /// Previous value of a corrected event, from the row supersedes points to; that assertion may lie outside the query window.
    pub previous_object_name: Option<String>,
    pub previous_object_value: Option<serde_json::Value>,
    /// Which **valid-time** span this assertion describes. It is orthogonal to `at`, do not conflate them
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    /// Precision describes the granularity of **those dates the fact has**. None when neither end has a date.
    /// Previously this was NOT NULL DEFAULT day, so facts with no date also claimed day-level precision (see `facts.valid_from_precision`)
    /// Granularity of the start end: year | month | day. None when there is no valid_from
    pub valid_from_precision: Option<String>,
    /// Granularity of the end, plus one extra `unknown`: **the text says it ended, but not which day**.
    /// Only when both `valid_to` and this are None is it "still ongoing" (see `facts.valid_to_precision`)
    pub valid_to_precision: Option<String>,
    pub confidence: f32,
    pub document_id: Option<Uuid>,
    pub filename: Option<String>,
    pub quote: Option<String>,
    /// Where this quote came from (0040): stated / ocr / transcribed / described; empty when there is no evidence
    pub quote_origin: Option<String>,
}

/// Summary of one side of an entity under resolution review.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewSide {
    pub id: Uuid,
    pub name: String,
    /// None when no type was determined (0009). Colour has its own default: it is something the canvas must get
    pub type_label: Option<String>,
    pub color: String,
    pub disambiguator: Option<String>,
    pub degree: i64,
    pub top_facts: Vec<String>,
}

/// Open suggestion an agent left on a pair (0025): a label hangs on the card, and a person's ruling on the card
/// is the answer to it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ReviewProposal {
    pub id: Uuid,
    /// merge | keep | unsure
    pub action: String,
    pub confidence: f32,
    pub reason: Option<String>,
}

/// Entity resolution review item: a grey-zone pair of suspected same entities.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewItem {
    pub id: Uuid,
    pub score: f32,
    pub reason: Option<String>,
    /// adjudicating = waiting for LLM ruling; human = waiting for human final review
    pub stage: String,
    pub created_at: DateTime<Utc>,
    pub left: ReviewSide,
    pub right: ReviewSide,
    /// The agent's suggestion; None if absent
    pub proposal: Option<ReviewProposal>,
}

/// Merge log row (history section of the review page).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MergeLogView {
    pub id: Uuid,
    pub source_name: String,
    pub target_name: String,
    /// NULL = automatically merged by LLM
    pub merged_by_name: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub reverted_at: Option<DateTime<Utc>>,
}

/// Low-confidence fact review row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct FactReviewItem {
    pub id: Uuid,
    pub subject_name: String,
    pub predicate_label: Option<String>,
    pub object_name: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub confidence: f32,
    pub evidence_count: i64,
    pub quote: Option<String>,
}

/// Evidence for a fact (quoted sentence + location in the source).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EvidenceView {
    /// The predicate wording the model actually used in this chunk. After an out-of-vocabulary predicate is demoted to related_to,
    /// only "related" remains on the fact row. The original intent lives only here
    pub proposed_predicate: Option<String>,
    pub quote: Option<String>,
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub filename: String,
    pub seq: i32,
    /// Which version of the document the evidence came from
    pub doc_version: i32,
    /// The document has a newer version (the evidence rests on the old one; does not mean the fact is invalid)
    pub stale: bool,
    /// The document this evidence lives in has been deleted (#268). If the fact is still alive, it is because it has another source
    pub document_deleted: bool,
    /// Where the quote came from (0040): `stated` written in the file, `ocr` recognized on a scanned page, `transcribed`
    /// from a recording transcript, `described` a model's description of an image. The last has no original words to compare against
    pub origin: String,
    /// The engine or model that read this text; empty when there is no original text
    pub origin_model: Option<String>,
    /// Points back to the location in the original file: page number (and box), recording start/end ms and speaker, which page the image is on
    pub anchor: Option<serde_json::Value>,
}

/// How far the library has got (#313): the four pages' empty states share this one judgment.
///
/// Each page used to assume "already ready" on its own, so the empty states all said the same thing. The graph page
/// asked people to configure a model even while documents were being extracted; the chat page showed its greeting as usual even when no model was configured.
/// The user only hit the wall after asking the first question.
///
/// **Returns only booleans and counts.** The model setting comes from workspace settings, and that table requires workspace admin
/// to see (`settings_routes::get`). Ordinary members cannot see the configuration, but need to know whether it is configured,
/// so this only says "whether or not", without base_url, model name or any credential.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Readiness {
    /// The workspace has a chat model configured. Without it, neither extraction nor chat can run
    pub has_chat_model: bool,
    /// Number of live documents (tombstones do not count: deleted documents do not make the library "have something")
    pub documents: i64,
    /// Number of documents being parsed or extracted
    pub processing: i64,
    /// Number of documents whose parsing or extraction failed
    pub failed: i64,
    /// Number of entities in the graph. Documents are all in and extraction has finished, yet this is still 0: it means nothing was extracted at all
    pub entities: i64,
}

/// Temporal conflicts (the ones S3 automatic closure cannot decide): old fact vs new fact, a person rules on them on the Review page.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConflictView {
    pub id: Uuid,
    /// no_time | simultaneous | described_evidence (`low_confidence` was recorded by 0045 cut 3,
    /// and legacy rows still carry it)
    pub reason: String,
    pub created_at: DateTime<Utc>,
    pub predicate_label: String,
    /// Both sides' full triples: a conflict on the subject side changes the object, a conflict on the object side changes the subject
    pub old_fact_id: Uuid,
    pub old_subject: String,
    pub old_object: Option<String>,
    pub old_valid_from: Option<DateTime<Utc>>,
    pub old_valid_from_precision: Option<String>,
    pub new_fact_id: Uuid,
    pub new_subject: String,
    pub new_object: Option<String>,
    pub new_valid_from: Option<DateTime<Utc>>,
    pub new_valid_from_precision: Option<String>,
    pub new_confidence: f32,
}

/// Chunk view used by the document viewer.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ChunkFull {
    pub id: Uuid,
    pub seq: i32,
    pub text: String,
    /// Where this text came from (0040): the viewer highlights recognized words by it and jumps to that page by the anchor
    pub origin: String,
    pub origin_model: Option<String>,
    pub anchor: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct KnowledgeBase {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    /// `knowledge` | `memory` (Agent memory space)
    pub kind: String,
    pub description: Option<String>,
    /// open = everyone, by deployment role; restricted = visible only to the kb_members list
    pub visibility: String,
    /// The deployment's shared default space (the first library created): always open, cannot be deleted
    pub is_default: bool,
    /// When extraction meets a wording outside the ontology, whether the system may automatically add it to the ontology and rewrite the facts waiting on it.
    /// Default on: the ten default relations of a new library were not chosen by anyone, and until a person fills them in the graph is largely unusable.
    /// Turning it off does not affect "noticing": unmatched statistics keep accumulating and stay visible, they just become proposals you click through.
    pub auto_extend_ontology: bool,
    /// Which language the built-in ontology is seeded in, and which language new class/relation descriptions are written in (`en` | `zh`).
    /// **Follows the corpus, not the UI**: the reader of a description is the model reading these documents.
    /// See docs/decisions/0004.
    /// Whether inferred facts are written into the ledger (R1). **Default on** (0050): derived facts are marked,
    /// kept in their own section, and the whole batch can be revoked at any time. What changes is not the part of the ledger people wrote. Turned off, the cost is that
    /// a new library's graph keeps lacking transitive chains and symmetric pairs, and people must first discover this switch before they can see the edges they should see.
    pub materialize_inferences: bool,
    /// After extraction finishes, automatically schedule a round of type resolution (0016 C2). **Only the tier that refines within the original class subtree lands automatically**,
    /// cross-axis reclassifications are still left to people. Default on: on the benchmark the automatic tier hits 39/41 (#297), and every batch can be revoked
    pub auto_type_resolution: bool,
    /// Governance switch (0025, **default on**, see 0050): when on, the govern task goes through the duplicate pairs waiting on people first in, first out,
    /// reads the people's precedents in the ledger before ruling; when off, the task stops as soon as it sees a pair between two clusters
    pub governance: bool,
    /// The moment governance was turned on; the circuit breaker only counts withdrawals after it (0025 decision 9).
    /// A new library is born with governance on, so this cell equals the library creation time (0050)
    pub governance_since: Option<DateTime<Utc>>,
    /// How often to re-run inference (minutes). See `knowledge_bases.inference_interval_minutes`
    pub inference_interval_minutes: i32,
    /// When it was last run. **Answers "whether it was looked at last time", not "whether it changed last time"**
    pub last_inference_at: Option<DateTime<Utc>>,
    pub ontology_lang: String,
    /// Data description written by exploration from the schema: what a row is, keys, units, code values, time axis, similar columns.
    /// Only writes what the schema says; rewritten on every exploration
    pub data_description: Option<String>,
    /// Questions exploration is unsure about and needs the library owner to answer (JSON string array)
    pub data_questions: serde_json::Value,
    /// Conventions written by people: rules not in the schema, such as "test orders don't count" or "valid orders are 2/3/4".
    /// Exploration does not touch them. Measured: on the wide-table corpus, question answering without conventions gets 2/18, with them 14/18 (#520)
    pub data_conventions: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// KB member matrix row (the Members section of library Settings).
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct KbMemberView {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    /// viewer | editor | admin
    pub role: String,
}

/// Data source list view for question answering: the connection string is not sent down (credentials go in but never out); only a host:port/db summary is shown.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DataSourceView {
    pub id: Uuid,
    pub name: String,
    pub engine: String,
    /// Connection summary (host:port/db, no credentials)
    pub summary: String,
    pub created_at: DateTime<Utc>,
    pub last_test_at: Option<DateTime<Utc>>,
    pub last_test_ok: Option<bool>,
}

/// One candidate ontology row (class / relation / attribute) returned by vector search.
///
/// `distance` is cosine distance: the smaller, the closer. It is passed to the caller as is rather than first folded into a "similarity":
/// where the threshold should sit is for the consumer to decide from its own data, so it is not normalized here.
#[derive(Debug, Clone, Serialize)]
pub struct TypeCandidate {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub description: String,
    /// Only present on relation rows: `relation` or `attribute`
    pub kind: Option<String>,
    pub distance: f32,
}

/// What a metric definition looked like before a change.
///
/// **Stores a full snapshot of the version, not a diff** (0006): when reading, the question being answered is "what was it at that time",
/// and a diff can only answer that by replaying from the beginning. `before` is the row's `to_jsonb` from before the change,
/// with id and kb_id removed.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MappingRevision {
    pub id: Uuid,
    pub before: serde_json::Value,
    /// The person who changed it. **Bare foreign key + user soft delete**, so attribution is not lost when the person leaves;
    /// it is NULL only if the row was really hard-deleted
    pub changed_by_name: Option<String>,
    pub changed_at: DateTime<Utc>,
}

/// What one round of mapping exploration scanned, dropped, and kept (#503).
///
/// **It answers coverage**: eleven proposals against an eighty-column wide table look exactly the same in `concept_mappings` as
/// eleven that exactly cover a small library. The numerator is `tables_covered`,
/// the denominator is `tables_scanned`, and `schema_truncated` says whether incomplete coverage means "not seen"
/// or "seen but not proposed".
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ExplorationRun {
    pub id: Uuid,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub sources: Vec<String>,
    pub tables_scanned: i32,
    pub columns_scanned: i32,
    /// The schema text hit its limit: tables not in the prompt never had a chance to be proposed by the model
    pub schema_truncated: bool,
    /// How many proposals this round may make (scaled by the number of tables)
    pub cap: i32,
    /// How many the model returned / how many were stored. The difference is what got dropped; the details are in `dropped`
    pub returned: i32,
    pub accepted: i32,
    /// `{"source": {"n": 12, "example": "…"}, …}`, keys see
    /// `utopia_store::exploration_runs::drop_reason`
    pub dropped: serde_json::Value,
    pub tables_covered: Vec<String>,
    /// A round that failed also leaves a row: failure is not the same as "ran but proposed nothing"
    pub error: Option<String>,
}

/// One mapping in the semantic layer: business concept -> data asset definition (see `docs/decisions/0011`).
///
/// **Fields are columns, not keys in JSON.** Previously it was a `mapped_to` fact,
/// with all of these stuffed into `object_value`, so "which concepts map to the orders table"
/// required parsing JSON, and the rule "one concept, one row per source" was beyond the database's reach.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ConceptMapping {
    pub id: Uuid,
    pub concept_id: Uuid,
    /// The name of the concept. The read side always needs it (question-answering prompt, Review list),
    /// so looking up the entity table again each time is wasted work
    pub concept_name: String,
    /// The attached data source. The same concept may have different definitions on different sources; this is supported on purpose
    pub source: String,
    pub table_name: Option<String>,
    pub expr: Option<String>,
    pub sql: Option<String>,
    pub unit: Option<String>,
    pub summary: Option<String>,
    /// Derived metric ("conversion rate = orders / visits"): computed, not a column in a table
    pub derived: bool,
    /// proposed | confirmed | rejected
    ///
    /// **A state, not a confidence.** Previously the fact's confidence was borrowed to express "proposed 0.6 / confirmed 1.0",
    /// which encodes a binary state as a float, and also drops it into the "low-confidence fact" tier as a side effect
    pub status: String,
    /// The person who wrote a convention from scratch records the person; for exploration-proposed ones it is empty (#562). `decided_by` cannot tell this apart:
    /// an exploration proposal that a person later confirmed also has decided_by
    pub written_by: Option<Uuid>,
}

/// One axiom violation, with its triple text prepared for display (see `axiom_violations`).
///
/// **Both facts are expanded into subject-predicate-object text**: the Review page must let a person see at a glance where the contradiction is,
/// whereas two UUIDs show nothing. For the self-reflexive kind the two are identical: it is one fact.
#[derive(Debug, Clone, Serialize)]
pub struct AxiomViolation {
    pub id: Uuid,
    /// self_loop | asymmetry | cycle | functional | inverse_functional | signature | derived_contradiction
    pub kind: String,
    /// Which relation the criterion came from. If a person rules "the axiom is wrong", they enter the ontology from here to fix it
    pub predicate: Option<String>,
    pub left_fact: Uuid,
    pub left_text: String,
    pub right_fact: Uuid,
    pub right_text: String,
    /// The `path` length: facts on the cycle (including the endpoints), facts in a mutex group, derivation premises. Self-loops and signatures are 0
    pub path_len: i32,
    pub detected_at: chrono::DateTime<chrono::Utc>,
    /// Unique to `derived_contradiction` (0017): the inferred triple. It is not stored in the database,
    /// so it can only be written out here. Fields: see `reasoning::run`. Other kinds are `{}`
    pub detail: serde_json::Value,
    /// Review hint (0017 §2): `stale` (an old assertion with no end date), `duplicate` (same-named
    /// entity), `unsure` (low extraction confidence). Only one is given; empty if none
    pub hint: Option<String>,
    /// Every fact on the cycle, in order (empty for other kinds). **Ids are given per item**: retracting a fact must say which one,
    /// and which one on the cycle is wrong is only known once a person has looked (#202)
    pub path: Vec<ViolationFact>,
}

/// One fact in a violation: its id and triple text
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViolationFact {
    pub id: Uuid,
    pub text: String,
}

/// An internal self-contradiction of the ontology (see `ontology_defects`).
///
/// **Not the same as [`AxiomViolation`]**: that one says "the facts conflict with the definitions", this one says
/// "the definitions do not hold up on their own". The latter is more fundamental: an ontology that contradicts itself makes every conclusion of the former suspect.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OntologyDefect {
    pub id: Uuid,
    /// symmetric_and_asymmetric | transitive_and_functional | subclass_cycle
    /// | disjoint_with_ancestor | inherits_disjoint | inverse_of_itself
    /// | inverse_not_mutual | sub_property_cycle | rules_disagree
    pub kind: String,
    /// Unique to `rules_disagree` (0017): which two rules, which axiom they collide on, how many pairs, and how many examples
    pub detail: serde_json::Value,
    /// Label (class or predicate) of the object at fault. If it cannot be found, it has already been deleted
    pub subject_label: Option<String>,
    /// The other side: the mutually exclusive class
    pub other_label: Option<String>,
    /// Labels of the classes on the cycle, in order
    pub path_labels: Vec<String>,
    pub detected_at: chrono::DateTime<chrono::Utc>,
}

/// An inferred fact, together with its proof (the "inferred" tier of the entity panel).
///
/// **`premises` is the reason this tier exists**: without premises shown, a derived edge and an ordinary edge
/// look no different on screen, and that is exactly what "inference polluting knowledge" looks like.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DerivedFactView {
    pub id: Uuid,
    pub predicate_id: Uuid,
    pub object_value: Option<serde_json::Value>,
    pub rule_id: Option<Uuid>,
    pub attribute_rule_id: Option<Uuid>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to_precision: Option<String>,
    pub subject_id: Uuid,
    pub subject: String,
    /// A literal-valued conclusion (business-rule classification and attributes) has no entity object (0021)
    pub object_id: Option<Uuid>,
    pub object: String,
    pub predicate: String,
    /// transitive | symmetric | inverse | sub_property, or `business` (business rule)
    pub rule: String,
    /// The name of the business rule, or the kind of axiom together with its declared predicate (e.g. `inverse of works_for`).
    /// The declared predicate of an axiom such as inverse may differ from the conclusion's predicate, so the conclusion's predicate cannot be used as the explanation.
    pub rule_name: Option<String>,
    /// Which version of the business-rule definition it was inferred from (0060), and that version's definition itself. None for axiom-inferred facts
    pub rule_version: Option<i32>,
    pub rule_definition: Option<serde_json::Value>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub confidence: f32,
    pub derived_at: DateTime<Utc>,
    /// Direct premises, expanded into triple text in derivation order
    pub premises: Vec<String>,
}

/// A **non-landed** derivation (0017 §3): inferred, but it collided with an assertion and is kept out of the graph.
///
/// It has no id, since only stored ones do. It is referred to here by the id of that `derived_contradiction` violation,
/// and both the "not landed" tier in the panel and the ghost edge in the graph match the Review card by this id.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BlockedDerivation {
    pub violation_id: Uuid,
    pub subject_id: Uuid,
    pub subject: String,
    pub object_id: Uuid,
    pub object: String,
    pub predicate: String,
    pub rule: String,
    /// The predicate the declaration lives on
    pub via_label: String,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    /// The assertion that blocks it, and its triple text
    pub against_fact: Uuid,
    pub against_text: String,
    /// Premise fact ids in derivation order; the proof chain expands from here
    pub premises: Vec<Uuid>,
}

/// One step of a proof: a premise, together with its evidence (0002 R2).
///
/// A premise is either an assertion, whose leaf is its original sentence (chunk), or **another derivation** (0030),
/// in which case the evidence for that step is its own premises, one level further down in `premises`. So a proof is a tree,
/// and its depth shares the same upper limit as inference.
#[derive(Debug, Clone, Serialize)]
pub struct ProofStep {
    pub seq: i32,
    /// `facts.id` for an assertion, `derived_facts.id` for a derivation: see `derived`
    pub fact_id: Uuid,
    /// Whether this step itself was inferred (0030). **The UI must be able to tell**: an inferred premise and
    /// a premise read from text can look the same in a sentence, yet whether they can be traced back to the original text is entirely different
    pub derived: bool,
    pub subject_id: Uuid,
    pub subject: String,
    pub predicate_id: Option<Uuid>,
    /// The relation name in the ontology. Predicate-less facts (0010) do not take part in inference, so in theory this always has a value here;
    /// keeping it an Option avoids lying on the read path
    pub predicate: Option<String>,
    pub object_id: Option<Uuid>,
    pub object: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// This premise was later retracted. The derivation is invalidated with it, but the proof must still show "what it relied on at the time"
    pub retracted: bool,
    pub evidence: Vec<EvidenceView>,
    /// This step's own premises, by `seq` (0030). For an assertion step it is empty: its leaf is
    /// the original sentence in `evidence`, with nothing further to ask
    pub premises: Vec<ProofStep>,
}

/// Full proof of a derived fact: itself, plus the premises expanded down to the original sentences in order.
#[derive(Debug, Clone, Serialize)]
pub struct Proof {
    pub derived: DerivedFactView,
    pub steps: Vec<ProofStep>,
}

/// **Real counts** for each tier of the review queue.
///
/// Fetching them separately from the list is deliberate: the list is capped (ten per page), counting is not. Previously the left column read the
/// array length, but the API always returned only 100 items: a library with 164 pending items showed 100, and
/// after clearing them the number would reappear.
/// One fact waiting for a person to approve (0015). `quote` is the full text of that memory: the confirmation UI must show the original sentence and
/// the triple side by side, since listing only the triple would make a person judge its correctness out of thin air.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PendingFactView {
    pub id: Uuid,
    pub subject_id: Uuid,
    pub subject_name: String,
    pub predicate_id: Option<Uuid>,
    /// The relation name in the ontology; when empty the frontend shows `proposed_predicate` (italic, marking it as the original wording)
    pub predicate_label: Option<String>,
    pub proposed_predicate: Option<String>,
    pub object_id: Option<Uuid>,
    pub object_name: Option<String>,
    pub object_value: Option<serde_json::Value>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    pub confidence: f32,
    pub chunk_id: Uuid,
    pub quote: String,
    pub proposed_by: Option<Uuid>,
    pub proposed_by_name: Option<String>,
    /// When recorded via MCP, the name of that token (the name a person gave the agent in 0014).
    /// For memories recorded in the web chat this is empty: then "who said it" is the person themselves
    pub proposed_token_name: Option<String>,
    pub created_at: DateTime<Utc>,
    /// The document's own relation phrase (0044). Some = this is an open statement: once approved, it lands in `facts` with `layer = 'open'`,
    /// the phrase is written as is, there is no predicate and no `valid_*` is written; None = old ontology-shaped form
    pub phrase: Option<String>,
    /// Properties of an open statement, recorded by the document's role words: `[{"role": "amount", "value": "$2 million"} |
    /// {"role": "to", "entity_id": "<uuid>"}]`. Only meaningful when `phrase` is non-empty
    pub qualifiers: Option<serde_json::Value>,
    /// Time words mentioned in the statement, **copied verbatim, never a date** (0045):
    /// `[{"text": "March 4, 2011", "char_start": 143}]`, the offsets are character offsets into `chunks.text`
    pub time_words: Option<serde_json::Value>,
    /// Character offset (not bytes) of the quote in `chunks.text`, computed server-side by searching the text; NULL = not located
    pub quote_start: Option<i32>,
    pub quote_end: Option<i32>,
}

/// Who proposed a memory.
///
/// **Two layers, not one**: `user_id` is the person (the token represents them, 0014), and `token_id` is
/// which of their agents is attached to this library. The same person may connect three clients at once; recording only the person
/// would force the reviewer to guess among three identical "Zhang San said" entries.
#[derive(Debug, Clone, Copy, Default)]
pub struct Proposer {
    pub user_id: Option<Uuid>,
    /// None = not via MCP (web chat, or bulk ingestion)
    pub token_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, sqlx::FromRow)]
pub struct ReviewCounts {
    /// Facts extracted from memory, waiting for a person to approve (0015). Sorted first: they are what the person said themselves
    pub pending: i64,
    pub duplicates: i64,
    /// Among duplicate entries, the tier where both sides have the same type (both typed and equal): same name and same class is the tier a person most wants to bulk-merge first (#428)
    pub duplicates_same_type: i64,
    /// The tier where the two sides' types conflict (both typed and unequal): same name, different meaning; merging would be wrong
    pub duplicates_type_conflict: i64,
    pub conflicts: i64,
    pub unconfirmed: i64,
    pub lowconf: i64,
    pub mappings: i64,
    pub violations: i64,
    /// Signature and category word for which the two aligner votes disagree (the #725 alignment queue)
    pub alignment: i64,
    /// An action the errata agent was stopped by the gate from taking, waiting for a person to answer (0044 decision 7)
    pub errata: i64,
    pub defects: i64,
    pub merges: i64,
    /// Suggestion an agent wrote, waiting for a person to answer (0025)
    pub agent: i64,
    /// All of an agent's records (for paging the Agent queue)
    pub agent_rows: i64,
    /// This library's govern task is running right now
    pub agent_running: bool,
    /// Pairs for the agent to look at: among the pairs waiting on people, those with no suggestion yet
    pub agent_queue: i64,
}

/// One of the agent's entries (0025): which pair it looked at, what it wanted to do, on what basis, and how the person answered.
/// `left` / `right` are the names of that pair, still readable as the entities they were at the time, even after a merge
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AgentDecisionView {
    pub id: Uuid,
    pub run_id: Uuid,
    pub target_kind: String,
    pub target_id: Uuid,
    /// merge | keep | unsure
    pub action: String,
    pub confidence: f32,
    pub reason: Option<String>,
    pub precedents: serde_json::Value,
    /// proposed | applied | accepted | overridden | reverted | superseded
    pub status: String,
    pub merge_id: Option<Uuid>,
    /// The single question a deferral left for a person; only present on unsure rows
    pub question: Option<String>,
    /// What it looked at: [{tool, args, note}]
    pub trace: serde_json::Value,
    /// Number of model calls spent in the loop
    pub calls: i32,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by_name: Option<String>,
    pub left: Option<String>,
    pub right: Option<String>,
}

/// Result for one item in a bulk ruling: `error` being None means success. One failure does not drag down the rest,
/// and the caller gets a per-item explanation, so the UI can point and say "these two did not go through, and why"
#[derive(Debug, Clone, Serialize)]
pub struct ReviewBatchOutcome {
    pub id: Uuid,
    pub error: Option<String>,
}

/// Overview of the review desk (#377): what is waiting to be handled, what has been handled, and the health of the library.
///
/// The seven numbers in the left column only say "how many are open"; the overview answers the three questions a reviewer has on arrival:
/// **how many are waiting, for how long, and is the queue shrinking or growing**. Each of the three sections is its own query,
/// combined and returned in one response.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewSummary {
    pub waiting: ReviewWaiting,
    pub decided: ReviewDecided,
    pub health: ReviewHealth,
    pub agent: ReviewAgent,
}

/// What the agent has done in this library (0025): open suggestions, and the current state of each recent entry
#[derive(Debug, Clone, Serialize, Default)]
pub struct ReviewAgent {
    /// Running right now
    pub running: bool,
    /// Pairs the agent has not yet looked at
    pub queue: i64,
    /// Suggestions waiting for a person to answer, in no particular order
    pub open: i64,
    pub last_7d: AgentWindow,
    pub last_30d: AgentWindow,
}

/// Rows the agent wrote in a time window, counted by **current** state: the ones it auto-ruled that still stand,
/// those a person accepted, those a person overturned, and those a person withdrew
#[derive(Debug, Clone, Serialize, Default)]
pub struct AgentWindow {
    pub applied: i64,
    pub proposed: i64,
    pub accepted: i64,
    pub overridden: i64,
    pub reverted: i64,
}

/// What is waiting in one queue tier: how many, and since when the oldest one has been waiting
#[derive(Debug, Clone, Serialize, Default)]
pub struct QueueWait {
    pub count: i64,
    pub oldest_at: Option<DateTime<Utc>>,
}

/// The seven queue tiers, using the same definitions as [`ReviewCounts`] (the same WHERE clause), with an added "oldest"
#[derive(Debug, Clone, Serialize, Default)]
pub struct ReviewWaiting {
    pub pending: QueueWait,
    pub duplicates: QueueWait,
    pub conflicts: QueueWait,
    pub unconfirmed: QueueWait,
    pub lowconf: QueueWait,
    pub violations: QueueWait,
    pub defects: QueueWait,
    pub alignment: QueueWait,
    pub errata: QueueWait,
}

/// Handled: two windows, the last 7 days and the last 30 days, plus one bar per day for the last 14 days
#[derive(Debug, Clone, Serialize)]
pub struct ReviewDecided {
    pub last_7d: DecidedWindow,
    pub last_30d: DecidedWindow,
    /// The last 14 days, one entry per day, including days with no decisions (count 0): bars need equal spacing
    pub daily: Vec<DecidedDay>,
}

/// Decisions in a time window: the total, how many were AI auto-rulings (those with no actor in the ledger),
/// split by action and by person
#[derive(Debug, Clone, Serialize, Default)]
pub struct DecidedWindow {
    pub total: i64,
    pub automatic: i64,
    pub by_action: Vec<ActionCount>,
    pub by_actor: Vec<ActorCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionCount {
    /// Action name in the ledger (review.merge / fact.confirm / conflict.close_old …)
    pub action: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActorCount {
    /// None = background auto-ruling (batch rulings that have no client and no person)
    pub actor_id: Option<Uuid>,
    /// Identity snapshot from the ledger; recognizable even if the person has been deleted
    pub label: Option<String>,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecidedDay {
    pub day: chrono::NaiveDate,
    pub count: i64,
}

/// Library health: how many of the living facts are tentative (low confidence, all their evidence has been replaced,
/// or clashing with other facts)
#[derive(Debug, Clone, Serialize, Default)]
pub struct ReviewHealth {
    pub facts: i64,
    pub low_confidence: i64,
    pub unconfirmed: i64,
    pub contested: i64,
}

/// Which OWL axioms a relation declares.
///
/// **Passed as one bundle, not a string of arguments.** They belong to one family: the reasoner
/// (0002) uses them as its criteria, and they should appear side by side in the UI. Six bools spread across a parameter list
/// would sooner or later get passed in the wrong order, and the compiler can do nothing for `bool`s.
///
/// The last two are not bools: `inverseOf` and `subPropertyOf` point at **another relation**,
/// and in the UI they are dropdowns rather than checkboxes. Differing in shape does not change that they belong to this family: the reasoner's
/// four rule sources are exactly two of these six bits plus these two (0002).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationAxioms {
    /// Unique on the subject side (one person, one birthplace)
    pub functional: bool,
    /// Unique on the object side (one project, one leader)
    pub inverse_functional: bool,
    /// A→B ∧ B→C ⟹ A→C
    pub transitive: bool,
    /// A→B ⟹ B→A
    pub symmetric: bool,
    /// A→B ⟹ no B→A exists
    pub asymmetric: bool,
    /// No A→A exists
    pub irreflexive: bool,
    /// `p⁻¹ = q`: `A p B ⟹ B q A`. **Stored one way, used both ways**: normalized when the axiom is loaded
    /// (`reasoning::axioms`), so declaring it on one side is enough and the reverse follows automatically
    pub inverse_of: Option<Uuid>,
    /// `p ⊑ q`: `A p B ⟹ A q B`. A specific assertion implies the general one as well
    pub sub_property_of: Option<Uuid>,
}

/// One page of the library's document list, together with statistics beyond this page.
///
/// **The statistics are not affected by name/status filters**: `ready` / `done` / `extracting` / `failed` say
/// how many there are in this source; that is the scope of the bulk buttons, and unrelated to what you are searching for right now.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentPage {
    pub docs: Vec<Document>,
    /// Total number of hits for the filter (the pager uses it)
    pub total: i64,
    /// Number of documents ingested (`status = 'ready'`): the scope of a re-extract
    pub ready: i64,
    /// Number of documents extracted (`graph_status = 'done'`): the numerator of the extraction progress bar
    pub done: i64,
    pub extracting: i64,
    pub failed: i64,
    /// Number of tombstones across the library (deleted, not yet purged): the number on the "Deleted" row of the left column, does not change with scope
    pub deleted: i64,
}

/// Metadata of one personal access token (0014). **Never contains the plaintext**:
/// the plaintext exists only once, in the return value of `tokens::issue`; the database holds only the hash.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TokenView {
    pub id: Uuid,
    pub name: String,
    /// The short piece shown to people (`utp_pat_ab12`). Enough to match the string in a config file,
    /// but not enough to reconstruct it
    pub token_prefix: String,
    /// read | write. **The ceiling is not authorization**: effective permission = the person's role ∩ this scope
    pub scope: String,
    /// None = all libraries this person can enter
    pub kb_ids: Option<Vec<Uuid>>,
    pub expires_at: Option<DateTime<Utc>>,
    /// "Is this key still in use?" Must be answerable before revoking, otherwise nobody dares to revoke
    pub last_used_at: Option<DateTime<Utc>>,
    /// **Revocation is stamped, not deleted**: the fact of revoking itself must be left on record
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
