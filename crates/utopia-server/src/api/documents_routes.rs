use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use percent_encoding::{percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use utopia_core::models::{Document, Role};
use utopia_core::AppError;
use uuid::Uuid;

use crate::auth::{AuthUser, SessionOrToken};
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct UploadQuery {
    /// Target folder source: uploads go straight into that folder (only kind=folder accepts uploads)
    #[serde(default)]
    pub source: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct ContentQuery {
    #[serde(default)]
    pub version: Option<i32>,
}

const PURGED_MESSAGE: &str = "The document contents have been purged";

fn gone(message: &'static str) -> Response {
    (StatusCode::GONE, Json(json!({ "error": message }))).into_response()
}

fn missing_blob_invariant(document_id: Uuid, sha256: &str) -> AppError {
    AppError::Other(anyhow::anyhow!(
        "document {document_id} ledger references unavailable blob {sha256}"
    ))
}

fn content_disposition(filename: &str) -> String {
    const FILENAME: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_');

    let fallback: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded = percent_encode(filename.as_bytes(), FILENAME);
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

fn content_headers(
    document: &Document,
    version: &utopia_store::documents::DocumentVersion,
    byte_count: usize,
) -> ApiResult<HeaderMap> {
    let mime = HeaderValue::from_str(&document.mime)
        .map_err(|_| anyhow::anyhow!("document {} has an invalid MIME header", document.id))?;
    let disposition = HeaderValue::from_str(&content_disposition(&document.filename))
        .map_err(|_| anyhow::anyhow!("document {} has an unsafe filename", document.id))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, mime);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&byte_count.to_string())
            .map_err(|_| anyhow::anyhow!("content length is not a valid header"))?,
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", version.sha256))
            .map_err(|_| anyhow::anyhow!("document digest is not a valid header"))?,
    );
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    Ok(headers)
}

pub(crate) async fn require_reader_kb(
    state: &AppState,
    reader: &SessionOrToken,
    kb_id: Uuid,
) -> ApiResult<utopia_core::models::KnowledgeBase> {
    let kb =
        utopia_store::access::require_kb(&state.pool, &reader.user, kb_id, Role::Viewer).await?;
    if !reader.reaches(kb_id) {
        return Err(AppError::NotFound.into());
    }
    Ok(kb)
}

/// Serve the retained original named by the document ledger.
pub async fn content(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(id): Path<Uuid>,
    Query(query): Query<ContentQuery>,
) -> ApiResult<Response> {
    if query.version.is_some_and(|version| version < 1) {
        return Err(AppError::invalid("bad_version", "Version must be 1 or greater").into());
    }
    let document = utopia_store::documents::get(&state.pool, id).await?;
    require_reader_kb(&state, &reader, document.kb_id).await?;
    if document.purged_at.is_some() {
        return Ok(gone(PURGED_MESSAGE));
    }
    let Some(retained) = read_retained(&state, id, query.version, |_| true).await? else {
        return Ok(gone(PURGED_MESSAGE));
    };
    let headers = content_headers(&retained.document, &retained.version, retained.bytes.len())?;
    Ok((StatusCode::OK, headers, retained.bytes).into_response())
}

/// One retained original with the ledger row and document row that named it.
struct Retained {
    document: Document,
    version: utopia_store::documents::DocumentVersion,
    bytes: Vec<u8>,
}

/// Read a retained original, `None` when the document has been purged or the
/// locked row is not one `serves` accepts.
///
/// Hold the row lock through the blob read. Replacement and purge otherwise
/// can move or delete the selected blob after the ledger says we may serve it.
/// A caller's own conditions on the row go in `serves`, so they are judged on
/// the row the lock holds rather than on one read before it.
async fn read_retained(
    state: &AppState,
    id: Uuid,
    requested: Option<i32>,
    serves: impl FnOnce(&Document) -> bool,
) -> ApiResult<Option<Retained>> {
    let mut tx = state.pool.begin().await?;
    let document: Document =
        sqlx::query_as("SELECT * FROM documents WHERE id = $1 FOR NO KEY UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if document.purged_at.is_some() || !serves(&document) {
        return Ok(None);
    }
    let version: utopia_store::documents::DocumentVersion = match requested {
        Some(requested) => sqlx::query_as(
            "SELECT version, sha256, size_bytes, ingested_at
               FROM document_versions WHERE document_id = $1 AND version = $2",
        )
        .bind(id)
        .bind(requested)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?,
        None => sqlx::query_as(
            "SELECT version, sha256, size_bytes, ingested_at
               FROM document_versions WHERE document_id = $1 AND sha256 = $2
               ORDER BY version DESC LIMIT 1",
        )
        .bind(id)
        .bind(&document.sha256)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "document {} has no ledger version for its current digest",
                id
            )
        })?,
    };
    let bytes = state
        .blob
        .get(&version.sha256)
        .await
        .map_err(|_| missing_blob_invariant(id, &version.sha256))?;
    tx.commit().await?;

    if version.size_bytes != bytes.len() as i64 {
        return Err(anyhow::anyhow!(
            "document {} version {} has an inaccurate ledger size",
            id,
            version.version
        )
        .into());
    }
    Ok(Some(Retained {
        document,
        version,
        bytes,
    }))
}

/// Name the exact retained versions the content route can address.
pub async fn versions(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let document = utopia_store::documents::get(&state.pool, id).await?;
    require_reader_kb(&state, &reader, document.kb_id).await?;
    if document.purged_at.is_some() {
        return Ok(gone(PURGED_MESSAGE));
    }
    let versions = utopia_store::documents::versions(&state.pool, id).await?;
    Ok((StatusCode::OK, Json(json!({ "versions": versions }))).into_response())
}

#[derive(Deserialize)]
pub struct ExternalIdQuery {
    pub external_id: String,
}

/// Read a pushed document back by the identity its source pushed it under (#1276).
///
/// The reader is the person, as on the content route; a source's push key is no
/// credential here, so a leaked key stays push-only (0052). A document in the
/// trash, a tombstoned identity and one never pushed are all 404, and purge
/// erases the identity, so a purged one is 404 too.
pub async fn by_external_id(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path((kb_id, source_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<ExternalIdQuery>,
) -> ApiResult<Response> {
    require_reader_kb(&state, &reader, kb_id).await?;
    let source = super::sources_routes::source_in_kb(&state, kb_id, source_id).await?;
    if !crate::pushed_keys::is_push_kind(&source.kind) {
        return Err(AppError::NotFound.into());
    }
    let external_id = query.external_id.trim();
    let key = crate::pushed_keys::pushed_key(&source, external_id);
    let found = utopia_store::documents::find_by_external_key(&state.pool, source.id, &key)
        .await?
        .ok_or(AppError::NotFound)?;
    let live_under_key = |document: &Document| {
        document.source_id == Some(source.id)
            && document.external_key.as_deref() == Some(key.as_str())
            && document.deleted_at.is_none()
            && document.missing_since.is_none()
    };
    let Retained {
        document,
        version,
        bytes,
    } = read_retained(&state, found.id, None, live_under_key)
        .await?
        .ok_or(AppError::NotFound)?;
    let versions = utopia_store::documents::versions(&state.pool, document.id).await?;
    let body = json!({
        "id": document.id,
        "external_id": external_id,
        "filename": document.filename,
        "mime": document.mime,
        "sha256": document.sha256,
        "size_bytes": document.size_bytes,
        "status": document.status,
        "doc_time": document.doc_time,
        "updated_at": document.updated_at,
        "version": version.version,
        "content": String::from_utf8_lossy(&bytes),
        "versions": versions,
    });
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// Batch upload (multipart, several files allowed). Duplicate content (same sha256 in the same KB) is skipped.
pub async fn upload(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let target_source = match q.source {
        Some(sid) => {
            let src = utopia_store::sources::get(&state.pool, sid).await?;
            if src.kb_id != kb_id || src.kind != "folder" {
                return Err(AppError::invalid(
                    "upload_needs_folder",
                    "Uploads can only target a folder source in this knowledge base",
                )
                .into());
            }
            Some(sid)
        }
        None => None,
    };

    let mut created: Vec<Document> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::invalid_detail("bad_upload", "Malformed upload", e.to_string()))?
    {
        let Some(filename) = field.file_name().map(String::from) else {
            continue;
        };
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = field.bytes().await.map_err(|e| {
            AppError::invalid_detail("upload_read_failed", "Failed to read upload", e.to_string())
        })?;
        if bytes.is_empty() {
            skipped.push(json!({ "filename": filename, "reason": "empty file" }));
            continue;
        }

        let sha256 = hex(&Sha256::digest(&bytes));
        state
            .blob
            .put(&sha256, &bytes)
            .await
            .map_err(AppError::Other)?;

        match utopia_store::documents::create_from_upload(
            &state.pool,
            kb_id,
            &filename,
            &mime,
            bytes.len() as i64,
            &sha256,
            target_source,
            content_time(&filename, &bytes),
        )
        .await
        {
            Ok(doc) => created.push(doc),
            Err(AppError::Conflict(_)) => {
                skipped.push(json!({ "filename": filename, "reason": "duplicate content" }));
            }
            Err(e) => return Err(e.into()),
        }
    }

    if created.is_empty() && skipped.is_empty() {
        return Err(AppError::invalid("no_files", "No files received").into());
    }
    Ok(Json(json!({ "created": created, "skipped": skipped })))
}

#[derive(serde::Deserialize)]
pub struct DocsQuery {
    /// Source scope: default = all; `none` = documents with no source; otherwise a source id
    #[serde(default)]
    pub source: Option<String>,
    /// Filename contains
    #[serde(default)]
    pub q: Option<String>,
    /// Extraction status: none | queued | extracting | done | failed
    #[serde(default)]
    pub graph: Option<String>,
    /// `deleted` = the "deleted" view: lists tombstones only (#268). Default = live ones
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// One page of the library.
///
/// **Changed to server-side filtering and paging**: previously the whole library was fetched and the front end sliced it. 27 documents is fine; 20,000 would push
/// the whole table into the browser. Client-side filtering also has a more hidden flaw: it can only filter what has already been fetched.
pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<DocsQuery>,
) -> ApiResult<Json<utopia_core::models::DocumentPage>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    let page = utopia_store::documents::page(
        &state.pool,
        kb_id,
        parse_scope(q.source.as_deref()),
        q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        q.graph.as_deref().filter(|s| !s.is_empty()),
        q.state.as_deref() == Some("deleted"),
        q.limit.unwrap_or(15).clamp(1, 200),
        q.offset.unwrap_or(0).max(0),
    )
    .await?;
    Ok(Json(page))
}

/// `None` = all, `Some(None)` = documents with no source, `Some(Some(id))` = one source.
///
/// An unrecognised string is treated as "all" rather than an error: this parameter comes from one click in the UI,
/// and one click should not turn the whole page into an error.
fn parse_scope(raw: Option<&str>) -> Option<Option<Uuid>> {
    match raw {
        None | Some("") => None,
        Some("none") => Some(None),
        Some(s) => s.parse().ok().map(Some),
    }
}

/// One-click retry of every extraction failure in this scope.
///
/// **The reason it exists is that retrying one at a time is too slow**: five failures in one source means five clicks, and failures tend to come
/// in batches (the model endpoint went down for a while, and everything that came in during that time failed).
pub async fn retry_failed(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
    Query(q): Query<DocsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Editor).await?;
    let ids =
        utopia_store::documents::failed_ids(&state.pool, kb_id, parse_scope(q.source.as_deref()))
            .await?;
    // Enqueue one by one rather than as one SQL batch status update: queueing has other side effects (cancelling running jobs,
    // clearing incremental markers), which live in `queue_extraction_one`; bypassing it would leave a half-done state
    let mut queued = 0usize;
    for id in &ids {
        if utopia_store::documents::queue_extraction_one(&state.pool, *id)
            .await
            .is_ok()
        {
            queued += 1;
        }
    }
    if queued > 0 {
        state.emit_document(kb_id, ids[0]);
    }
    Ok(Json(json!({ "queued": queued, "found": ids.len() })))
}

/// Document detail + all chunks (for the document viewer).
pub async fn detail(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, id).await?;
    utopia_store::access::require_kb(&state.pool, &user, doc.kb_id, Role::Viewer).await?;
    let chunks = utopia_store::documents::chunks_full(&state.pool, id).await?;
    Ok(Json(json!({ "document": doc, "chunks": chunks })))
}

/// Reverse evidence chain: facts extracted from the document's chunks (right column of the document viewer).
pub async fn extractions(
    State(state): State<AppState>,
    reader: SessionOrToken,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, id).await?;
    require_reader_kb(&state, &reader, doc.kb_id).await?;
    let facts = utopia_store::graph::document_extractions(&state.pool, id).await?;
    Ok(Json(json!({ "facts": facts })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, id).await?;
    utopia_store::access::require_kb(&state.pool, &user, doc.kb_id, Role::Editor).await?;

    // Tombstone, not subtraction (#268): the document, chunks, evidence and raw file are all kept; only facts with no other source are voided
    let report = utopia_store::documents::delete(&state.pool, doc.kb_id, id, Some(user.id)).await?;
    let search = state.search.clone();
    let did = id.to_string();
    tokio::task::spawn_blocking(move || search.delete_document(&did))
        .await
        .map_err(|e| AppError::Other(e.into()))?
        .map_err(AppError::Other)?;
    // The premise is voided, so derivations built on it become invalid too, without waiting for the next scheduled re-derivation
    settle_derivations(&state, doc.kb_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(doc.kb_id),
        user.id,
        "document.deleted",
        "document",
        Some(id),
        json!({
            "filename": doc.filename,
            "deletion_id": report.deletion_id,
            "invalidated_facts": report.invalidated_facts,
        }),
    )
    .await;
    state.emit_document(doc.kb_id, id);
    Ok(Json(json!({
        "ok": true,
        "deletion_id": report.deletion_id,
        "invalidated_facts": report.invalidated_facts,
    })))
}

/// Undo a deletion: the document, chunks and the facts voided this time come back as they were, and the index is rebuilt.
/// Sync hitting a tombstone and a same-content re-upload take the same store function; this is just the route a person presses
pub async fn restore(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, id).await?;
    utopia_store::access::require_kb(&state.pool, &user, doc.kb_id, Role::Editor).await?;
    let doc = utopia_store::documents::restore(&state.pool, doc.kb_id, id).await?;
    reindex(&state, &doc).await?;
    settle_derivations(&state, doc.kb_id).await?;
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(doc.kb_id),
        user.id,
        "document.restored",
        "document",
        Some(id),
        json!({ "filename": doc.filename }),
    )
    .await;
    state.emit_document(doc.kb_id, id);
    Ok(Json(json!({ "ok": true })))
}

/// Real delete (#268, second half): content is wiped and irreversible; only open for deleted documents, and only library admins can press it.
/// The library records it first (purged_at), then deletes the file: if the file delete fails, only an orphaned raw file is left; the other way round
/// would leave the library saying "still recoverable" when the raw file is already gone
pub async fn purge(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, id).await?;
    utopia_store::access::require_kb(&state.pool, &user, doc.kb_id, Role::Admin).await?;
    let report = utopia_store::documents::purge(&state.pool, doc.kb_id, id).await?;
    for sha in &report.blobs {
        if let Err(e) = state.blob.delete(sha).await {
            tracing::warn!(document = %id, sha, error = %e, "purge: blob left behind");
        }
    }
    let _ = utopia_store::audit::record(
        &state.pool,
        Some(doc.kb_id),
        user.id,
        "document.purged",
        "document",
        Some(id),
        json!({
            "filename": doc.filename,
            "chunks": report.chunks,
            "blobs": report.blobs.len(),
        }),
    )
    .await;
    state.emit_document(doc.kb_id, id);
    Ok(Json(
        json!({ "ok": true, "chunks": report.chunks, "blobs": report.blobs.len() }),
    ))
}

/// A revived document returns to the full-text index: its chunk text was always there, only the index entries are rewritten
pub async fn reindex(state: &AppState, doc: &Document) -> utopia_core::AppResult<()> {
    let chunks =
        utopia_store::documents::chunks_in_document(&state.pool, doc.kb_id, doc.id).await?;
    let pairs: Vec<(String, String)> = chunks
        .into_iter()
        .map(|c| (c.id.to_string(), c.text))
        .collect();
    let search = state.search.clone();
    let (kb, did) = (doc.kb_id.to_string(), doc.id.to_string());
    tokio::task::spawn_blocking(move || search.reindex_document(&kb, &did, &pairs))
        .await
        .map_err(|e| AppError::Other(e.into()))?
        .map_err(AppError::Other)?;
    Ok(())
}

/// If the premise changed, re-derive so derivations catch up; libraries with the switch off do not derive. Delete, undo and sync-revive all share this
pub(crate) async fn settle_derivations(
    state: &AppState,
    kb_id: Uuid,
) -> utopia_core::AppResult<()> {
    let kb = utopia_store::kbs::get(&state.pool, kb_id).await?;
    if kb.materialize_inferences {
        utopia_store::reasoning::materialize(&state.pool, kb_id).await?;
    }
    Ok(())
}

/// Reprocess (parser upgrade / retry after failure).
pub async fn reprocess(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let doc = utopia_store::documents::get(&state.pool, id).await?;
    utopia_store::access::require_kb(&state.pool, &user, doc.kb_id, Role::Editor).await?;
    utopia_store::documents::set_status(&state.pool, id, "pending").await?;
    let job_id = utopia_store::jobs::enqueue(
        &state.pool,
        "process_document",
        json!({ "document_id": id }),
    )
    .await?;
    Ok(Json(json!({ "job_id": job_id })))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Only recognise a complete date line that stands alone at the start; do not take event dates mentioned in the body as the document date (#610).
///
/// Order: **filename, then first line of the body**. The filename is always present when multipart is parsed;
/// corpus items such as announcements and filings are usually named by date, and whether the body says "29 February 2024 notice" or
/// the filename is "2024-02-29_gaoshu.txt" is the same thing to the reader, but only the filename
/// route can be read before the multipart record is stored. Date formats match the body (ISO or full-width Chinese).
fn content_time(filename: &str, bytes: &[u8]) -> Option<chrono::DateTime<chrono::Utc>> {
    if !is_text_extension(filename) {
        return None;
    }
    let stem = std::path::Path::new(filename).file_stem()?.to_str()?;
    if let Some(day) = parse_filename_date(stem) {
        return Some(day.and_hms_opt(0, 0, 0)?.and_utc());
    }
    // Only decode the first 4 KiB of the header: date lines are only recognised at the start. For formats like PDF and Word that need a date, read
    // their own metadata in their own parsers; do not guess here
    const HEADER_BYTES: usize = 4096;
    let text = utopia_ingest::decode_text(&bytes[..bytes.len().min(HEADER_BYTES)]);
    let header = if bytes.len() > HEADER_BYTES {
        // A truncated half-line may have text after the date, and must not be mistaken for a standalone date line.
        text.rsplit_once('\n')?.0
    } else {
        &text
    };
    let line = header
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let date = line
        .strip_prefix('（')
        .and_then(|s| s.strip_suffix('）'))
        .or_else(|| line.strip_prefix('(').and_then(|s| s.strip_suffix(')')))
        .unwrap_or(line);
    if !date.get(..4)?.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .or_else(|_| chrono::NaiveDate::parse_from_str(date, "%Y年%m月%d日"))
        .ok()?;
    // Consistent with the project's existing day-precision convention: UTC midnight is the storage convention; we do not guess the author's timezone.
    Some(day.and_hms_opt(0, 0, 0)?.and_utc())
}

/// Text-type extensions: only try date parsing for `txt` / `md` / `markdown`.
/// For PDF / Word and similar formats that need a date, read their own metadata in their own parsers (#610 decision 2).
fn is_text_extension(filename: &str) -> bool {
    let Some(ext) = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
    else {
        return false;
    };
    ["txt", "md", "markdown"]
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known))
}

/// Parse the ISO `YYYY-MM-DD` or the Chinese form `YYYY年M月D日` in the main name's prefix.
///
/// Does not depend on the `regex` crate: a hand-written scan is enough, and the failure modes are clear.
///  Strict match: `%Y` is 4 digits, `%m` / `%d` are 1-2 digits (consistent with body parsing; `chrono`
///  rejects impossible dates such as `2023-02-29`, but accepts `2024-2-9` and `2024-02-09`).
fn parse_filename_date(stem: &str) -> Option<chrono::NaiveDate> {
    let bytes = stem.as_bytes();
    // The first 4 characters must be digits; `chrono` would also reject this if it slipped past, but failing early saves an allocation
    if bytes.len() < 4 || !bytes[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    // ISO: `YYYY-MM-DD` or `YYYY-M-D`, at least 8 characters
    if bytes.len() >= 8 && bytes[4] == b'-' {
        // Find the next separator (position 5+), which can be `-` or another character (stop position)
        let after_year = 5;
        let month_end = find_ascii_digit_end(bytes, after_year);
        // Month: 1-2 digits
        let month_str = std::str::from_utf8(&bytes[after_year..month_end]).ok()?;
        // After the month there must be `-`
        if month_end >= bytes.len() || bytes[month_end] != b'-' {
            return None;
        }
        let day_start = month_end + 1;
        let day_end = find_ascii_digit_end(bytes, day_start);
        let day_str = std::str::from_utf8(&bytes[day_start..day_end]).ok()?;
        return chrono::NaiveDate::parse_from_str(
            &format!("{}-{}-{}", &stem[..4], month_str, day_str),
            "%Y-%m-%d",
        )
        .ok();
    }
    // Chinese form: `YYYY年M月D日`; the `年` must follow the year
    let year_str = std::str::from_utf8(&bytes[..4]).ok()?;
    let after_year = 4;
    let rest = stem.get(after_year..)?;
    let zh_tail = rest.strip_prefix('年')?;
    let (month_str, zh_tail) = read_ascii_digits(zh_tail)?;
    let zh_tail = zh_tail.strip_prefix('月')?;
    let (day_str, zh_tail) = read_ascii_digits(zh_tail)?;
    if !zh_tail.starts_with('日') {
        return None;
    }
    chrono::NaiveDate::parse_from_str(
        &format!("{year_str}年{month_str}月{day_str}日"),
        "%Y年%m月%d日",
    )
    .ok()
}

/// Starting from `start`, read 1-2 ASCII digits and return (digit string, remaining string).
fn read_ascii_digits(s: &str) -> Option<(&str, &str)> {
    let end = s
        .as_bytes()
        .iter()
        .take(2)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if end == 0 {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// Starting from `start`, walk forward over ASCII digits and return the position of the first non-digit (closed-open interval).
/// Running to the end is also allowed: returns `bytes.len()`.
fn find_ascii_digit_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i
}

#[cfg(test)]
#[path = "documents_routes_tests.rs"]
mod tests;

/// Extraction drop signal: facts that were extracted but could not be landed. Fetched for the whole library in one go: after
/// aggregation by (document × reason × specific object) the row count is small, and the Library both counts totals and expands details,
/// so it does not need a request per row.
pub async fn extraction_drops(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    let drops = utopia_store::extraction_drops::for_kb(&state.pool, kb_id).await?;
    Ok(Json(json!({ "drops": drops })))
}
