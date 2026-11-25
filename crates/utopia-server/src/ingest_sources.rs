//! Source sync task: url (web fetch) / rss (feed, pubDate → doc_time) /
//! github_issues / gitlab_issues / jira_issues (tickets, update instant → doc_time, the body carries the status-change history) /
//! s3 (object storage, LastModified → doc_time).
//! Everything goes through sha256 de-duplication (duplicate content is skipped silently); new documents enter the standard ingestion pipeline (process_document).
//! folder is a pure container (uploads go into it) and api is push-type: neither has pull semantics.

use crate::state::AppState;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use utopia_core::models::SourceKind;
use utopia_core::models::{Role, Source};
use utopia_core::AppError;
use uuid::Uuid;

#[cfg(test)]
#[path = "source_checkpoint_tests.rs"]
mod source_checkpoint_tests;

#[cfg(test)]
#[path = "timestamp_checkpoint_tests.rs"]
mod timestamp_checkpoint_tests;

/// Cap on new documents per sync (stops an overlong feed or URL list from dragging down the task)
const MAX_NEW_PER_SYNC: usize = 200;
const MAX_FEED_BYTES: usize = 4 * 1024 * 1024;
const RSS_MAX_INFLIGHT: i64 = 25;
const RSS_HYDRATION_ATTEMPTS: i32 = 5;

/// User-Agent for fetches. reqwest sends none by default, and Wikipedia explicitly refuses anonymous requests
/// (403), as do Cloudflare-fronted sites generally, which is why the URL and RSS source types broke for a large number of
/// real websites. Identifying ourselves is also crawler etiquette: site operators can recognise us and reach us.
pub(crate) const UA: &str = concat!(
    "Utopia/",
    env!("CARGO_PKG_VERSION"),
    " (+https://utopia.bi; self-hosted knowledge platform)"
);

/// Output stats of one sync (Moved/Unchanged are not counted).
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncStats {
    pub created: usize,
    pub updated: usize,
    pub discovered: usize,
    pub queued_for_content: usize,
    pub content_terminal: usize,
    /// This round did not finish because it hit the cap, so `since` must not advance (#1212 / ADR 0072's Custom)
    ///
    /// The `started_at` of the last successful round is the lower bound for the next; if this round were recorded as `ok`, the lower bound would jump past
    /// the entries in this batch that have not been read, and every later round could never reach them. Mark it here, so `sync_source` keeps
    /// the cursor where it is; this round's created / updated are still recorded in the audit as usual.
    pub incomplete: bool,
    /// The position this round reached (GitHub / Jira update instant, ADR 0072)
    pub checkpoint: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct RssObservation {
    key: String,
    title: String,
    article_url: Option<String>,
    summary: String,
    embedded_html: Option<String>,
    doc_time: Option<DateTime<Utc>>,
    has_usable_source: bool,
}

impl SyncStats {
    fn absorb(&mut self, action: IngestAction) {
        match action {
            IngestAction::Created => self.created += 1,
            IngestAction::Updated => self.updated += 1,
            _ => {}
        }
    }
    fn total(&self) -> usize {
        self.created + self.updated
    }
}

/// Ticket sources: progress is the instant in `sources.sync_checkpoint`, committed together with this round's success (0072).
/// This question is asked in three places, written in one place so that adding a fourth ticket source cannot miss one of them
fn reads_by_timestamp(kind: SourceKind) -> bool {
    matches!(
        kind,
        SourceKind::GithubIssues | SourceKind::GitlabIssues | SourceKind::JiraIssues
    )
}

pub async fn sync_source(state: &AppState, source_id: Uuid) -> anyhow::Result<SyncStats> {
    let source = utopia_store::sources::get(&state.pool, source_id).await?;
    let kind = SourceKind::parse(&source.kind);
    // #1212 / ADR 0072: the lower bound comes from **this source's own cursor**.
    //
    // - Ticket sources (GitHub / Jira): the cursor is the update instant consumed, and the whole group at that instant has been read.
    // - custom: the cursor is the instant **the whole response of this round finished**, advanced to (see `round_started_at` below).
    //
    // **The cursor is a column of the source itself, not inferred from the run.** The earlier lower bound came from the `started_at` of the last `ok` run,
    // `started_at`: that says when the previous round started, not how far it read; when a round did not finish,
    // the two are not the same thing, and once the lower bound passes unread entries, every later round can never reach them.
    //
    // The cursor is stored on the source, so it can be **cleared in one go** (ADR 0072's Recovery section): old sources' cursors are NULL, so they start over,
    // and deduplication of documents, versions and processing tasks means unchanged content does not
    // become a new version.
    let since = match kind {
        Some(kind) if kind == SourceKind::Custom || reads_by_timestamp(kind) => {
            utopia_store::sources::sync_checkpoint(&state.pool, source_id).await?
        }
        _ => None,
    };
    // **The instant this round started.** The custom cursor advances to here, not to "now":
    // changes arriving during the run must stay in the next round's window (ADR 0072's Custom section,
    // "preserving the existing protection for changes during the run").
    let round_started_at = Utc::now();
    utopia_store::sources::mark_running(&state.pool, source_id).await?;
    let run_id = utopia_store::sources::start_run(&state.pool, source_id).await?;
    state.emit_source(source.kb_id);

    // Exhaustive by enum: adding a source kind forces a decision here about how it syncs; the compiler will not let a missed branch slip through
    let outcome = match kind {
        Some(SourceKind::Url) => sync_urls(state, &source).await,
        Some(SourceKind::Rss) => sync_rss(state, &source).await,
        Some(SourceKind::Custom) => sync_custom(state, &source, since).await,
        Some(SourceKind::GithubIssues) => sync_github_issues(state, &source, since).await,
        Some(SourceKind::GithubRepo) => sync_github_repo(state, &source).await,
        Some(SourceKind::GitlabRepo) => sync_gitlab_repo(state, &source).await,
        Some(SourceKind::GitlabIssues) => sync_gitlab_issues(state, &source, since).await,
        Some(SourceKind::JiraIssues) => sync_jira_issues(state, &source, since).await,
        Some(SourceKind::S3 | SourceKind::AzureBlob | SourceKind::Gcs) => {
            sync_object_storage(state, &source).await
        }
        Some(SourceKind::Webdav) => sync_webdav(state, &source).await,
        Some(SourceKind::Notion) => sync_notion(state, &source).await,
        Some(SourceKind::GoogleDrive) => sync_google_drive(state, &source).await,
        // Passive containers: folder / api / memory / upload have no pull semantics
        Some(
            SourceKind::Folder
            | SourceKind::Api
            | SourceKind::Statements
            | SourceKind::Memory
            | SourceKind::Upload,
        ) => Ok(SyncStats::default()),
        None => Err(anyhow::anyhow!("unknown source kind `{}`", source.kind)),
    };

    // The round did not finish: the cursor stays where it is, but the parts this round did complete are still recorded in the audit
    // (ADR 0072's "Keep the committed counts visible for incomplete runs").
    // The run is written as `failed` rather than `ok`: the cursor advances only once the whole response for this round has been walked,
    // so when a round is unfinished, the next round resumes reading from where it stopped.
    if let Ok(stats) = &outcome {
        if stats.incomplete {
            let err = format!(
                "changed {MAX_NEW_PER_SYNC} items and stopped with more behind; \
                 the checkpoint stays where it was, so the next sync resumes here"
            );
            utopia_store::sources::finish_run(
                &state.pool,
                run_id,
                source_id,
                Some(&err),
                stats.created as i32,
                stats.updated as i32,
            )
            .await?;
            utopia_store::sources::finish_sync_quiet(
                &state.pool,
                source_id,
                &err,
                stats.total() as i32,
            )
            .await?;
            state.emit_source(source.kb_id);
            tracing::warn!(
                %source_id,
                kind = %source.kind,
                created = stats.created,
                updated = stats.updated,
                discovered = stats.discovered,
                "来源同步未读完，游标不前进"
            );
            return Ok(*stats);
        }
    }
    let outcome = match outcome {
        // custom: this round walked the **whole response**, so the cursor advances to **the instant this round started**.
        // Not advanced to now: changes that arrived during the run are still in the next round's window, because they have not been walked yet.
        // The round that hit the cap (`incomplete`) never gets here: the cursor stays where it is, and the next round resumes from there.
        Ok(stats) if matches!(kind, Some(SourceKind::Custom)) => {
            utopia_store::sources::finish_timestamp_sync(
                &state.pool,
                run_id,
                source_id,
                since,
                Some(round_started_at),
                stats.created as i32,
                stats.updated as i32,
            )
            .await
            .map(|()| stats)
            .map_err(anyhow::Error::from)
        }
        Ok(stats) if kind.is_some_and(reads_by_timestamp) => {
            utopia_store::sources::finish_timestamp_sync(
                &state.pool,
                run_id,
                source_id,
                since,
                stats.checkpoint,
                stats.created as i32,
                stats.updated as i32,
            )
            .await
            .map(|()| stats)
            .map_err(anyhow::Error::from)
        }
        other => other,
    };
    match outcome {
        Ok(stats) => {
            if !kind.is_some_and(|k| k == SourceKind::Custom || reads_by_timestamp(k)) {
                utopia_store::sources::finish_run(
                    &state.pool,
                    run_id,
                    source_id,
                    None,
                    stats.created as i32,
                    stats.updated as i32,
                )
                .await?;
                utopia_store::sources::finish_sync(
                    &state.pool,
                    source_id,
                    None,
                    stats.total() as i32,
                )
                .await?;
            }
            state.emit_source(source.kb_id);
            tracing::info!(
                %source_id,
                kind = %source.kind,
                created = stats.created,
                updated = stats.updated,
                discovered = stats.discovered,
                queued_for_content = stats.queued_for_content,
                content_terminal = stats.content_terminal,
                "来源同步完成"
            );
            Ok(stats)
        }
        Err(e) => {
            utopia_store::sources::finish_run(
                &state.pool,
                run_id,
                source_id,
                Some(&e.to_string()),
                0,
                0,
            )
            .await?;
            utopia_store::sources::finish_sync(&state.pool, source_id, Some(&e.to_string()), 0)
                .await?;
            state.emit_source(source.kb_id);
            Err(e)
        }
    }
}

/// Result of the three-way decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IngestAction {
    Created,
    Updated,
    Moved,
    Unchanged,
    /// Tombstone: marks "not in source" (does not delete the document; whether to delete is the user's decision in the UI)
    Tombstoned,
    /// The library already has a document with this same content (`documents_kb_sha_idx`): nothing is created or changed this time; points at that one
    Duplicate(Uuid),
}

/// Where identity comes from. `Path`: the source identifies documents by position, and the same content appearing at a new position is a move or rename, with history following it;
/// `Declared`: the caller gave the ID explicitly, and identity is that ID; never turn another live ID's document into it;
/// only the one already marked by a tombstone can be taken over (rename). `StableId`: the source's ID does not change with moves or renames,
/// so a new ID cannot take over another document by file name or content, even if the old file is temporarily out of the sync scope.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Identity {
    Path,
    Declared,
    StableId,
}

#[derive(Debug, Clone, Copy)]
pub struct IngestOutcome {
    pub action: IngestAction,
}

pub(crate) async fn write_blob(state: &AppState, sha256: &str, bytes: &[u8]) -> anyhow::Result<()> {
    state.blob.put(sha256, bytes).await
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Push with ordinary upload semantics (KB-level /ingest → Uploads): no source, no identity key,
/// no three-way decision: if the same content already exists in the KB it is treated as a no-op; everything else is created new.
pub async fn ingest_upload(
    state: &AppState,
    kb_id: Uuid,
    filename: &str,
    mime: &str,
    bytes: &[u8],
    doc_time: Option<DateTime<Utc>>,
) -> anyhow::Result<IngestAction> {
    if bytes.is_empty() {
        return Ok(IngestAction::Unchanged);
    }
    let sha256 = sha_hex(bytes);
    write_blob(state, &sha256, bytes).await?;
    match utopia_store::documents::create(
        &state.pool,
        kb_id,
        filename,
        mime,
        bytes.len() as i64,
        &sha256,
        None,
        doc_time,
        None,
    )
    .await
    {
        Ok(doc) => {
            utopia_store::jobs::enqueue(
                &state.pool,
                "process_document",
                serde_json::json!({ "document_id": doc.id }),
            )
            .await?;
            state.emit_document(kb_id, doc.id);
            Ok(IngestAction::Created)
        }
        Err(utopia_core::AppError::Conflict(_)) => Ok(IngestAction::Unchanged),
        Err(e) => Err(e.into()),
    }
}

/// Existing callers only need the action; hydration also needs the stable document UUID.
#[allow(clippy::too_many_arguments)]
pub async fn ingest_item(
    state: &AppState,
    kb_id: Uuid,
    source_id: Uuid,
    external_key: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
    doc_time: Option<DateTime<Utc>>,
) -> anyhow::Result<IngestAction> {
    Ok(ingest_item_with_outcome(
        state,
        kb_id,
        source_id,
        external_key,
        Identity::Path,
        filename,
        mime,
        bytes,
        doc_time,
    )
    .await?
    .action)
}

#[allow(clippy::too_many_arguments)]
pub async fn ingest_item_with_outcome(
    state: &AppState,
    kb_id: Uuid,
    source_id: Uuid,
    external_key: &str,
    identity: Identity,
    filename: &str,
    mime: &str,
    bytes: &[u8],
    doc_time: Option<DateTime<Utc>>,
) -> anyhow::Result<IngestOutcome> {
    if bytes.is_empty() {
        return Ok(IngestOutcome {
            action: IngestAction::Unchanged,
        });
    }
    let sha256 = sha_hex(bytes);

    // Primary decision: logical identity; fallback: pre-migration historical documents (no key) are claimed by file name, and the key is backfilled
    let mut existing =
        utopia_store::documents::find_by_external_key(&state.pool, source_id, external_key).await?;
    if existing.is_none() && identity != Identity::StableId {
        if let Some(legacy) =
            utopia_store::documents::find_legacy_by_filename(&state.pool, source_id, filename)
                .await?
        {
            utopia_store::documents::adopt_external_key(&state.pool, legacy.id, external_key)
                .await?;
            // When claiming, record the old content as version 1 (there were no version records before)
            utopia_store::documents::record_version(
                &state.pool,
                legacy.id,
                &legacy.sha256,
                legacy.size_bytes,
            )
            .await?;
            existing = Some(legacy);
        }
    }

    if let Some(doc) = existing {
        // A document pruned by the decay sweep: pushing the same content again does not restore it (RC-003); otherwise every round's re-push would
        // bring it back and the next sweep would delete it again. Changed content is new information and is restored as usual
        if doc.deleted_at.is_some()
            && doc.sha256 == sha256
            && utopia_store::decay::was_evicted(&state.pool, doc.id).await?
        {
            return Ok(IngestOutcome {
                action: IngestAction::Unchanged,
            });
        }
        // The new content is already held by another document in the library: answer duplicate first, so we do not find out only after restoring from the trash and rebuilding the index
        if doc.sha256 != sha256 {
            if let Some(holder) =
                utopia_store::documents::find_by_kb_sha(&state.pool, kb_id, &sha256).await?
            {
                return Ok(IngestOutcome {
                    action: IngestAction::Duplicate(holder.id),
                });
            }
        }
        // Ordinary source reappearance retains upstream restore semantics,
        // including search and derivation repair, before comparing content.
        let revived = doc.deleted_at.is_some();
        if revived {
            let restored = utopia_store::documents::restore(&state.pool, kb_id, doc.id).await?;
            crate::api::documents_routes::reindex(state, &restored).await?;
            crate::api::documents_routes::settle_derivations(state, kb_id).await?;
            let _ = utopia_store::audit::record_opt(
                &state.pool,
                Some(kb_id),
                None,
                "document.restored",
                "document",
                Some(doc.id),
                serde_json::json!({"filename":restored.filename,"via":"sync"}),
            )
            .await;
        }
        if doc.sha256 == sha256 {
            if revived {
                state.emit_document(kb_id, doc.id);
                return Ok(IngestOutcome {
                    action: IngestAction::Updated,
                });
            }
            return Ok(IngestOutcome {
                action: IngestAction::Unchanged,
            });
        }
        // Change: replaced in place, the old version goes into document_versions, committed together with the processing task
        write_blob(state, &sha256, bytes).await?;
        match utopia_store::documents::replace_content_and_enqueue_processing(
            &state.pool,
            doc.id,
            filename,
            mime,
            bytes.len() as i64,
            &sha256,
            doc_time,
        )
        .await
        {
            Ok(()) => {}
            Err(utopia_core::AppError::Conflict(_)) => {
                return duplicate_of(state, kb_id, &sha256).await
            }
            Err(e) => return Err(e.into()),
        }
        state.emit_document(kb_id, doc.id);
        return Ok(IngestOutcome {
            action: IngestAction::Updated,
        });
    }

    // Same content at a new path: recognised as a move/rename, and the pipeline is not re-run. A declared ID takes over only the one already marked by a tombstone
    // (tombstone the old ID first, then push the new ID = rename); a live one is never moved
    let moved = match identity {
        Identity::StableId => None,
        Identity::Path | Identity::Declared => {
            utopia_store::documents::find_by_source_sha(&state.pool, source_id, &sha256)
                .await?
                .filter(|doc| identity == Identity::Path || doc.missing_since.is_some())
        }
    };
    if let Some(doc) = moved {
        utopia_store::documents::update_location(&state.pool, doc.id, filename, external_key)
            .await?;
        state.emit_document(kb_id, doc.id);
        return Ok(IngestOutcome {
            action: IngestAction::Moved,
        });
    }

    write_blob(state, &sha256, bytes).await?;
    match utopia_store::documents::create_with_version_and_processing(
        &state.pool,
        kb_id,
        filename,
        mime,
        bytes.len() as i64,
        &sha256,
        Some(source_id),
        doc_time,
        Some(external_key),
    )
    .await
    {
        Ok(doc) => {
            state.emit_document(kb_id, doc.id);
            Ok(IngestOutcome {
                action: IngestAction::Created,
            })
        }
        // Same content already in the KB: do not fabricate a new identity; say honestly which document it duplicates
        Err(utopia_core::AppError::Conflict(_)) => duplicate_of(state, kb_id, &sha256).await,
        Err(e) => Err(e.into()),
    }
}

async fn duplicate_of(
    state: &AppState,
    kb_id: Uuid,
    sha256: &str,
) -> anyhow::Result<IngestOutcome> {
    let holder = utopia_store::documents::find_by_kb_sha(&state.pool, kb_id, sha256).await?;
    Ok(IngestOutcome {
        action: holder.map_or(IngestAction::Unchanged, |d| IngestAction::Duplicate(d.id)),
    })
}

async fn sync_urls(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    let urls: Vec<String> = source.config["urls"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if urls.is_empty() {
        anyhow::bail!("url source is missing config.urls (a list of page URLs)");
    }

    let mut stats = SyncStats::default();
    let mut last_err: Option<String> = None;
    for url in urls.iter().take(MAX_NEW_PER_SYNC) {
        match fetch_page(url).await {
            Ok((filename, mime, bytes)) => {
                // Logical identity = the URL itself: when page content changes, replace in place (history goes into the versions table)
                let action = ingest_item(
                    state,
                    source.kb_id,
                    source.id,
                    url,
                    &filename,
                    &mime,
                    &bytes,
                    None,
                )
                .await?;
                stats.absorb(action);
            }
            Err(e) => {
                tracing::warn!(%url, error = %e, "fetch failed");
                last_err = Some(format!("{url}: {e}"));
            }
        }
    }
    // Partial failure: if anything was produced, count it as success (errors go to the log); report an error only if everything failed
    if stats.total() == 0 {
        if let Some(err) = last_err {
            anyhow::bail!("{err}");
        }
    }
    // The configured list is the whole set: documents not on the list are marked "not in source" (a fetch failure does not count, since it is still configured)
    utopia_store::documents::reconcile_missing(&state.pool, source.id, &urls)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(stats)
}

/// Fetch one page. The address was entered by this library's operator, so the intranet is allowed (`Reach::Operator`),
/// but redirects may not carry the request into the intranet, and the response body is capped (#330).
async fn fetch_page(url: &str) -> anyhow::Result<(String, String, Vec<u8>)> {
    let page = crate::http_fetch::get(
        url,
        crate::http_fetch::Reach::Operator,
        crate::http_fetch::Limits::default(),
    )
    .await?;
    let mime = if page.mime == "application/octet-stream" {
        "text/html".to_string()
    } else {
        page.mime
    };
    // The identity is still **the URL in the config** (the same page under a new address should not become a new document),
    // but landing somewhere else is worth a line
    if page.final_url.as_str() != url {
        tracing::debug!(from = url, to = %page.final_url, "page redirected to somewhere else");
    }
    let filename = filename_from_url(url, &mime);
    Ok((filename, mime, page.bytes))
}

fn filename_from_url(url: &str, mime: &str) -> String {
    let stripped = url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let slug: String = stripped
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = truncate_utf8(&slug, 120);
    let has_ext = slug
        .rsplit('.')
        .next()
        .map(|e| e.len() <= 5)
        .unwrap_or(false)
        && slug.contains('.')
        && !slug.ends_with('.');
    if !has_ext || mime.contains("html") {
        format!("{slug}.html")
    } else {
        slug
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn rss_article_link(entry: &feed_rs::model::Entry) -> Option<String> {
    entry.links.iter().find_map(|link| {
        let rel_is_alternate = link
            .rel
            .as_deref()
            .is_none_or(|rel| rel.eq_ignore_ascii_case("alternate"));
        if !rel_is_alternate {
            return None;
        }
        if link.media_type.as_deref().is_some_and(|media| {
            !media.eq_ignore_ascii_case("text/html")
                && !media.eq_ignore_ascii_case("application/xhtml+xml")
        }) {
            return None;
        }
        let parsed = reqwest::Url::parse(link.href.trim()).ok()?;
        match parsed.scheme() {
            "http" | "https" if parsed.host().is_some() => Some(parsed.to_string()),
            _ => None,
        }
    })
}

const RSS_EXTERNAL_KEY_MAX_BYTES: usize = 4_096;

fn rss_entry_key(entry: &feed_rs::model::Entry, article_url: Option<&str>) -> Option<String> {
    let identity = if !entry.id.trim().is_empty() {
        entry.id.trim()
    } else {
        article_url?.trim()
    };
    if identity.is_empty() {
        return None;
    }
    Some(bound_rss_identity(identity))
}

fn bound_rss_identity(identity: &str) -> String {
    if identity.len() <= RSS_EXTERNAL_KEY_MAX_BYTES {
        return identity.to_owned();
    }
    // Never truncate a publisher's GUID or URL: doing so silently merges
    // distinct feed items sharing the same prefix. A bounded digest retains
    // the complete identity material without violating the database bound.
    format!("rss:v1:sha256:{}", sha_hex(identity.as_bytes()))
}

fn content_is_substantive(markdown: &str, summary: &str, linked_page: bool) -> bool {
    crate::rss_full_content::quality_check(markdown, summary, linked_page).is_ok()
}

fn has_usable_rss_content(feed_usable: bool, article_url: Option<&str>) -> bool {
    feed_usable || article_url.is_some_and(|url| !url.trim().is_empty())
}

#[cfg(test)]
#[path = "rss_sync_contract_tests.rs"]
mod rss_sync_contract_tests;

async fn sync_rss(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    let feed_url = source.config["feed_url"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("rss source is missing config.feed_url"))?;
    let content_mode = utopia_store::sources::rss_content_mode(&source.config)?;
    let observation = if content_mode == utopia_store::sources::RSS_FULL_CONTENT_MODE {
        Some(
            utopia_store::rss_full_content::begin_feed_observation(
                &state.pool,
                source.id,
                &source.config,
            )
            .await?,
        )
    } else {
        None
    };

    let feed_body = crate::http_fetch::get(
        feed_url,
        crate::http_fetch::Reach::Operator,
        crate::http_fetch::Limits {
            max_bytes: MAX_FEED_BYTES,
            ..Default::default()
        },
    )
    .await?;
    let bytes = feed_body.bytes;
    // feed-rs normally synthesizes missing entry IDs from link + title (or a
    // random UUID). Neither is an application-level stable identity. Leave
    // missing IDs empty so rss_entry_key can use only the stable article URL.
    let feed = feed_rs::parser::Builder::new()
        .id_generator(|_, _, _| String::new())
        .build()
        .parse(&bytes[..])
        .map_err(|e| anyhow::anyhow!("Failed to parse feed: {e}"))?;

    let observations: Vec<RssObservation> = feed
        .entries
        .iter()
        .filter_map(|entry| {
            let title = entry
                .title
                .as_ref()
                .map(|t| t.content.trim().to_string())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| "untitled".into());
            let article_url = rss_article_link(entry);
            let key = rss_entry_key(entry, article_url.as_deref())?;
            let embedded_html = entry
                .content
                .as_ref()
                .and_then(|content| content.body.as_deref())
                .map(str::trim)
                .filter(|body| !body.is_empty())
                .map(|body| truncate_utf8(body, 2 * 1024 * 1024));
            let summary = entry
                .summary
                .as_ref()
                .map(|summary| summary.content.trim().to_string())
                .unwrap_or_default();
            let embedded_markdown = embedded_html
                .as_deref()
                .and_then(|html| crate::rss_full_content::normalize_feed_html(html).ok());
            let feed_usable = embedded_markdown
                .as_deref()
                .is_some_and(|markdown| content_is_substantive(markdown, &summary, false));
            let usable_source = has_usable_rss_content(feed_usable, article_url.as_deref());
            Some(RssObservation {
                key,
                title,
                article_url,
                summary,
                embedded_html,
                doc_time: entry.published.or(entry.updated),
                has_usable_source: usable_source,
            })
        })
        .collect();

    if let Some((activation, observed_at)) = observation {
        let entries: Vec<utopia_store::rss_full_content::NewEntry> = observations
            .iter()
            .map(|observation| utopia_store::rss_full_content::NewEntry {
                external_key: observation.key.clone(),
                title: truncate_utf8(&observation.title, 2_048),
                article_url: observation
                    .article_url
                    .as_deref()
                    .map(|url| truncate_utf8(url, 8_192)),
                summary: truncate_utf8(&observation.summary, 16_384),
                embedded_html: observation.embedded_html.clone(),
                doc_time: observation.doc_time,
                has_usable_source: observation.has_usable_source,
            })
            .collect();

        if activation.activation_state == "pending" {
            let discovered = utopia_store::rss_full_content::record_baseline(
                &state.pool,
                source.id,
                activation.activation_generation,
                &entries,
            )
            .await?;
            return Ok(SyncStats {
                discovered,
                ..SyncStats::default()
            });
        }
        if activation.activation_state != "active" {
            anyhow::bail!("RSS full-content activation is disabled");
        }

        let discovered = utopia_store::rss_full_content::discover_observed(
            &state.pool,
            source.id,
            activation.activation_generation,
            &entries,
            observed_at,
        )
        .await?;
        let queued = utopia_store::rss_full_content::claim_pending_and_enqueue(
            &state.pool,
            source.id,
            activation.activation_generation,
            RSS_MAX_INFLIGHT,
            RSS_HYDRATION_ATTEMPTS,
        )
        .await?;
        return Ok(SyncStats {
            discovered: discovered.discovered,
            queued_for_content: queued,
            content_terminal: discovered.terminal,
            ..SyncStats::default()
        });
    }

    let mut stats = SyncStats::default();
    for observation in observations.into_iter().take(MAX_NEW_PER_SYNC) {
        let RssObservation {
            key,
            title,
            article_url,
            summary,
            embedded_html,
            doc_time,
            ..
        } = observation;
        let body = embedded_html.unwrap_or(summary);
        let safe_title = escape_html(&title);
        let safe_link = article_url.as_deref().map(escape_html).unwrap_or_default();
        let html = format!(
            "<html><head><title>{safe_title}</title></head><body><h1>{safe_title}</h1>\n<p><a href=\"{safe_link}\">{safe_link}</a></p>\n{body}</body></html>",
        );
        let slug = {
            let slug = slugify(&title);
            if slug.is_empty() {
                "untitled".to_string()
            } else {
                slug
            }
        };
        let filename = format!("{slug}.html");
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &key,
            &filename,
            "text/html",
            html.as_bytes(),
            doc_time,
        )
        .await?;
        stats.absorb(action);
    }
    Ok(stats)
}

/// GitHub ticket: one ticket is one document, and its body carries its status-change history.
///
/// The ticket list is filtered incrementally by `since`; comments and events are read per ticket, without an incremental window.
/// Each document is replaced in full every time: filtering comments by `since` too would delete old discussion that had not changed.
/// See [`crate::github_issues`] for details.
///
/// `doc_time` takes `updated_at` rather than `created_at`: each sync captures "what this ticket looks like at this moment",
/// so the cognitive time should say when this state held. Adding a comment changes
/// `updated_at`, so the content changes, a new version is recorded, and `doc_time` follows it.
async fn sync_github_issues(
    state: &AppState,
    source: &Source,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<SyncStats> {
    // api.github.com is a fixed public host, so it is held to content-level strictness (`Reach::Content`)
    let http = crate::http_fetch::client_for(
        &reqwest::Url::parse("https://api.github.com/")?,
        crate::http_fetch::Reach::Content,
        crate::http_fetch::Limits::default(),
    )
    .await?;
    sync_github_issues_with_client(state, source, since, &http, "https://api.github.com").await
}

// The HTTP seam lets regression tests run the same sync and persist path; production still always hits the public GitHub API.
async fn sync_github_issues_with_client(
    state: &AppState,
    source: &Source,
    since: Option<DateTime<Utc>>,
    http: &reqwest::Client,
    api_base: &str,
) -> anyhow::Result<SyncStats> {
    let repo = source.config["repo"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("github_issues source is missing config.repo (owner/name)")
        })?;
    if repo.split('/').count() != 2 {
        anyhow::bail!("config.repo should look like owner/name, got {repo:?}");
    }
    let auth = source.config["auth_header"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // PRs are issues in GitHub's model too. Excluded by default: a ticket-system query wants tickets; but some repositories
    // actually record decisions in PR descriptions, so a switch is provided
    let include_prs = source.config["include_pull_requests"]
        .as_bool()
        .unwrap_or(false);

    let base = format!("{api_base}/repos/{repo}");

    let window = crate::github_issues::fetch_window(
        http,
        &format!("{base}/issues"),
        since,
        MAX_NEW_PER_SYNC,
        include_prs,
        auth,
    )
    .await?;
    let mut stats = SyncStats {
        checkpoint: window.checkpoint,
        ..SyncStats::default()
    };
    for issue in &window.issues {
        // Only the tickets selected this round are rebuilt, but each one needs its full comment history read. Even if repository-level comments
        // drop `since`, unrelated tickets' and PRs' comments can push them out of the page cap.
        let comments = crate::github_issues::fetch_all::<crate::github_issues::Comment>(
            http,
            &format!("{base}/issues/{}/comments", issue.number),
            &[],
            auth,
        )
        .await?;
        let cs = crate::github_issues::comments_for_issue(issue, &comments);
        // Events are fetched per ticket. N is only the number of tickets to write this round: on a first sync it equals the total,
        // afterwards, with `since` as the backstop, it is usually a single digit
        let events = crate::github_issues::sort_events(
            crate::github_issues::fetch_all(
                http,
                &format!("{base}/issues/{}/events", issue.number),
                &[],
                auth,
            )
            .await?,
        );
        let es: Vec<&crate::github_issues::Event> = events.iter().collect();
        let body = crate::github_issues::render(issue, &cs, &es);
        // The logical identity includes the repository: when two repos are attached to one knowledge base, #18 does not overwrite the other
        let key = format!("github:{repo}#{}", issue.number);
        let filename = format!("{}-{}.md", issue.number, slugify(&issue.title));
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &key,
            &filename,
            "text/markdown",
            body.as_bytes(),
            Some(issue.updated_at),
        )
        .await?;
        stats.absorb(action);
    }
    Ok(stats)
}

/// GitHub repository files: one recursive fetch of the whole tree of the default branch, one document per path.
/// The blob SHA in the tree alone tells "unchanged"; only changed ones have their commit date and content fetched one by one.
async fn sync_github_repo(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    // Same convention as github_issues: the fixed public host is held to content-level strictness
    let url = reqwest::Url::parse("https://api.github.com/")?;
    let http = crate::http_fetch::client_for(
        &url,
        crate::http_fetch::Reach::Content,
        crate::http_fetch::Limits::default(),
    )
    .await?;
    sync_github_repo_with_client(state, source, &http, "https://api.github.com").await
}

// The HTTP seam lets regression tests run the same sync and persist path; production still always hits the public GitHub API.
async fn sync_github_repo_with_client(
    state: &AppState,
    source: &Source,
    http: &reqwest::Client,
    api_base: &str,
) -> anyhow::Result<SyncStats> {
    use crate::{github_issues::get_checked, github_repo as gr};
    let repo = source.config["repo"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("github_repo source is missing config.repo (owner/name)"))?;
    if repo.split('/').count() != 2 {
        anyhow::bail!("config.repo should look like owner/name, got {repo:?}");
    }
    let auth = source.config["auth_header"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let filter = gr::FileFilter::from_config(&source.config)?;
    let base = format!("{api_base}/repos/{repo}");
    let tree_url = reqwest::Url::parse(&format!("{base}/git/trees/HEAD?recursive=1"))?;
    let tree: gr::TreeResponse = get_checked(http, tree_url, auth, None)
        .await?
        .json()
        .await?;
    let (truncated, chosen) = (tree.truncated, gr::choose(tree.tree, &filter));
    let keys: Vec<String> = chosen
        .iter()
        .map(|e| format!("github:{repo}:{}", e.path))
        .collect();
    let mut stats = SyncStats::default();
    for (entry, key) in chosen.iter().zip(&keys) {
        // Unchanged entries are skipped entirely: if the stored bytes give the same git SHA, there is no need to spend two requests
        // A document in the trash must go through the ingestion flow, so it is restored even if the upstream content has not changed.
        let old =
            utopia_store::documents::find_by_external_key(&state.pool, source.id, key).await?;
        let unchanged = match old {
            Some(d) if d.deleted_at.is_none() && Some(d.size_bytes) == entry.size => state
                .blob
                .get(&d.sha256)
                .await
                .is_ok_and(|b| gr::git_blob_sha(&b) == entry.sha),
            _ => false,
        };
        if unchanged {
            continue;
        }
        // The document date is taken from the last commit that touched this path; it is asked only when the file is new or has changed
        let mut cu = reqwest::Url::parse(&format!("{base}/commits"))?;
        cu.query_pairs_mut()
            .append_pair("path", &entry.path)
            .append_pair("per_page", "1");
        let commits: serde_json::Value = get_checked(http, cu, auth, None).await?.json().await?;
        let doc_time = commits[0]["commit"]["committer"]["date"]
            .as_str()
            .and_then(|d| d.parse().ok());
        let bu = reqwest::Url::parse(&format!("{base}/git/blobs/{}", entry.sha))?;
        let bytes = get_checked(http, bu, auth, Some("application/vnd.github.raw+json"))
            .await?
            .bytes()
            .await?;
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            key,
            &entry.path,
            "text/markdown",
            &bytes,
            doc_time,
        )
        .await?;
        stats.absorb(action);
    }
    if truncated {
        // Do not decide "who disappeared" when the tree is incomplete: absence may just mean it has not been listed yet
        tracing::warn!(%repo, "GitHub file tree was truncated, so this round ingests only the listed files and does not reconcile files absent from the source");
    } else {
        utopia_store::documents::reconcile_missing(&state.pool, source.id, &keys)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    Ok(stats)
}

/// GitLab project files (gitlab.com, #1327): same shape as [`sync_github_repo`], one document per path,
/// "unchanged" is judged by blob SHA. The tree is paged; a new or changed file costs one extra `HEAD` to get its size before deciding whether to fetch it.
async fn sync_gitlab_repo(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    let url = reqwest::Url::parse("https://gitlab.com/")?;
    let http = crate::http_fetch::client_for(
        &url,
        crate::http_fetch::Reach::Content,
        crate::http_fetch::Limits::default(),
    )
    .await?;
    sync_gitlab_repo_with_client(state, source, &http, "https://gitlab.com/api/v4").await
}

/// Send one request with the token and judge the status. GitLab rate limits return 429; say clearly that it is rate limiting, where the right action is to wait or configure a token
pub(crate) async fn gitlab_checked(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: reqwest::Url,
    auth: Option<&str>,
) -> anyhow::Result<reqwest::Response> {
    let mut req = http.request(method, url);
    if let Some(a) = auth {
        req = req.header(reqwest::header::AUTHORIZATION, a);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        anyhow::bail!("GitLab rate limit: too many requests, sync later; unauthenticated quota is lower, configure a read_api token");
    }
    if !status.is_success() {
        anyhow::bail!("HTTP {status} from GitLab");
    }
    Ok(resp)
}

/// `config.project`: gitlab_repo and gitlab_issues accept the same kind of project path
fn gitlab_project(source: &Source) -> anyhow::Result<&str> {
    let project = source.config["project"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} source is missing config.project (group/name)",
                source.kind
            )
        })?;
    // Subgroups can be several levels deep, but every segment must have text
    if project.split('/').count() < 2 || project.split('/').any(str::is_empty) {
        anyhow::bail!(
            "config.project should look like group/name or group/subgroup/name, got {project:?}"
        );
    }
    Ok(project)
}

/// GitLab tickets and merge requests (gitlab.com, #1327): one ticket is one document, and its body carries the history of
/// changes to status, labels and milestones. The list is filtered incrementally by update instant; notes and events are read per ticket to the last page, in the same
/// shape as [`sync_github_issues`]. See [`crate::gitlab_issues`] for details.
async fn sync_gitlab_issues(
    state: &AppState,
    source: &Source,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<SyncStats> {
    let http = crate::http_fetch::client_for(
        &reqwest::Url::parse("https://gitlab.com/")?,
        crate::http_fetch::Reach::Content,
        crate::http_fetch::Limits::default(),
    )
    .await?;
    sync_gitlab_issues_with_client(state, source, since, &http, "https://gitlab.com/api/v4").await
}

// The HTTP seam lets regression tests run the same sync and persist path; production always hits the gitlab.com API.
async fn sync_gitlab_issues_with_client(
    state: &AppState,
    source: &Source,
    since: Option<DateTime<Utc>>,
    http: &reqwest::Client,
    api_base: &str,
) -> anyhow::Result<SyncStats> {
    use crate::gitlab_issues as gl;
    use chrono::SubsecRound as _;
    let project = gitlab_project(source)?;
    // Notes and events without a token return 401, public projects too. Say this plainly rather than sending a request that is bound to fail,
    // nor fall back to ingesting only the body with no history: the history is the reason this source exists
    let auth = source.config["auth_header"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "gitlab_issues source needs config.auth_header: GitLab answers 401 for a \
                 ticket's notes and events without a token, even on a public project"
            )
        })?;
    // Merge requests are not collected by default, for the same reason as GitHub's include_pull_requests
    let kinds: &[gl::Kind] = if source.config["include_merge_requests"]
        .as_bool()
        .unwrap_or(false)
    {
        &[gl::Kind::Issue, gl::Kind::MergeRequest]
    } else {
        &[gl::Kind::Issue]
    };
    let mut base = reqwest::Url::parse(api_base)?;
    // The project path is one segment: its `/` must become `%2F`
    base.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("API base cannot take a path"))?
        .pop_if_empty()
        .extend(["projects", project]);

    let window = gl::fetch_window(http, &base, kinds, since, MAX_NEW_PER_SYNC, Some(auth)).await?;
    let mut stats = SyncStats {
        checkpoint: window.checkpoint,
        ..SyncStats::default()
    };
    for (kind, item) in &window.tickets {
        let details = gl::fetch_details(http, &base, *kind, item.iid, Some(auth)).await?;
        let body = gl::render(*kind, item, &details);
        // The iids of tickets and of merge requests each count from 1, so identity and file names must be kept apart
        let key = format!("gitlab:{project}{}{}", kind.sigil(), item.iid);
        let prefix = match kind {
            gl::Kind::Issue => "",
            gl::Kind::MergeRequest => "mr-",
        };
        let filename = format!("{prefix}{}-{}.md", item.iid, slugify(&item.title));
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &key,
            &filename,
            "text/markdown",
            body.as_bytes(),
            // GitLab's instants carry milliseconds; truncate to seconds before storing (0013 decisions 6 and 7)
            Some(item.updated_at.trunc_subsecs(0)),
        )
        .await?;
        stats.absorb(action);
    }
    Ok(stats)
}

// The HTTP seam lets regression tests run the same sync and persist path; production always hits the gitlab.com API.
async fn sync_gitlab_repo_with_client(
    state: &AppState,
    source: &Source,
    http: &reqwest::Client,
    api_base: &str,
) -> anyhow::Result<SyncStats> {
    use crate::{github_repo as gr, gitlab_repo as gl};
    let project = gitlab_project(source)?;
    let auth = source.config["auth_header"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let filter = gr::FileFilter::from_config(&source.config)?;
    let base = reqwest::Url::parse(api_base)?;
    // Each segment is encoded separately: the `/` in the project path and the `/` in the file path must both become `%2F`
    let at = |segments: &[&str]| -> anyhow::Result<reqwest::Url> {
        let mut u = base.clone();
        u.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("API base cannot take a path"))?
            .pop_if_empty()
            .extend(["projects", project, "repository"])
            .extend(segments);
        Ok(u)
    };

    let mut tree_url = at(&["tree"])?;
    tree_url
        .query_pairs_mut()
        .append_pair("recursive", "true")
        .append_pair("per_page", "100")
        .append_pair("pagination", "keyset");
    let (mut entries, mut next, mut pages) = (Vec::<gl::TreeEntry>::new(), Some(tree_url), 0);
    while let Some(url) = next.take() {
        if pages == gl::MAX_TREE_PAGES {
            next = Some(url);
            break;
        }
        let resp = gitlab_checked(http, reqwest::Method::GET, url, auth).await?;
        let link = resp
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        entries.extend(resp.json::<Vec<gl::TreeEntry>>().await?);
        next = gl::next_page(link.as_deref(), &base);
        pages += 1;
    }
    let (truncated, chosen) = (next.is_some(), gl::choose(entries, &filter));
    let keys: Vec<String> = chosen
        .iter()
        .map(|e| format!("gitlab:{project}:{}", e.path))
        .collect();
    let mut stats = SyncStats::default();
    for (entry, key) in chosen.iter().zip(&keys) {
        // Unchanged entries are skipped entirely: the tree has no sizes, but the stored bytes giving the same git SHA is enough
        // A document in the trash must go through the ingestion flow, so it is restored even if the upstream content has not changed.
        let old =
            utopia_store::documents::find_by_external_key(&state.pool, source.id, key).await?;
        let unchanged = match old {
            Some(d) if d.deleted_at.is_none() => state
                .blob
                .get(&d.sha256)
                .await
                .is_ok_and(|b| gr::git_blob_sha(&b) == entry.id),
            _ => false,
        };
        if unchanged {
            continue;
        }
        // Ask the size first; the size limit must be checked before downloading. A file over the limit is not stored, so every round asks about it again:
        // one HEAD, without downloading the body. When rate limiting is actually hit, record the blocked blob SHAs too
        let mut fu = at(&["files", &entry.path])?;
        fu.query_pairs_mut().append_pair("ref", "HEAD");
        let head = gitlab_checked(http, reqwest::Method::HEAD, fu, auth).await?;
        let size = head
            .headers()
            .get("x-gitlab-size")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok());
        let Some(size) = size else {
            tracing::warn!(%project, path = %entry.path, "GitLab gave no file size, skipping this file");
            continue;
        };
        if !filter.size_ok(size) {
            continue;
        }
        // The document date is taken from the last commit that touched this path; it is asked only when the file is new or has changed
        let mut cu = at(&["commits"])?;
        cu.query_pairs_mut()
            .append_pair("path", &entry.path)
            .append_pair("per_page", "1");
        let commits: serde_json::Value = gitlab_checked(http, reqwest::Method::GET, cu, auth)
            .await?
            .json()
            .await?;
        let doc_time = commits[0]["committed_date"]
            .as_str()
            .and_then(gl::commit_time);
        let bytes = gitlab_checked(
            http,
            reqwest::Method::GET,
            at(&["blobs", &entry.id, "raw"])?,
            auth,
        )
        .await?
        .bytes()
        .await?;
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            key,
            &entry.path,
            "text/markdown",
            &bytes,
            doc_time,
        )
        .await?;
        stats.absorb(action);
    }
    if truncated {
        tracing::warn!(%project, pages = gl::MAX_TREE_PAGES, "GitLab file tree not fully paged, so this round ingests only the listed files and does not reconcile files absent from the source");
    } else {
        utopia_store::documents::reconcile_missing(&state.pool, source.id, &keys)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    Ok(stats)
}

/// Jira tickets: one ticket is one document, and its body carries the **field-level** change history.
///
/// Cheaper than the GitHub path: one `search?expand=changelog` call returns the ticket itself,
/// the full change history and the comments, **with no N+1**. Incrementality is expressed through JQL's `updated >= …`,
/// because Jira has no `since` parameter. See [`crate::jira_issues`] for details.
///
/// `doc_time` takes `updated`, the same convention as github_issues: each sync captures
/// "what this ticket looks like at this moment", and the cognitive time should say when this state held.
async fn sync_jira_issues(
    state: &AppState,
    source: &Source,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<SyncStats> {
    let base_url = source.config["base_url"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("jira_issues source is missing config.base_url"))?;
    let project = source.config["project"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("jira_issues source is missing config.project"))?;
    // The project key is concatenated straight into the JQL, so it cannot be an arbitrary string. Jira keys are already restricted
    // to alphanumerics plus underscores: blocking that also blocks JQL injection
    if !project.chars().all(|c| c.is_alphanumeric() || c == '_') {
        anyhow::bail!("config.project should be a Jira project key, got {project:?}");
    }
    let auth = source.config["auth_header"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let http = crate::http_fetch::client_for(
        &reqwest::Url::parse(base_url).map_err(|e| anyhow::anyhow!("Invalid base_url: {e}"))?,
        crate::http_fetch::Reach::Operator,
        crate::http_fetch::Limits::default().with_overall(std::time::Duration::from_secs(45)),
    )
    .await?;

    let cloud = crate::jira_issues::is_jira_cloud(base_url);
    let window = crate::jira_issues::fetch_window(
        &http,
        base_url,
        cloud,
        project,
        since,
        MAX_NEW_PER_SYNC,
        auth,
    )
    .await?;
    let mut stats = SyncStats {
        checkpoint: window.checkpoint,
        ..SyncStats::default()
    };
    for issue in &window.issues {
        let body = crate::jira_issues::render(issue);
        // The logical identity includes the site: when one knowledge base has two Jiras attached, PROJ-1 does not overwrite the other
        let host = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "jira".into());
        let key = format!("jira:{host}/{}", issue.key);
        let filename = format!(
            "{}-{}.md",
            issue.key,
            slugify(issue.fields.summary.as_deref().unwrap_or(""))
        );
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &key,
            &filename,
            "text/markdown",
            body.as_bytes(),
            issue.fields.updated.map(|t| t.0),
        )
        .await?;
        stats.absorb(action);
    }
    Ok(stats)
}

/// Title → filename-safe fragment. Same convention as the RSS path (non-alphanumerics become -, then truncated).
fn slugify(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = truncate_utf8(&s, 60);
    s.trim_matches('-').to_string()
}

/// Custom fetcher: Utopia Ingest Interface:
/// `GET {endpoint}?since=<start time of the last successful sync, RFC3339>` (the first sync has no since; an Authorization header may be configured),
/// response `{"items":[{"id":"stable unique ID","title":"document name","content":"body (plain text/Markdown/HTML)",
///                  "doc_time":"RFC3339, optional","mime":"text/markdown, optional"}]}`.
/// id → external_key (custom:{id}); the three-way decision applies: same id with same content is skipped, new content is updated in place.
async fn sync_custom(
    state: &AppState,
    source: &Source,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<SyncStats> {
    let endpoint = source.config["endpoint"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("custom source is missing config.endpoint"))?;
    let mut url =
        reqwest::Url::parse(endpoint).map_err(|e| anyhow::anyhow!("Invalid endpoint URL: {e}"))?;
    if let Some(t) = since {
        url.query_pairs_mut().append_pair("since", &t.to_rfc3339());
    }

    // The endpoint was entered by the operator, so the intranet and loopback are both legitimate targets; the proxy trade-off (loopback through a proxy
    // only yields 502) is folded together with address validation into `client_for` (#330)
    let http = crate::http_fetch::client_for(
        &url,
        crate::http_fetch::Reach::Operator,
        crate::http_fetch::Limits::default(),
    )
    .await?;
    let mut req = http.get(url);
    if let Some(auth) = source.config["auth_header"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    {
        req = req.header(reqwest::header::AUTHORIZATION, auth.trim());
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} from endpoint", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    let items = body["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Response is missing the items[] array"))?;

    // `discovered` records the count in the response, not the count run in: otherwise the log and warnings would read as `(0 > 200)`
    let mut stats = SyncStats {
        discovered: items.len(),
        ..Default::default()
    };
    let mut seen_keys: Vec<String> = Vec::new();
    // **Walk the whole response, and charge only the changed entries to the budget** (ADR 0072's Custom section).
    // `take(MAX_NEW_PER_SYNC)` is what the ADR explicitly lists under Rejected: it always takes the same
    // first 200 entries, and a retry ten thousand times would never reach the 201st. Here every entry is first checked with `ingest_item`, and only
    // `Created` / `Updated` count towards `changed`; `Unchanged` / `Moved` / `Duplicate`
    // do not spend the budget, so a retry can get past the unchanged prefix and reach the rest.
    let mut changed = 0usize;
    for item in items.iter() {
        // Once the budget is full, do not walk further: the rest stays for the next round, and this round's `incomplete` leaves the cursor where it is
        if changed >= MAX_NEW_PER_SYNC {
            stats.incomplete = true;
            tracing::warn!(
                source_id = %source.id,
                imported = changed,
                remaining = items.len().saturating_sub(seen_keys.len()),
                "custom 本轮用满 {MAX_NEW_PER_SYNC} 条预算，还有条目没读到；游标不前进，下一轮接着走"
            );
            break;
        }
        let Some(id) = item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            tracing::warn!(source_id = %source.id, "custom item missing id, skipping");
            continue;
        };
        let Some(content) = item["content"].as_str().filter(|s| !s.trim().is_empty()) else {
            tracing::warn!(source_id = %source.id, %id, "custom item missing content, skipping");
            continue;
        };
        let title = item["title"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        let mime = item["mime"].as_str().unwrap_or("text/markdown");
        let doc_time = item["doc_time"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let filename = ensure_extension(title, mime);
        let key = format!("custom:{id}");
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &key,
            &filename,
            mime,
            content.as_bytes(),
            doc_time,
        )
        .await?;
        match action {
            IngestAction::Created | IngestAction::Updated => changed += 1,
            _ => {}
        }
        stats.absorb(action);
        seen_keys.push(key);
    }
    // In an incremental response (?since=), absence is not deletion, so no full-set reconciliation is done; but:
    // 1) an entry that reappears has its missing marker removed (lost and found again)
    // 2) only an explicit tombstone in deleted[] marks "not in source"; whether to delete the document is the user's decision in the UI
    if !seen_keys.is_empty() {
        utopia_store::documents::clear_missing_keys(&state.pool, source.id, &seen_keys)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    let tombstones: Vec<String> = body["deleted"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|id| format!("custom:{}", id.trim()))
                .collect()
        })
        .unwrap_or_default();
    if !tombstones.is_empty() {
        let n = utopia_store::documents::mark_missing_keys(&state.pool, source.id, &tombstones)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        tracing::info!(source_id = %source.id, count = n, "custom tombstone: marking as absent from source");
    }
    Ok(stats)
}

/// Object storage sync: list the objects under the prefix and ingest them one by one.
///
/// `external_key` uses `s3://bucket/key`, the same convention as `file:///` and page URLs:
/// the provenance describes itself. If the prefix changes but the content does not, `ingest_item` recognises it as a "move" rather than a new addition,
/// and the extraction is not run again.
///
/// **`doc_time` takes `LastModified`, and that is the write moment, not the document's own time.**
/// A 2019 contract uploaded today would land on today's date in the timeline. Object storage has no better
/// source, unless the file name or body carries a date, and that is the extractor's job. This one has the same problem as the `url` source:
/// the first criterion of `0013` is only half met here.
async fn sync_object_storage(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    let bucket = source.config["bucket"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{} source is missing config.bucket", source.kind))?;
    let prefix = source.config["prefix"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let store = crate::object_storage::client(&source.kind, &source.config)?;
    let (objects, truncated) =
        crate::object_storage::fetch(&source.kind, store.as_ref(), bucket, prefix).await?;

    // Hitting the cap is not an error, but it must be said: otherwise "sync succeeded" would hide things that did not come in,
    // which is exactly the silent failure 0005 describes
    if truncated {
        tracing::warn!(
            %bucket, prefix = prefix.unwrap_or(""),
            "对象数到达单次上限，其余留给下一次同步"
        );
    }

    let mut stats = SyncStats::default();
    for obj in objects {
        // **Do not guess the mime.** `utopia_ingest::parse` checks the magic number first, then the extension;
        // its comment says "the extension may lie". Guessing from the file name here would only add one more
        // source that can lie, and would need an extra dependency for it. `octet-stream` is honest:
        // we just have a run of bytes and have not looked at what is inside.
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &obj.external_key,
            &obj.filename,
            "application/octet-stream",
            &obj.bytes,
            obj.last_modified,
        )
        .await?;
        stats.absorb(action);
    }
    Ok(stats)
}

/// WebDAV sync: walk directories level by level and ingest the files.
///
/// `external_key` uses `webdav://host/path`: the same file under a new mount point is still the same file,
/// while same-named paths on different drives are two different files.
async fn sync_webdav(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    let base = source.config["base_url"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("webdav source is missing config.base_url"))?;
    let root = source.config["path"].as_str().map(str::trim).unwrap_or("/");
    let user = source.config["username"].as_str().map(str::trim);
    let pass = source.config["password"].as_str().map(str::trim);
    let auth = match (user, pass) {
        (Some(u), Some(p)) if !u.is_empty() => Some((u, p)),
        _ => None,
    };

    let http = crate::http_fetch::client_for(
        &reqwest::Url::parse(base).map_err(|e| anyhow::anyhow!("Invalid base_url: {e}"))?,
        crate::http_fetch::Reach::Operator,
        crate::http_fetch::Limits::default().with_overall(std::time::Duration::from_secs(60)),
    )
    .await?;
    let (files, truncated) = crate::webdav::fetch(&http, base, root, auth).await?;
    if truncated {
        tracing::warn!(
            base,
            root,
            "file count reached the per-run limit, the rest is left for the next sync"
        );
    }

    let mut stats = SyncStats::default();
    for f in files {
        // Do not guess the mime, for the same reason as object storage: parsing checks the magic number first
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &f.external_key,
            &f.filename,
            "application/octet-stream",
            &f.bytes,
            f.last_modified,
        )
        .await?;
        stats.absorb(action);
    }
    Ok(stats)
}

/// Notion sync: ingest the pages the integration can see.
///
/// `doc_time` takes `last_edited_time`: **this is the page's own edit moment**, more honest than the object storage
/// write moment: a 2019 contract uploaded to S3 today lands on today's date, whereas a Notion
/// page's edit moment is exactly when its content changed.
async fn sync_notion(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    let token = source.config["token"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("notion source is missing config.token"))?;
    let query = source.config["query"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let (pages, truncated, unread) = crate::notion::fetch(token, query).await?;
    if truncated {
        tracing::warn!("page count reached the per-run limit, the rest is left for the next sync");
    }

    let mut stats = SyncStats::default();
    for p in pages {
        let action = ingest_item(
            state,
            source.kb_id,
            source.id,
            &p.external_key,
            &p.filename,
            "text/markdown",
            p.text.as_bytes(),
            p.last_edited,
        )
        .await?;
        stats.absorb(action);
    }
    // Pages that could be read are already in the library; the pages that could not be read make this round record failure and name them,
    // or else they would sit on an old version with nobody knowing
    if !unread.is_empty() {
        anyhow::bail!(
            "{} Notion page(s) synced; {} could not be read and keep their stored content: {}",
            stats.total(),
            unread.len(),
            unread
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(stats)
}

/// Google Drive sync: walk the selected folders and ingest the files that can be read (0074).
///
/// `doc_time` takes `modifiedTime`, same reasoning as Notion: it is the moment the content was last changed.
/// Files already marked unchanged with the document still present are not even downloaded; see the module comment in `google_drive`.
///
/// **Reconciliation is done only when the traversal is complete**: when the listing hits the cap, or a selected folder cannot be opened, what was not seen
/// is not the same as gone; only the missing marker is removed from what this round saw. Hitting the cap on downloads does not affect reconciliation: the listing is complete.
///
/// A round that does not finish (listing hit the cap, downloads hit the cap, or something could not be opened or read) reports failure and gives the reason,
/// rather than success: nothing may be hidden under "sync succeeded"
async fn sync_google_drive(state: &AppState, source: &Source) -> anyhow::Result<SyncStats> {
    use crate::google_drive;

    let refresh = source.config["refresh_token"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "google_drive source is not connected: connect a Google account in its settings"
            )
        })?;
    // It reads with the Drive of the person who connected it: if they left the library, were demoted to read-only, or their account was deactivated,
    // this authorisation no longer reads on the library's behalf, so the people in the library must reconnect
    let connector = match google_drive::connected_by(&source.config) {
        Some(id) => utopia_store::accounts::find_user_by_id(&state.pool, id).await?,
        None => None,
    };
    let access = match connector {
        Some(user) => {
            utopia_store::access::require_kb(&state.pool, &user, source.kb_id, Role::Editor).await
        }
        None => Err(AppError::NotFound),
    };
    let still_editor = match access {
        Ok(_) => true,
        Err(AppError::NotFound | AppError::Forbidden) => false,
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(
        still_editor,
        "the person who connected this Google Drive is no longer an editor of this knowledge base: reconnect it in the source settings"
    );
    let cfg = google_drive::oauth_config().ok_or_else(|| {
        anyhow::anyhow!(
            "Google Drive is not configured on this server (UTOPIA_GOOGLE_DRIVE_CLIENT_ID)"
        )
    })?;
    let folders = google_drive::selected_folders(&source.config);
    if folders.is_empty() {
        anyhow::bail!("google_drive source has no folders selected");
    }

    let drive = google_drive::Drive::connect(&cfg, refresh).await?;
    sync_google_drive_with_client(state, source, &drive, &folders).await
}

pub(crate) async fn sync_google_drive_with_client(
    state: &AppState,
    source: &Source,
    drive: &crate::google_drive::Drive,
    folders: &[String],
) -> anyhow::Result<SyncStats> {
    use crate::google_drive;

    let listing = drive.walk(folders).await?;
    let known = google_drive::markers(&state.pool, source.id).await?;
    let mut stats = SyncStats::default();
    let mut seen_keys = Vec::with_capacity(listing.files.len());
    let mut seen_ids = Vec::with_capacity(listing.files.len());
    let mut unread: Vec<String> = Vec::new();
    let mut downloads = 0usize;
    let mut deferred = 0usize;
    for f in &listing.files {
        let key = f.external_key();
        seen_keys.push(key.clone());
        seen_ids.push(f.id.clone());
        let marker = f.marker();
        if known.get(&f.id) == Some(&marker) {
            let live = utopia_store::documents::find_by_external_key(&state.pool, source.id, &key)
                .await?
                .is_some_and(|d| d.deleted_at.is_none());
            if live {
                continue;
            }
        }
        if downloads == google_drive::MAX_DOWNLOADS_PER_SYNC {
            deferred += 1;
            continue;
        }
        downloads += 1;
        // Files that cannot be read are skipped and what the library already has stays as it is; the rest syncs as usual, as with Notion
        let content = match drive.content(f).await {
            Ok(c) => c,
            Err(error) => {
                tracing::warn!(id = %f.id, error = %format!("{error:#}"), "google drive file could not be read, leaving the stored document as it is");
                unread.push(format!("{}: {error:#}", f.name));
                continue;
            }
        };
        let outcome = ingest_item_with_outcome(
            state,
            source.kb_id,
            source.id,
            &key,
            Identity::StableId,
            &content.filename,
            content.mime,
            &content.bytes,
            f.modified,
        )
        .await?;
        google_drive::remember(&state.pool, source.id, &f.id, &marker).await?;
        stats.absorb(outcome.action);
    }

    if listing.truncated || !listing.unreachable.is_empty() {
        utopia_store::documents::clear_missing_keys(&state.pool, source.id, &seen_keys).await?;
    } else {
        utopia_store::documents::reconcile_missing(&state.pool, source.id, &seen_keys).await?;
        google_drive::forget_others(&state.pool, source.id, &seen_ids).await?;
    }

    let mut gaps: Vec<String> = Vec::new();
    if !listing.unreachable.is_empty() {
        gaps.push(format!(
            "{} selected folder(s) could not be opened (deleted, or no longer shared with the connected account): {}",
            listing.unreachable.len(),
            listing.unreachable.join(", ")
        ));
    }
    if !unread.is_empty() {
        gaps.push(format!(
            "{} could not be read and keep their stored content: {}",
            unread.len(),
            unread
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if !listing.broken_shortcuts.is_empty() {
        gaps.push(format!(
            "{} shortcut(s) point to files the connected account cannot open: {}",
            listing.broken_shortcuts.len(),
            listing
                .broken_shortcuts
                .iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    if listing.truncated {
        gaps.push(
            "the folders go deeper or hold more files than one walk lists, so the rest were not looked at"
                .into(),
        );
    }
    if deferred > 0 {
        gaps.push(format!(
            "{deferred} more file(s) wait for the next sync (at most {} downloads per run)",
            google_drive::MAX_DOWNLOADS_PER_SYNC
        ));
    }
    anyhow::ensure!(
        gaps.is_empty(),
        "{} file(s) synced; {}",
        stats.total(),
        gaps.join("; ")
    );
    Ok(stats)
}

/// If a title has no extension, add one from the mime, so it enters the parse matrix's dispatch.
fn ensure_extension(title: &str, mime: &str) -> String {
    let has_ext = title
        .rsplit('.')
        .next()
        .map(|e| e.len() <= 5 && e.len() >= 2 && !e.contains(' '))
        .unwrap_or(false)
        && title.contains('.');
    if has_ext {
        return title.to_string();
    }
    let ext = if mime.contains("html") {
        "html"
    } else if mime.contains("plain") {
        "txt"
    } else {
        "md"
    };
    format!("{title}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::{
        bound_rss_identity, has_usable_rss_content, rss_entry_key, RSS_EXTERNAL_KEY_MAX_BYTES,
    };

    #[test]
    fn summary_only_does_not_make_an_rss_entry_hydratable_without_an_article_link() {
        assert!(!has_usable_rss_content(false, None));
        assert!(has_usable_rss_content(false, Some("https://example.com/a")));
        assert!(has_usable_rss_content(true, None));
    }

    #[test]
    fn overlong_rss_identity_is_hashed_without_prefix_collisions_from_truncation() {
        let first = "a".repeat(RSS_EXTERNAL_KEY_MAX_BYTES) + "-one";
        let second = "a".repeat(RSS_EXTERNAL_KEY_MAX_BYTES) + "-two";
        let first = bound_rss_identity(&first);
        let second = bound_rss_identity(&second);
        assert_ne!(first, second);
        assert!(first.len() < RSS_EXTERNAL_KEY_MAX_BYTES);
    }

    #[test]
    fn parser_leaves_missing_entry_ids_empty_for_application_fallback() {
        let feed = feed_rs::parser::Builder::new()
            .id_generator(|_, _, _| String::new())
            .build()
            .parse(
                &br#"<?xml version="1.0"?><rss version="2.0"><channel><title>Test</title><link>https://example.com/</link><description>Test</description><item><title>Original</title><description>Summary</description></item></channel></rss>"#[..],
            )
            .expect("test feed should parse");
        let entry = &feed.entries[0];
        assert!(entry.id.is_empty());
        assert!(rss_entry_key(entry, None).is_none());
        assert!(rss_entry_key(entry, Some("https://example.com/article")).is_some());
    }
}

#[cfg(test)]
#[path = "source_filename_tests.rs"]
mod source_filename_tests;

#[cfg(test)]
#[path = "github_sync_tests.rs"]
mod github_sync_tests;

#[cfg(test)]
#[path = "github_repo_sync_tests.rs"]
mod github_repo_sync_tests;
#[cfg(test)]
#[path = "gitlab_issues_sync_tests.rs"]
mod gitlab_issues_sync_tests;
#[cfg(test)]
#[path = "gitlab_repo_sync_tests.rs"]
mod gitlab_repo_sync_tests;
