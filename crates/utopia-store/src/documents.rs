use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use utopia_core::models::{ChunkView, Document, DocumentPage, SourcedChunk};
use utopia_core::{AppError, AppResult};
use utopia_ingest::ChunkPiece;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &PgPool,
    kb_id: Uuid,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    source_id: Option<Uuid>,
    doc_time: Option<chrono::DateTime<chrono::Utc>>,
    external_key: Option<&str>,
) -> AppResult<Document> {
    create_with_time_source(
        pool,
        kb_id,
        filename,
        mime,
        size_bytes,
        sha256,
        source_id,
        doc_time,
        "source",
        external_key,
    )
    .await
}

/// Body detection only applies to file uploads; dates from JSON ingest and sync still use the original source semantics.
#[allow(clippy::too_many_arguments)]
pub async fn create_from_upload(
    pool: &PgPool,
    kb_id: Uuid,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    source_id: Option<Uuid>,
    content_time: Option<DateTime<Utc>>,
) -> AppResult<Document> {
    let mut tx = pool.begin().await?;
    let deleted: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM documents
          WHERE kb_id = $1 AND sha256 = $2 AND deleted_at IS NOT NULL AND purged_at IS NULL",
    )
    .bind(kb_id)
    .bind(sha256)
    .fetch_optional(&mut *tx)
    .await?;
    let document = if let Some(id) = deleted {
        let (document, _) = restore_tx(&mut tx, kb_id, id).await?;
        crate::jobs::enqueue_with_max_attempts_tx(
            &mut tx,
            "process_document",
            serde_json::json!({ "document_id": id }),
            3,
        )
        .await?;
        document
    } else {
        create_with_version_and_processing_on(
            &mut tx,
            kb_id,
            filename,
            mime,
            size_bytes,
            sha256,
            source_id,
            content_time,
            "content",
            None,
        )
        .await?
    };
    tx.commit().await?;
    Ok(document)
}

#[allow(clippy::too_many_arguments)]
async fn create_with_time_source(
    pool: &PgPool,
    kb_id: Uuid,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    source_id: Option<Uuid>,
    doc_time: Option<DateTime<Utc>>,
    time_source: &str,
    external_key: Option<&str>,
) -> AppResult<Document> {
    // The same content came back, and it had only been deleted: revive that document instead of hitting the (kb_id, sha256) unique
    // index with "already exists". This is the natural form of undoing a delete: re-uploading means "I want it back" (#268)
    if let Some((id,)) = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM documents
          WHERE kb_id = $1 AND sha256 = $2 AND deleted_at IS NOT NULL AND purged_at IS NULL",
    )
    .bind(kb_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await?
    {
        return restore(pool, kb_id, id).await;
    }
    // Reading the original needs the current digest found in the version ledger: the document and its version 1 are written together, and neither can be missing (#1204 fixed
    // the file upload path; JSON pushes go through here)
    let mut tx = pool.begin().await?;
    let document: Document = sqlx::query_as(
        "INSERT INTO documents (id, kb_id, filename, mime, size_bytes, sha256, source_id,
                                doc_time, doc_time_source, external_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(filename)
    .bind(mime)
    .bind(size_bytes)
    .bind(sha256)
    .bind(source_id)
    .bind(doc_time)
    // No date means no date: the upload moment is the record time, not the document's date (0045 decision 3, #714)
    .bind(if doc_time.is_some() {
        time_source
    } else {
        "none"
    })
    .bind(external_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => AppError::Conflict(format!(
            "File already exists (identical content): {filename}"
        )),
        _ => AppError::Db(e),
    })?;
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
         VALUES ($1, $2, 1, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(document.id)
    .bind(sha256)
    .bind(size_bytes)
    .bind(document.doc_time)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(document)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_with_version_and_processing(
    pool: &PgPool,
    kb_id: Uuid,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    source_id: Option<Uuid>,
    doc_time: Option<chrono::DateTime<chrono::Utc>>,
    external_key: Option<&str>,
) -> AppResult<Document> {
    let mut tx = pool.begin().await?;
    let document = create_with_version_and_processing_on(
        &mut tx,
        kb_id,
        filename,
        mime,
        size_bytes,
        sha256,
        source_id,
        doc_time,
        "source",
        external_key,
    )
    .await?;
    tx.commit().await?;
    Ok(document)
}

// Content reads require the current digest in the version ledger. Upload used to
// commit only the document, so a successful upload could never serve its original.
#[allow(clippy::too_many_arguments)]
async fn create_with_version_and_processing_on(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    source_id: Option<Uuid>,
    doc_time: Option<DateTime<Utc>>,
    time_source: &str,
    external_key: Option<&str>,
) -> AppResult<Document> {
    let document: Document = sqlx::query_as(
        "INSERT INTO documents (id, kb_id, filename, mime, size_bytes, sha256, source_id,
                                doc_time, doc_time_source, external_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(filename)
    .bind(mime)
    .bind(size_bytes)
    .bind(sha256)
    .bind(source_id)
    .bind(doc_time)
    // No date means no date: the sync moment is the record time, not the document's date (0045 decision 3, #714)
    .bind(if doc_time.is_some() {
        time_source
    } else {
        "none"
    })
    .bind(external_key)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => AppError::Conflict(format!(
            "File already exists (identical content): {filename}"
        )),
        _ => AppError::Db(e),
    })?;
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
         VALUES ($1, $2, 1, $3, $4, (SELECT doc_time FROM documents WHERE id = $2))",
    )
    .bind(Uuid::now_v7())
    .bind(document.id)
    .bind(sha256)
    .bind(size_bytes)
    .execute(&mut **tx)
    .await?;
    crate::jobs::enqueue_with_max_attempts_tx(
        tx,
        "process_document",
        serde_json::json!({ "document_id": document.id }),
        3,
    )
    .await?;
    Ok(document)
}

pub async fn replace_content_and_enqueue_processing(
    pool: &PgPool,
    id: Uuid,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    doc_time: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM documents WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    sqlx::query(
        "UPDATE documents SET filename = $2, mime = $3, size_bytes = $4, sha256 = $5,
                doc_time = COALESCE($6, doc_time),
                doc_time_source = CASE WHEN $6 IS NULL THEN doc_time_source ELSE 'source' END,
                status = 'pending', graph_status = 'none', error = NULL,
                missing_since = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(filename)
    .bind(mime)
    .bind(size_bytes)
    .bind(sha256)
    .bind(doc_time)
    .execute(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => AppError::Conflict(format!(
            "File already exists (identical content): {filename}"
        )),
        _ => AppError::Db(e),
    })?;
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
         VALUES ($1, $2,
                 (SELECT coalesce(max(version), 0) + 1 FROM document_versions WHERE document_id = $2),
                 $3, $4, (SELECT doc_time FROM documents WHERE id = $2))",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(sha256)
    .bind(size_bytes)
    .execute(&mut *tx)
    .await?;
    crate::jobs::enqueue_with_max_attempts_tx(
        &mut tx,
        "process_document",
        serde_json::json!({ "document_id": id }),
        3,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn upsert_source_document_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    source_id: Uuid,
    external_key: &str,
    filename: &str,
    mime: &str,
    size_bytes: i64,
    sha256: &str,
    doc_time: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Document> {
    // Reuse the ordinary restoration transaction (including facts/chunks), not
    // a second RSS-specific tombstone update. The caller owns source fencing.
    let deleted: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM documents WHERE source_id = $1 AND external_key = $2
           AND deleted_at IS NOT NULL AND purged_at IS NULL",
    )
    .bind(source_id)
    .bind(external_key)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(id) = deleted {
        restore_tx(tx, kb_id, id).await?;
    }
    let existing: Option<Document> = sqlx::query_as(
        "SELECT * FROM documents
          WHERE source_id = $1 AND external_key = $2 AND deleted_at IS NULL
          FOR UPDATE",
    )
    .bind(source_id)
    .bind(external_key)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(document) = existing {
        if document.sha256 == sha256 {
            return Ok(document);
        }
        sqlx::query(
            "UPDATE documents SET filename = $2, mime = $3, size_bytes = $4, sha256 = $5,
                    doc_time = COALESCE($6, doc_time), status = 'pending', graph_status = 'none',
                    error = NULL, missing_since = NULL, updated_at = now()
             WHERE id = $1",
        )
        .bind(document.id)
        .bind(filename)
        .bind(mime)
        .bind(size_bytes)
        .bind(sha256)
        .bind(doc_time)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
             VALUES ($1, $2,
                     (SELECT coalesce(max(version), 0) + 1 FROM document_versions WHERE document_id = $2),
                     $3, $4, (SELECT doc_time FROM documents WHERE id = $2))",
        )
        .bind(Uuid::now_v7())
        .bind(document.id)
        .bind(sha256)
        .bind(size_bytes)
        .execute(&mut **tx)
        .await?;
        crate::jobs::enqueue_with_max_attempts_tx(
            tx,
            "process_document",
            serde_json::json!({ "document_id": document.id }),
            3,
        )
        .await?;
        return Ok(sqlx::query_as("SELECT * FROM documents WHERE id = $1")
            .bind(document.id)
            .fetch_one(&mut **tx)
            .await?);
    }

    if let Some(document) = sqlx::query_as::<_, Document>(
        "SELECT * FROM documents
          WHERE source_id = $1 AND external_key IS NULL AND filename = $2
            AND deleted_at IS NULL LIMIT 1 FOR UPDATE",
    )
    .bind(source_id)
    .bind(filename)
    .fetch_optional(&mut **tx)
    .await?
    {
        sqlx::query("UPDATE documents SET external_key = $2, updated_at = now() WHERE id = $1")
            .bind(document.id)
            .bind(external_key)
            .execute(&mut **tx)
            .await?;
        if document.sha256 != sha256 {
            sqlx::query(
                "UPDATE documents SET filename = $2, mime = $3, size_bytes = $4, sha256 = $5,
                        doc_time = COALESCE($6, doc_time), status = 'pending', graph_status = 'none',
                        error = NULL, missing_since = NULL, updated_at = now()
                 WHERE id = $1",
            )
            .bind(document.id)
            .bind(filename)
            .bind(mime)
            .bind(size_bytes)
            .bind(sha256)
            .bind(doc_time)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
                 VALUES ($1, $2,
                         (SELECT coalesce(max(version), 0) + 1 FROM document_versions WHERE document_id = $2),
                         $3, $4, (SELECT doc_time FROM documents WHERE id = $2))",
            )
            .bind(Uuid::now_v7())
            .bind(document.id)
            .bind(sha256)
            .bind(size_bytes)
            .execute(&mut **tx)
            .await?;
            crate::jobs::enqueue_with_max_attempts_tx(
                tx,
                "process_document",
                serde_json::json!({ "document_id": document.id }),
                3,
            )
            .await?;
        }
        return Ok(sqlx::query_as("SELECT * FROM documents WHERE id = $1")
            .bind(document.id)
            .fetch_one(&mut **tx)
            .await?);
    }

    let document: Document = sqlx::query_as(
        "INSERT INTO documents (id, kb_id, filename, mime, size_bytes, sha256, source_id,
                                doc_time, doc_time_source, external_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(kb_id)
    .bind(filename)
    .bind(mime)
    .bind(size_bytes)
    .bind(sha256)
    .bind(source_id)
    .bind(doc_time)
    // No date means no date: the sync moment is the record time, not the document's date (0045 decision 3, #714)
    .bind(if doc_time.is_some() { "source" } else { "none" })
    .bind(external_key)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => AppError::Conflict(format!(
            "File already exists (identical content): {filename}"
        )),
        _ => AppError::Db(e),
    })?;
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
         VALUES ($1, $2, 1, $3, $4, (SELECT doc_time FROM documents WHERE id = $2))",
    )
    .bind(Uuid::now_v7())
    .bind(document.id)
    .bind(sha256)
    .bind(size_bytes)
    .execute(&mut **tx)
    .await?;
    crate::jobs::enqueue_with_max_attempts_tx(
        tx,
        "process_document",
        serde_json::json!({ "document_id": document.id }),
        3,
    )
    .await?;
    Ok(document)
}

/// One page of the library, **with filters and counts**.
///
/// It used to be `SELECT * FROM documents WHERE kb_id = $1` with no limit and client-side paging in the frontend.
/// 27 documents was fine; 20,000 would pull the whole table into the browser. Client-side filtering has a subtler flaw:
/// it can only filter the rows it has already fetched.
///
/// Counts are computed separately, and **scoped only by source, unaffected by name/status filters**:
/// "how many in this source can be extracted" is the scope of the two bulk buttons, unrelated to what you are searching for right now.
#[allow(clippy::too_many_arguments)]
pub async fn page(
    pool: &PgPool,
    kb_id: Uuid,
    // None = all; Some(None) = only those without a source; Some(Some(id)) = one source
    source: Option<Option<Uuid>>,
    q: Option<&str>,
    graph_status: Option<&str>,
    // true = "deleted" view: only tombstones (deleted but not purged); false = live ones
    deleted: bool,
    limit: i64,
    offset: i64,
) -> AppResult<DocumentPage> {
    // All three filters are written as "a null parameter has no effect", so one SQL covers every combination.
    // The `$2 = 'any'` branch means "don't filter by source", `'none'` means "only those without a source" —
    // two sentinel strings rather than two nullable parameters, because NULL is ambiguous here:
    // it could mean either "don't filter" or "filter to source_id IS NULL"
    const WHERE: &str = "WHERE kb_id = $1
           AND (($5::bool AND deleted_at IS NOT NULL AND purged_at IS NULL)
                OR (NOT $5::bool AND deleted_at IS NULL))
           AND ($2 = 'any'
                OR ($2 = 'none' AND source_id IS NULL)
                OR source_id::text = $2)
           AND ($3::text IS NULL OR filename ILIKE '%' || $3 || '%')
           AND ($4::text IS NULL OR graph_status = $4)";
    let scope = match source {
        None => "any".to_string(),
        Some(None) => "none".to_string(),
        Some(Some(id)) => id.to_string(),
    };

    let docs: Vec<Document> = sqlx::query_as(&format!(
        "SELECT * FROM documents {WHERE}
         ORDER BY (CASE WHEN $5::bool THEN deleted_at ELSE created_at END) DESC, id DESC
         LIMIT $6 OFFSET $7"
    ))
    .bind(kb_id)
    .bind(&scope)
    .bind(q)
    .bind(graph_status)
    .bind(deleted)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let (total,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM documents {WHERE}"))
        .bind(kb_id)
        .bind(&scope)
        .bind(q)
        .bind(graph_status)
        .bind(deleted)
        .fetch_one(pool)
        .await?;

    // Counts are scoped only by source: the two bulk buttons act on the whole source, not on the few rows your search returned.
    // The four numbers each map to one column: `ready` is ingestion (`status`), the last three are extraction (`graph_status`).
    // The two dimensions cannot be added: a document that is "ingested, graph still extracting" counts on both sides, so if the progress bar used
    // `ready + extracting` as the denominator, two documents would show as 2 / 4
    let stats: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
           count(*) FILTER (WHERE status = 'ready'),
           count(*) FILTER (WHERE graph_status = 'done'),
           count(*) FILTER (WHERE graph_status IN ('queued', 'extracting')),
           count(*) FILTER (WHERE graph_status = 'failed')
         FROM documents
         WHERE kb_id = $1 AND deleted_at IS NULL
           AND ($2 = 'any'
                OR ($2 = 'none' AND source_id IS NULL)
                OR source_id::text = $2)",
    )
    .bind(kb_id)
    .bind(&scope)
    .fetch_one(pool)
    .await?;

    // The tombstone count is for the whole store: that row in the left column does not change with scope
    let (deleted_total,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM documents
          WHERE kb_id = $1 AND deleted_at IS NOT NULL AND purged_at IS NULL",
    )
    .bind(kb_id)
    .fetch_one(pool)
    .await?;

    Ok(DocumentPage {
        docs,
        total,
        ready: stats.0,
        done: stats.1,
        extracting: stats.2,
        failed: stats.3,
        deleted: deleted_total,
    })
}

/// Document ids whose extraction failed in this source (or the whole store). **The one-click retry needs exactly this list**.
pub async fn failed_ids(
    pool: &PgPool,
    kb_id: Uuid,
    source: Option<Option<Uuid>>,
) -> AppResult<Vec<Uuid>> {
    let scope = match source {
        None => "any".to_string(),
        Some(None) => "none".to_string(),
        Some(Some(id)) => id.to_string(),
    };
    Ok(sqlx::query_scalar(
        "SELECT id FROM documents
          WHERE kb_id = $1 AND deleted_at IS NULL AND graph_status = 'failed'
            AND ($2 = 'any'
                 OR ($2 = 'none' AND source_id IS NULL)
                 OR source_id::text = $2)",
    )
    .bind(kb_id)
    .bind(&scope)
    .fetch_all(pool)
    .await?)
}

pub async fn get(pool: &PgPool, id: Uuid) -> AppResult<Document> {
    sqlx::query_as("SELECT * FROM documents WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(AppError::NotFound)
}

/// The input to one processing run; the version number distinguishes content A → B → A, so comparing SHAs alone is not enough.
pub struct ProcessingInput {
    pub document: Document,
    pub version: i32,
}

pub async fn processing_input(pool: &PgPool, id: Uuid) -> AppResult<ProcessingInput> {
    let mut tx = pool.begin().await?;
    let document = sqlx::query_as("SELECT * FROM documents WHERE id = $1 FOR NO KEY UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let version = processing_version_tx(&mut tx, id).await?;
    tx.commit().await?;
    Ok(ProcessingInput { document, version })
}

async fn processing_version_tx(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> AppResult<i32> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(max(version), 0) FROM document_versions WHERE document_id = $1",
    )
    .bind(id)
    .fetch_one(&mut **tx)
    .await?)
}

/// Read the version after taking the lock: after waiting for the content-update transaction to commit, it must see the version it just appended.
async fn lock_processing_input_tx(
    tx: &mut Transaction<'_, Postgres>,
    input: &ProcessingInput,
) -> AppResult<bool> {
    let current: Option<(String, bool)> = sqlx::query_as(
        "SELECT sha256, deleted_at IS NOT NULL FROM documents WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(input.document.id)
    .fetch_optional(&mut **tx)
    .await?;
    if !matches!(current, Some((ref sha, false)) if sha == &input.document.sha256) {
        return Ok(false);
    }
    Ok(processing_version_tx(tx, input.document.id).await? == input.version)
}

pub async fn processing_is_current(pool: &PgPool, input: &ProcessingInput) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    let current = lock_processing_input_tx(&mut tx, input).await?;
    tx.commit().await?;
    Ok(current)
}

/// A recorded original, with only the facts the ingestion ledger guarantees.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DocumentVersion {
    pub version: i32,
    pub sha256: String,
    pub size_bytes: i64,
    pub ingested_at: DateTime<Utc>,
}

/// Every retained original version, oldest first.
///
/// This deliberately includes soft-deleted documents: their bytes remain part
/// of the auditable record until the separate purge action removes them.
pub async fn versions(pool: &PgPool, id: Uuid) -> AppResult<Vec<DocumentVersion>> {
    Ok(sqlx::query_as(
        "SELECT version, sha256, size_bytes, ingested_at
           FROM document_versions WHERE document_id = $1 ORDER BY version",
    )
    .bind(id)
    .fetch_all(pool)
    .await?)
}

/// Fetch a document narrowed by kb. **When the id comes from the model, only this branch may be used**: `get` looks up by id only,
/// and an id from another store would still be found.
pub async fn find_in_kb(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<Option<Document>> {
    Ok(sqlx::query_as(
        "SELECT * FROM documents WHERE id = $1 AND kb_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(kb_id)
    .fetch_optional(pool)
    .await?)
}

/// Fetch a document by its logical identity within a source (used by the three-way sync decision).
pub async fn find_by_external_key(
    pool: &PgPool,
    source_id: Uuid,
    external_key: &str,
) -> AppResult<Option<Document>> {
    Ok(
        sqlx::query_as("SELECT * FROM documents WHERE source_id = $1 AND external_key = $2")
            .bind(source_id)
            .bind(external_key)
            .fetch_optional(pool)
            .await?,
    )
}

/// Fetch a document by source + content hash (used to recognize renames/moves).
pub async fn find_by_source_sha(
    pool: &PgPool,
    source_id: Uuid,
    sha256: &str,
) -> AppResult<Option<Document>> {
    Ok(sqlx::query_as(
        "SELECT * FROM documents
              WHERE source_id = $1 AND sha256 = $2 AND deleted_at IS NULL LIMIT 1",
    )
    .bind(source_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await?)
}

/// The document in the store holding this content (the range covered by `documents_kb_sha_idx`: not purged, including those in the trash).
pub async fn find_by_kb_sha(
    pool: &PgPool,
    kb_id: Uuid,
    sha256: &str,
) -> AppResult<Option<Document>> {
    Ok(sqlx::query_as(
        "SELECT * FROM documents WHERE kb_id = $1 AND sha256 = $2 AND purged_at IS NULL",
    )
    .bind(kb_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await?)
}

/// Legacy documents from before the migration (no external_key) are claimed by file name: a one-time fallback for rows created before 0008.
pub async fn find_legacy_by_filename(
    pool: &PgPool,
    source_id: Uuid,
    filename: &str,
) -> AppResult<Option<Document>> {
    Ok(sqlx::query_as(
        "SELECT * FROM documents
         WHERE source_id = $1 AND external_key IS NULL AND filename = $2
           AND deleted_at IS NULL LIMIT 1",
    )
    .bind(source_id)
    .bind(filename)
    .fetch_optional(pool)
    .await?)
}

/// Backfills a logical identity onto legacy documents.
pub async fn adopt_external_key(pool: &PgPool, id: Uuid, external_key: &str) -> AppResult<()> {
    sqlx::query("UPDATE documents SET external_key = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(external_key)
        .execute(pool)
        .await?;
    Ok(())
}

/// A document's time context (0045 decision 3): its own date, the periods and calendars it defines, and the anchors the narrative sets.
/// The server fills it in during extraction and overwrites it whole; `time_context_at` records when it was last written, and later re-resolutions
/// take it as input
pub async fn set_time_context(
    pool: &PgPool,
    id: Uuid,
    context: &serde_json::Value,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET time_context = $2, time_context_at = now(), updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(context)
    .execute(pool)
    .await?;
    Ok(())
}

/// The document's own date, read from the body (0045 decision 3): `doc_time_source = 'content'`.
/// Only a date from the body or the source system counts as the document's date ([`Document::dated_at`]); the upload moment does not go here
pub async fn set_content_date(pool: &PgPool, id: Uuid, date: DateTime<Utc>) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET doc_time = $2, doc_time_source = 'content', updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(date)
    .execute(pool)
    .await?;
    Ok(())
}

/// Move/rename: same content at a new path; only the identity is updated, the pipeline is not rerun.
pub async fn update_location(
    pool: &PgPool,
    id: Uuid,
    filename: &str,
    external_key: &str,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET filename = $2, external_key = $3, missing_since = NULL,
                updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(filename)
    .bind(external_key)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a content version (the version number increments).
pub async fn record_version(
    pool: &PgPool,
    document_id: Uuid,
    sha256: &str,
    size_bytes: i64,
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    // Appending a version must also be mutually exclusive with processing write-backs; the foreign key's KEY SHARE lock does not block it.
    sqlx::query("SELECT id FROM documents WHERE id = $1 FOR NO KEY UPDATE")
        .bind(document_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes, doc_time)
         VALUES ($1, $2,
                 (SELECT coalesce(max(version), 0) + 1 FROM document_versions WHERE document_id = $2),
                 $3, $4, (SELECT doc_time FROM documents WHERE id = $2))",
    )
    .bind(Uuid::now_v7())
    .bind(document_id)
    .bind(sha256)
    .bind(size_bytes)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Full-set reconciliation (only for source types whose complete current state is visible, such as a url config list):
/// keys seen this round have their missing mark cleared, keys not seen get marked.
/// rss (sliding window) and custom's incremental responses (?since=) cannot use this function: absent ≠ deleted.
pub async fn reconcile_missing(
    pool: &PgPool,
    source_id: Uuid,
    seen_keys: &[String],
) -> AppResult<()> {
    clear_missing_keys(pool, source_id, seen_keys).await?;
    sqlx::query(
        "UPDATE documents SET missing_since = now(), updated_at = now()
         WHERE source_id = $1 AND missing_since IS NULL
           AND external_key IS NOT NULL AND NOT (external_key = ANY($2))",
    )
    .bind(source_id)
    .bind(seen_keys)
    .execute(pool)
    .await?;
    Ok(())
}

/// Entries that appear this round have their missing mark cleared (the entry came back).
pub async fn clear_missing_keys(pool: &PgPool, source_id: Uuid, keys: &[String]) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET missing_since = NULL, updated_at = now()
         WHERE source_id = $1 AND missing_since IS NOT NULL AND external_key = ANY($2)",
    )
    .bind(source_id)
    .bind(keys)
    .execute(pool)
    .await?;
    Ok(())
}

/// Explicit tombstones (custom responses' deleted[]): marked only when the source declares a deletion, never inferred from absence.
pub async fn mark_missing_keys(pool: &PgPool, source_id: Uuid, keys: &[String]) -> AppResult<u64> {
    let res = sqlx::query(
        "UPDATE documents SET missing_since = now(), updated_at = now()
         WHERE source_id = $1 AND missing_since IS NULL AND external_key = ANY($2)",
    )
    .bind(source_id)
    .bind(keys)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Ids of all documents under this source marked missing (for bulk cleanup).
pub async fn list_missing(pool: &PgPool, source_id: Uuid) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM documents
          WHERE source_id = $1 AND missing_since IS NOT NULL AND deleted_at IS NULL",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

pub async fn set_status(pool: &PgPool, id: Uuid, status: &str) -> AppResult<()> {
    // Starting processing again: the reader model that was missing last time no longer counts; if it still can't be read, it is recorded again
    sqlx::query(
        "UPDATE documents SET status = $2, error = NULL, reader_needed = NULL, updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_status_if_current(
    pool: &PgPool,
    input: &ProcessingInput,
    status: &str,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    if !lock_processing_input_tx(&mut tx, input).await? {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE documents SET status = $2, error = NULL, reader_needed = NULL, updated_at = now()
          WHERE id = $1",
    )
    .bind(input.document.id)
    .bind(status)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Only record a reason when the current input fails; when the reader model is missing, still wait with the original semantics until it is configured, then requeue.
pub async fn set_failed_if_current(
    pool: &PgPool,
    input: &ProcessingInput,
    error: &str,
    reader: Option<&str>,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    if !lock_processing_input_tx(&mut tx, input).await? {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE documents SET status = 'failed', error = $2,
                reader_needed = coalesce($3, reader_needed), reader_task = NULL,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(input.document.id)
    .bind(error)
    .bind(reader)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// When a `reader`-type model is configured: the live documents in this workspace waiting for it are requeued for processing.
///
/// The state change and task enqueueing happen in one transaction, so there is no half-state where "status back to pending, but no task queued".
/// Returns (document, store) so the caller can push list updates one by one
pub async fn requeue_waiting_for_reader(
    pool: &PgPool,
    workspace_id: Uuid,
    reader: &str,
) -> AppResult<Vec<(Uuid, Uuid)>> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE documents d SET status = 'pending', error = NULL, reader_needed = NULL,
                reader_task = NULL, updated_at = now()
           FROM knowledge_bases k
          WHERE k.id = d.kb_id AND k.workspace_id = $1
            AND d.reader_needed = $2 AND d.deleted_at IS NULL
         RETURNING d.id, d.kb_id",
    )
    .bind(workspace_id)
    .bind(reader)
    .fetch_all(&mut *tx)
    .await?;
    for (id, _) in &rows {
        crate::jobs::enqueue_with_max_attempts_tx(
            &mut tx,
            "process_document",
            serde_json::json!({ "document_id": id }),
            3,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(rows)
}

/// Remote task currently reading this file (0040 second slice; shape in migration 0059)
pub async fn reader_task(pool: &PgPool, id: Uuid) -> AppResult<Option<serde_json::Value>> {
    Ok(
        sqlx::query_scalar("SELECT reader_task FROM documents WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .flatten(),
    )
}

/// Records the remote task just submitted. **Only recorded when there is no task yet**: if the same document was queued twice and two processing tasks
/// were submitted at once, only the one recorded first counts, and the other is handed back to ask it — returns false
pub async fn claim_reader_task(
    pool: &PgPool,
    id: Uuid,
    task: &serde_json::Value,
) -> AppResult<bool> {
    let done =
        sqlx::query("UPDATE documents SET reader_task = $2 WHERE id = $1 AND reader_task IS NULL")
            .bind(id)
            .bind(task)
            .execute(pool)
            .await?;
    Ok(done.rows_affected() == 1)
}

/// Voids a remote task (read finished, file changed, or the service forgot it)
pub async fn clear_reader_task(pool: &PgPool, id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE documents SET reader_task = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_ready(pool: &PgPool, id: Uuid, text_len: i32, chunk_count: i32) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET status = 'ready', error = NULL, reader_needed = NULL, reader_task = NULL,
                text_len = $2,
                chunk_count = $3, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(text_len)
    .bind(chunk_count)
    .execute(pool)
    .await?;
    Ok(())
}

/// Readiness, extraction state, and enqueueing commit together; a stale task must not leave follow-up work for the new version.
pub async fn finish_processing_if_current(
    pool: &PgPool,
    input: &ProcessingInput,
    text_len: i32,
    chunk_count: i32,
    graph_status: Option<&str>,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    if !lock_processing_input_tx(&mut tx, input).await? {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE documents SET status = 'ready', error = NULL, reader_needed = NULL, reader_task = NULL,
                text_len = $2, chunk_count = $3,
                graph_status = coalesce($4, graph_status),
                graph_error = CASE WHEN $4 IS NULL THEN graph_error ELSE NULL END,
                updated_at = now() WHERE id = $1",
    )
    .bind(input.document.id)
    .bind(text_len)
    .bind(chunk_count)
    .bind(graph_status)
    .execute(&mut *tx)
    .await?;
    if graph_status == Some("queued") {
        crate::jobs::enqueue_extraction_tx(
            &mut tx,
            serde_json::json!({ "document_id": input.document.id }),
            3,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Output of one deletion, used for audit and UI notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeletionReport {
    pub deletion_id: Uuid,
    /// Facts voided along with it: only those where "every source has been deleted"
    pub invalidated_facts: usize,
    pub superseded_chunks: usize,
}

/// Deleting a document — **an event on the epistemic axis, not a subtraction** (#268).
///
/// It used to be one `DELETE` statement here, and foreign keys cascaded away the chunks and evidence together: facts stayed in the graph, alive, but lost their
/// sources, while the privacy policy says "facts are kept together with their sources". Now the document is tombstoned, and chunks go through `replace_chunks`
/// the long-used `superseded_at`, **clearing no content at all** — the original file was never deleted either; only facts are voided
/// where "every source has been deleted". The criterion looks at `documents.deleted_at`, not the chunks'
/// `superseded_at`: a fact whose evidence rests on an **old version** of the document is stale, and per the `stale_facts` rule
/// "not mentioned again ≠ false", it is left to a person and not voided; it is voided only if the source itself is gone.
///
/// Voided facts and tagged chunks are recorded in `document_deletions`, which [`restore`] reads back the same way.
/// `actor` None = engine (bulk cleanup from source reconciliation)
pub async fn delete(
    pool: &PgPool,
    kb_id: Uuid,
    id: Uuid,
    actor: Option<Uuid>,
) -> AppResult<DeletionReport> {
    let mut tx = pool.begin().await?;
    lock_document_source_tx(&mut tx, id).await?;
    let hit = sqlx::query(
        "UPDATE documents SET deleted_at = now(), updated_at = now()
          WHERE id = $1 AND kb_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(kb_id)
    .execute(&mut *tx)
    .await?;
    if hit.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    let chunks: Vec<(Uuid,)> = sqlx::query_as(
        "UPDATE chunks SET superseded_at = now()
          WHERE document_id = $1 AND superseded_at IS NULL RETURNING id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    // A voided fact may be the successor of another value: when it goes, the predecessor it was closed against must reconnect; if it was not voided but only lost
    // this evidence, its ordering date may also change. The affected timelines are locked first in a fixed order, then any row is voided
    // (same order as retraction merging, see the temporal module header)
    let (cited, _timelines) = lock_cited_timelines(&mut tx, kb_id, id, &[]).await?;
    // Tombstone first, then compute: this document now counts as "deleted", so only facts whose sole source is it get caught;
    // a fact that also has evidence in another live document stays put — deleting one duplicate upload should not topple half the picture
    let facts: Vec<(Uuid,)> = sqlx::query_as(
        "UPDATE facts f SET invalidated_at = now()
          WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND f.id = ANY($3)
            AND EXISTS (SELECT 1 FROM fact_evidence fe
                        JOIN chunks c ON c.id = fe.chunk_id
                        WHERE fe.fact_id = f.id AND c.document_id = $2)
            AND NOT EXISTS (SELECT 1 FROM fact_evidence fe
                            JOIN chunks c ON c.id = fe.chunk_id
                            JOIN documents d ON d.id = c.document_id
                            WHERE fe.fact_id = f.id AND d.deleted_at IS NULL)
            -- 实体的本名不随文档走（0041）：建实体时就记下了这条名字事实，这篇文档只是
            -- 给它补过出处。删掉出处，实体还叫这个名字，名字栏里不能少了它
            AND NOT EXISTS (SELECT 1 FROM relation_types nr
                             JOIN entities ne ON ne.id = f.subject_id
                            WHERE nr.id = f.predicate_id AND nr.builtin AND nr.key = 'known_as'
                              AND lower(f.object_value->>'value') = lower(ne.canonical_name))
          RETURNING f.id",
    )
    .bind(kb_id)
    .bind(id)
    .bind(&cited)
    .fetch_all(&mut *tx)
    .await?;
    let chunk_ids: Vec<Uuid> = chunks.into_iter().map(|(c,)| c).collect();
    let fact_ids: Vec<Uuid> = facts.into_iter().map(|(f,)| f).collect();
    let reattest = defer_reattest_tx(&mut tx, kb_id, &cited).await?;
    let deletion_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO document_deletions
            (id, kb_id, document_id, deleted_by, invalidated_facts, superseded_chunks,
             external_key, created_at)
         VALUES ($1, $2, $3, $4, $5, $6,
                 (SELECT external_key FROM documents WHERE id = $3), clock_timestamp())",
    )
    .bind(deletion_id)
    .bind(kb_id)
    .bind(id)
    .bind(actor)
    .bind(&fact_ids)
    .bind(&chunk_ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    settle_reattest(pool, reattest).await?;
    Ok(DeletionReport {
        deletion_id,
        invalidated_facts: fact_ids.len(),
        superseded_chunks: chunk_ids.len(),
    })
}

/// The evidence documents changed (one deleted, or a deletion undone): recompute these facts' attestations from the evidence still present.
///
/// The read path takes the attestation as the start date from which a fact without a start holds (`facts_holds_from`); the engine's timeline ordering uses the same
/// date (`temporal::DATED_AT`). After the earliest document is deleted, if the attestation does not move to the evidence that remains,
/// the predecessor reads up to the new anchor while it still reads from the old date, and two spans overlap by a year (#679 fourth review round).
///
/// Three kinds of rows, three origins (0064 decisions 3 and 5; 0022 revision 2026-10-03):
///
/// - **open statements**: the earliest of the remaining evidence, along with that date's name. Each piece of evidence records the moment its passage was
///   spoken (`fact_evidence.attested_at`, the section date written by time resolution); those without a record (this column
///   written before, whose document has not been re-parsed yet) are read by the document's own date, the same basis as the older `DATED_AT`.
///   If none of the remaining evidence carries a date, the attestation is empty — the deleted document's date no longer vouches for it;
/// - **typed rows that follow their statement** (materialized, rule-derived): the earliest attestation among the source statements,
///   the same sentence as at materialization (`materialize::sync_typed_attestation`). They copied the statement's evidence, so they are also in
///   this batch; the caller has already locked the timeline;
/// - **the rest** (written by people, or before this layering): the earliest document date among the remaining documents; if none of the remaining documents has a date,
///   the original value is kept — that date came from another source (written by a person)
async fn reattest_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    fact_ids: &[Uuid],
) -> AppResult<()> {
    if fact_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE facts f SET attested_from = w.at, attested_by = w.by
           FROM (SELECT DISTINCT ON (fe.fact_id) fe.fact_id,
                        COALESCE(fe.attested_at, own.at) AS at,
                        CASE WHEN fe.attested_at IS NOT NULL THEN fe.attested_by END AS by
                   FROM fact_evidence fe
                   JOIN documents d ON d.id = fe.document_id
              LEFT JOIN document_versions v ON v.document_id = fe.document_id
                                           AND v.version = fe.doc_version
                  CROSS JOIN LATERAL (
                        SELECT CASE WHEN d.doc_time_source IN ('content', 'source')
                                    THEN COALESCE(v.doc_time, d.doc_time) END AS at) own
                  WHERE fe.fact_id = ANY($1) AND d.deleted_at IS NULL
                  ORDER BY fe.fact_id, COALESCE(fe.attested_at, own.at) NULLS LAST, fe.chunk_id) w
          WHERE f.id = w.fact_id AND f.layer = 'open' AND f.invalidated_at IS NULL
            AND (f.attested_from IS DISTINCT FROM w.at OR f.attested_by IS DISTINCT FROM w.by)",
    )
    .bind(fact_ids)
    .execute(&mut **tx)
    .await?;
    crate::materialize::sync_typed_attestation(&mut **tx, kb_id, Some(fact_ids)).await?;
    sqlx::query(
        "UPDATE facts f SET attested_from = e.first
           FROM (SELECT fe.fact_id, min(d.doc_time) AS first
                   FROM fact_evidence fe
                   JOIN chunks c ON c.id = fe.chunk_id
                   JOIN documents d ON d.id = c.document_id
                  WHERE fe.fact_id = ANY($1) AND d.deleted_at IS NULL AND d.doc_time IS NOT NULL
                  GROUP BY fe.fact_id) e
          WHERE f.id = e.fact_id AND f.invalidated_at IS NULL AND f.layer <> 'open'
            AND NOT (f.implied
                     OR EXISTS (SELECT 1 FROM typed_fact_sources ts WHERE ts.fact_id = f.id))
            AND f.attested_from IS DISTINCT FROM e.first",
    )
    .bind(fact_ids)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Attestation recompute scheduled by delete or undelete (RC-010): the fact ids and that task.
#[derive(Debug)]
pub struct DeferredReattest {
    kb_id: Uuid,
    fact_ids: Vec<Uuid>,
    job_id: i64,
}

/// Attestation recompute does not run inside the delete transaction (RC-010): that transaction starts by locking the source row, and recomputing a document that references thousands of facts
/// takes tens of seconds, during which every push from the same source queues behind that lock. Here we only queue one `reattest_facts`,
/// committed together with the voiding — if the process exits after the commit, the task is still there
async fn defer_reattest_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    fact_ids: &[Uuid],
) -> AppResult<Option<DeferredReattest>> {
    if fact_ids.is_empty() {
        return Ok(None);
    }
    let job_id = crate::jobs::enqueue_with_max_attempts_tx(
        tx,
        "reattest_facts",
        serde_json::json!({ "kb_id": kb_id, "fact_ids": fact_ids }),
        3,
    )
    .await?;
    Ok(Some(DeferredReattest {
        kb_id,
        fact_ids: fact_ids.to_vec(),
        job_id,
    }))
}

/// After commit, with the source row released, run the queued recompute right away: when it returns, attestations and timelines have caught up, same
/// promise as before; then remove the task if it has not been claimed yet. If a worker claimed it first, that is fine too: the recompute only writes values that differ
async fn settle_reattest(pool: &PgPool, deferred: Option<DeferredReattest>) -> AppResult<()> {
    let Some(deferred) = deferred else {
        return Ok(());
    };
    reattest_facts(pool, deferred.kb_id, &deferred.fact_ids).await?;
    sqlx::query("DELETE FROM jobs WHERE id = $1 AND status = 'queued'")
        .bind(deferred.job_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Recomputes these facts' attestations from the evidence still present; their timelines are then put back in order (`reattest_facts` task, RC-010).
/// Runs in its own transaction: first lock the timelines in a fixed order (the same order as delete), without touching the source row
pub async fn reattest_facts(pool: &PgPool, kb_id: Uuid, fact_ids: &[Uuid]) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    let timelines = crate::temporal::timelines_of(&mut *tx, kb_id, fact_ids, None).await?;
    crate::temporal::lock_timelines(&mut tx, kb_id, &timelines).await?;
    reattest_tx(&mut tx, kb_id, fact_ids).await?;
    crate::temporal::tidy_timelines_tx(&mut tx, kb_id, &timelines).await?;
    tx.commit().await?;
    Ok(())
}

/// Locks the existing facts that reference this document, and their uniqueness timelines, then returns.
///
/// **Read again after locking** (#679 third review round): while waiting for the lock, a timeline recompute may have rewritten one row into
/// a new row; the first read got the old id. Voiding with the old list would leave the new row alive while its sources were already deleted.
/// Rewrites only happen under the timeline locks, so the list does not change once locked. If a row read again lands on a timeline that is not locked
/// (a retraction merge sent it back to its source entity), lock those as well
///
/// `also`: other facts to lock together (the ones to revive on undelete). **Lock everything in one pass**: lock the referencing ones first,
/// then the ones to revive; taking the locks in two rounds would let another transaction that takes the same two locks in order deadlock with it
async fn lock_cited_timelines(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    document_id: Uuid,
    also: &[Uuid],
) -> AppResult<(Vec<Uuid>, Vec<crate::temporal::Timeline>)> {
    let cited_sql = "SELECT DISTINCT f.id FROM facts f
                       JOIN fact_evidence fe ON fe.fact_id = f.id
                       JOIN chunks c ON c.id = fe.chunk_id
                      WHERE f.kb_id = $1 AND f.invalidated_at IS NULL AND c.document_id = $2";
    let cited: Vec<Uuid> = sqlx::query_scalar(cited_sql)
        .bind(kb_id)
        .bind(document_id)
        .fetch_all(&mut **tx)
        .await?;
    let first: Vec<Uuid> = cited.iter().chain(also).copied().collect();
    let mut timelines = crate::temporal::timelines_of(&mut **tx, kb_id, &first, None).await?;
    crate::temporal::lock_timelines(tx, kb_id, &timelines).await?;
    let cited: Vec<Uuid> = sqlx::query_scalar(cited_sql)
        .bind(kb_id)
        .bind(document_id)
        .fetch_all(&mut **tx)
        .await?;
    let late: Vec<_> = crate::temporal::timelines_of(&mut **tx, kb_id, &cited, None)
        .await?
        .into_iter()
        .filter(|t| !timelines.contains(t))
        .collect();
    if !late.is_empty() {
        crate::temporal::lock_timelines(tx, kb_id, &late).await?;
        timelines.extend(late);
    }
    Ok((cited, timelines))
}

/// Undoes a delete: the document, the chunks tagged by this delete, and the facts voided by it are revived in place, same shape as `revert_merge`.
///
/// Only revives what is on the `document_deletions` list (including the list from the last shared-source deletion) —
/// older chunk versions, and facts voided before the delete,
/// are not on the list. All three paths go through here: a person clicks undo, a sync hits a tombstone, or the same content is re-uploaded
pub async fn restore(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<Document> {
    let mut tx = pool.begin().await?;
    let (document, reattest) = restore_tx(&mut tx, kb_id, id).await?;
    tx.commit().await?;
    settle_reattest(pool, reattest).await?;
    Ok(document)
}

async fn restore_tx(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    id: Uuid,
) -> AppResult<(Document, Option<DeferredReattest>)> {
    lock_document_source_tx(tx, id).await?;
    // The document and the facts voided by this delete share the same transaction time; remember it before clearing the tombstone.
    let deleted_at = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT deleted_at FROM documents WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .flatten();
    let row: Option<(Uuid, Vec<Uuid>, Vec<Uuid>)> = sqlx::query_as(
        "SELECT id, invalidated_facts, superseded_chunks FROM document_deletions
          WHERE document_id = $1 AND kb_id = $2 AND reverted_at IS NULL
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(id)
    .bind(kb_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((deletion_id, fact_ids, chunk_ids)) = row else {
        return Err(AppError::Conflict("Document is not deleted".into()));
    };
    // Once restored, it no longer counts as evicted by decay (RC-003)
    sqlx::query("DELETE FROM decay_evictions WHERE document_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    // Purged items have nothing to revive: the chunks and the original are both gone
    let purged: bool =
        sqlx::query_scalar("SELECT purged_at IS NOT NULL FROM documents WHERE id = $1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    if purged {
        return Err(AppError::Conflict(
            "The document's content was purged; there is nothing to restore".into(),
        ));
    }
    let hit = sqlx::query(
        "UPDATE documents SET deleted_at = NULL, updated_at = now()
          WHERE id = $1 AND kb_id = $2 AND deleted_at IS NOT NULL",
    )
    .bind(id)
    .bind(kb_id)
    .execute(&mut **tx)
    .await?;
    if hit.rows_affected() == 0 {
        return Err(AppError::Conflict("Document is not deleted".into()));
    }
    sqlx::query("UPDATE chunks SET superseded_at = NULL WHERE id = ANY($1)")
        .bind(&chunk_ids)
        .execute(&mut **tx)
        .await?;
    // When A and B both attest a fact, only B, deleted last, records the fact in the voided list. Restoring A should also bring it back,
    // but only trusts the voided time left by the delete transaction; facts retracted separately later cannot be revived from the old list.
    let shared: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT DISTINCT f.id, f.invalidated_at FROM facts f
           JOIN fact_evidence fe ON fe.fact_id = f.id
           JOIN chunks c ON c.id = fe.chunk_id
          WHERE f.kb_id = $1 AND c.document_id = $2 AND f.invalidated_at IS NOT NULL
            AND EXISTS (SELECT 1 FROM document_deletions dd
                         JOIN documents d ON d.id = dd.document_id
                        WHERE dd.kb_id = $1 AND dd.reverted_at IS NULL
                          AND f.id = ANY(dd.invalidated_facts)
                          AND f.invalidated_at = d.deleted_at)",
    )
    .bind(kb_id)
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let (shared_ids, shared_stamps): (Vec<_>, Vec<_>) = shared.into_iter().unzip();
    let restored: Vec<Uuid> = fact_ids.iter().chain(&shared_ids).copied().collect();
    // Revived facts return to their own timelines; facts that reference this document still have their ordering dates restored.
    // Lock first, then revive, then recompute (same as delete); if a fact is voided again while waiting for the lock, do not overwrite the new decision.
    let (cited, _timelines) = lock_cited_timelines(tx, kb_id, id, &restored).await?;
    sqlx::query(
        "UPDATE facts SET invalidated_at = NULL
          WHERE invalidated_at IS NOT NULL
            AND ((id = ANY($1) AND invalidated_at = $4) OR (id, invalidated_at) IN
                 (SELECT * FROM unnest($2::uuid[], $3::timestamptz[])))",
    )
    .bind(&fact_ids)
    .bind(&shared_ids)
    .bind(&shared_stamps)
    .bind(deleted_at)
    .execute(&mut **tx)
    .await?;
    let touched: Vec<Uuid> = cited.iter().chain(&restored).copied().collect();
    let reattest = defer_reattest_tx(tx, kb_id, &touched).await?;
    sqlx::query("UPDATE document_deletions SET reverted_at = now() WHERE id = $1")
        .bind(deletion_id)
        .execute(&mut **tx)
        .await?;
    let document = sqlx::query_as("SELECT * FROM documents WHERE id = $1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    Ok((document, reattest))
}

/// The output of one real delete. `blobs` are the original-content fingerprints owned exclusively by this document (including its history versions); those still referenced elsewhere
/// are not on the list. BlobStore is server-side, so file deletion is done by the caller
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    pub blobs: Vec<String>,
    pub chunks: u64,
}

async fn lock_document_source_tx(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> AppResult<()> {
    // Match RSS ordering: source, then activation/entry/document, never reverse it.
    sqlx::query("SELECT id FROM sources WHERE id = (SELECT source_id FROM documents WHERE id = $1) FOR UPDATE")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Real delete (#268, second half): actually wipes the content, **only open for documents already deleted, not reversible**.
///
/// Deleting is an event and reversible ([`delete`] / [`restore`]); this is a different action: chunk rows are removed
/// (evidence quotes cascade with the foreign key), version rows are removed, and the document row is kept as a tombstone with `purged_at` set. `external_key`
/// is cleared — the identity is released, so if the sync source still has this document, the next sync ingests it again as a new document; the same content
/// can also be uploaded again (the unique index only covers rows not purged). Facts stay voided, and the `document_deletions` entry stays on record:
/// "this document once existed, was deleted at some point, and purged at another"
pub async fn purge(pool: &PgPool, kb_id: Uuid, id: Uuid) -> AppResult<PurgeReport> {
    let mut tx = pool.begin().await?;
    lock_document_source_tx(&mut tx, id).await?;
    type Stamp = Option<chrono::DateTime<chrono::Utc>>;
    let row: Option<(String, Stamp, Stamp)> = sqlx::query_as(
        "SELECT sha256, deleted_at, purged_at FROM documents
          WHERE id = $1 AND kb_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(kb_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((sha, deleted_at, purged_at)) = row else {
        return Err(AppError::NotFound);
    };
    if purged_at.is_some() {
        return Err(AppError::Conflict("The document was already purged".into()));
    }
    if deleted_at.is_none() {
        return Err(AppError::Conflict(
            "Delete the document before purging it".into(),
        ));
    }
    let mut shas: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT sha256 FROM document_versions WHERE document_id = $1")
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    if !shas.contains(&sha) {
        shas.push(sha);
    }
    let chunks = sqlx::query("DELETE FROM chunks WHERE document_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM document_versions WHERE document_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE documents
            SET purged_at = clock_timestamp(), external_key = NULL, text_len = 0, chunk_count = 0,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    // Original content is addressed by content, and other documents (in any store) may share the same one: only hand back the ones no one references anymore.
    //
    // **Decide all fingerprints in one query, and it must be at this position.** The check looks at the state after the version rows are deleted: this document's old versions
    // no longer count as references, so the original content it owns exclusively can be reclaimed. Moving this check
    // before the delete would make every original look referenced, never handed back, with no error at all (#516).
    // Previously each fingerprint was queried once; a document re-parsed thirty times was thirty round trips, all inside the transaction already holding
    // the source lock and the document row lock. Now it is one round trip, and both checks use the sha index from 0043.
    // The list order follows shas: current fingerprints come after the history, as before.
    let blobs: Vec<String> = sqlx::query_scalar(
        "SELECT s FROM unnest($1::text[]) WITH ORDINALITY AS u(s, n)
          WHERE NOT EXISTS (SELECT 1 FROM documents WHERE sha256 = s AND purged_at IS NULL)
            AND NOT EXISTS (SELECT 1 FROM document_versions WHERE sha256 = s)
          ORDER BY n",
    )
    .bind(&shas)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(PurgeReport { blobs, chunks })
}

/// Rebuilds document chunks (inside a transaction, idempotent). Returns (chunk_id, text) for the full-text index.
///
/// Claim-style incremental update: each new chunk is first matched by text against the current chunks — on a match it "claims" the existing row (which only updates
/// seq/offset/version, its identity continues: embedding, extracted_at and evidence links are all refreshed in place,
/// and unchanged paragraphs are neither re-extracted nor wrongly marked "evidence stale"); only unmatched chunks are newly created. Losing old chunks are soft-deleted
/// (superseded_at set) rather than physically removed — fact_evidence references stay intact and old versions can still be replayed,
/// and the embedding is cleared (old versions are not used in retrieval; vectors are the bulk of storage and are not kept).
pub async fn replace_chunks(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
    pieces: &[ChunkPiece],
) -> AppResult<Vec<(String, String)>> {
    Ok(
        replace_chunks_for_snapshot(pool, kb_id, document_id, pieces, None)
            .await?
            .unwrap_or_default(),
    )
}

/// A slow read must not overwrite newer-version chunks, nor refill a deleted document.
/// Returns None when the input is stale; the caller should end this processing run.
pub async fn replace_chunks_if_current(
    pool: &PgPool,
    input: &ProcessingInput,
    pieces: &[ChunkPiece],
) -> AppResult<Option<Vec<(String, String)>>> {
    replace_chunks_for_snapshot(
        pool,
        input.document.kb_id,
        input.document.id,
        pieces,
        Some(input),
    )
    .await
}

async fn replace_chunks_for_snapshot(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
    pieces: &[ChunkPiece],
    input: Option<&ProcessingInput>,
) -> AppResult<Option<Vec<(String, String)>>> {
    let mut tx = pool.begin().await?;
    // Two tasks may process the same document at once. Lock the parent row first, even when there are no chunks yet,
    // so the second task can claim rows the first one wrote, avoiding duplicate inserts.
    //
    // **`FOR NO KEY UPDATE`, not `FOR UPDATE`**: the latter conflicts with the `FOR KEY SHARE` that foreign key checks need,
    // so while this transaction is alive, every child-table insert for this document is blocked (chunks,
    // document_versions, memory::append), and this transaction makes one round trip per chunk: a document with four thousand chunks
    // would hold the lock for four seconds. Mutual exclusion against another `replace_chunks` is the same either way
    if let Some(input) = input {
        if !lock_processing_input_tx(&mut tx, input).await? {
            return Ok(None);
        }
    } else {
        sqlx::query("SELECT id FROM documents WHERE id = $1 FOR NO KEY UPDATE")
            .bind(document_id)
            .execute(&mut *tx)
            .await?;
    }
    let (version,): (i32,) = sqlx::query_as(
        "SELECT COALESCE(MAX(version), 1) FROM document_versions WHERE document_id = $1",
    )
    .bind(document_id)
    .fetch_one(&mut *tx)
    .await?;

    // Claim pool: current chunks grouped by text (identical duplicate chunks are paired as a multiset, each claiming its own)
    // Claims are keyed by "text + source": the same sentence read from the original and recognized on a scanned page is not the same chunk (0040)
    type Stored = (
        Uuid,
        String,
        String,
        Option<String>,
        Option<serde_json::Value>,
    );
    let old: Vec<Stored> = sqlx::query_as(
        "SELECT id, text, origin, origin_model, anchor FROM chunks
              WHERE document_id = $1 AND superseded_at IS NULL",
    )
    .bind(document_id)
    .fetch_all(&mut *tx)
    .await?;
    type ClaimKey = (String, String, Option<String>, Option<String>);
    let key_of = |text: &str,
                  origin: &str,
                  model: Option<&str>,
                  anchor: Option<&serde_json::Value>|
     -> ClaimKey {
        (
            text.to_string(),
            origin.to_string(),
            model.map(str::to_string),
            anchor.map(|a| a.to_string()),
        )
    };
    let mut claim_pool: std::collections::HashMap<ClaimKey, Vec<Uuid>> =
        std::collections::HashMap::new();
    for (id, text, origin, model, anchor) in old {
        claim_pool
            .entry(key_of(&text, &origin, model.as_deref(), anchor.as_ref()))
            .or_default()
            .push(id);
    }

    // First phase: claims (in-place row updates) and the list of inserts to do. New chunks must wait until soft-deletes finish before being inserted,
    // otherwise the "losing" check would soft-delete the new chunks inserted in this round too
    let mut adopted: Vec<Uuid> = Vec::new();
    let mut to_insert: Vec<(Uuid, &ChunkPiece)> = Vec::new();
    let mut out = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let p = &piece.provenance;
        let claimed = claim_pool
            .get_mut(&key_of(
                &piece.text,
                p.origin.as_str(),
                p.model.as_deref(),
                p.anchor.as_ref(),
            ))
            .and_then(|ids| ids.pop());
        if let Some(id) = claimed {
            sqlx::query(
                "UPDATE chunks SET seq = $2, char_start = $3, char_end = $4, doc_version = $5,
                        heading = $6
                 WHERE id = $1",
            )
            .bind(id)
            .bind(piece.seq)
            .bind(piece.char_start)
            .bind(piece.char_end)
            .bind(version)
            .bind(&piece.heading)
            .execute(&mut *tx)
            .await?;
            adopted.push(id);
            out.push((id.to_string(), piece.text.clone()));
        } else {
            let id = Uuid::now_v7();
            to_insert.push((id, piece));
            out.push((id.to_string(), piece.text.clone()));
        }
    }

    // Second phase: losing old chunks (no identical text in the new version) → soft-delete.
    //
    // **Vectors are kept** (0019 open question 2). Previously `embedding = NULL` was set here to save storage,
    // but what that saved was the smaller of the two halves: the chunk **text** is kept anyway (version replay needs it),
    // and a 1024-dimension vector is only a few KB. The cost was that the whole history became unsearchable: the record axis can go back in time,
    // but retrieval only knew the present, so as-of retrieval came back empty from the start.
    //
    // That is the default. In a large store that re-pushes everything on every sync, 3072-dimension vectors are the bigger half; deployments can set
    // `UTOPIA_SUPERSEDED_VECTOR_DAYS` so that `retention::prune_superseded` clears each day the vectors of superseded chunks
    // older than that many days; rows and text are kept as before (0019 revision).
    sqlx::query(
        "UPDATE chunks SET superseded_at = now()
         WHERE document_id = $1 AND superseded_at IS NULL AND NOT (id = ANY($2))",
    )
    .bind(document_id)
    .bind(&adopted)
    .execute(&mut *tx)
    .await?;

    // Third phase: insert the new chunks
    for (id, piece) in to_insert {
        sqlx::query(
            "INSERT INTO chunks
                (id, kb_id, document_id, seq, text, char_start, char_end, doc_version, heading,
                 origin, origin_model, anchor)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(id)
        .bind(kb_id)
        .bind(document_id)
        .bind(piece.seq)
        .bind(&piece.text)
        .bind(piece.char_start)
        .bind(piece.char_end)
        .bind(version)
        .bind(&piece.heading)
        .bind(piece.provenance.origin.as_str())
        .bind(&piece.provenance.model)
        .bind(&piece.provenance.anchor)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Some(out))
}

/// Marks a chunk as extracted as soon as its extraction finishes (claimed chunks carry the marker and skip re-extraction; also lets an interrupted extraction resume).
pub async fn mark_chunk_extracted(pool: &PgPool, chunk_id: Uuid) -> AppResult<()> {
    sqlx::query("UPDATE chunks SET extracted_at = now() WHERE id = $1")
        .bind(chunk_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Single-document re-extraction queue (manual Extract = full forced re-run): clears the incremental markers, retires any running task, sets
/// queued, and creates an extraction task — same semantics as `queue_extraction`, but for one document. Returns the job id.
///
/// The whole thing is one transaction. If the steps were separate, a failure at any step would leave a half state: markers cleared but the epoch not changed,
/// so the old task never notices it has been replaced; or queued set but no task created, so the document is never picked up.
pub async fn queue_extraction_one(pool: &PgPool, document_id: Uuid) -> AppResult<i64> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE chunks SET extracted_at = NULL
         WHERE document_id = $1 AND superseded_at IS NULL",
    )
    .bind(document_id)
    .execute(&mut *tx)
    .await?;
    // Delete old tasks that never started: clicking Extract twice should not pile up two tasks extracting the same document
    sqlx::query(
        "DELETE FROM jobs WHERE kind = 'extract_document' AND status = 'queued'
           AND payload->>'document_id' = $1",
    )
    .bind(document_id.to_string())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE documents SET graph_status = 'queued', graph_error = NULL,
                extract_epoch = extract_epoch + 1
         WHERE id = $1",
    )
    .bind(document_id)
    .execute(&mut *tx)
    .await?;
    let (job_id,): (i64,) = sqlx::query_as(
        "INSERT INTO jobs (kind, payload)
         VALUES ('extract_document', jsonb_build_object('document_id', $1::text))
         RETURNING id",
    )
    .bind(document_id)
    .fetch_one(&mut *tx)
    .await?;
    crate::jobs::notify_worker_tx(&mut tx).await?;
    tx.commit().await?;
    Ok(job_id)
}

/// Document viewer: all chunks, in order.
pub async fn chunks_full(
    pool: &PgPool,
    document_id: Uuid,
) -> AppResult<Vec<utopia_core::models::ChunkFull>> {
    let rows = sqlx::query_as(
        "SELECT id, seq, text, origin, origin_model, anchor FROM chunks
         WHERE document_id = $1 AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Chunks for extraction: text plus the embedding already computed during ingestion (reused in v2 extraction, zero extra embed calls).
#[derive(Debug, sqlx::FromRow)]
pub struct ChunkForExtract {
    pub id: Uuid,
    pub seq: i32,
    pub text: String,
    pub embedding: Option<Vector>,
    /// Where this text came from (0040): facts described from a picture cannot rewrite the ledger alone
    pub origin: String,
}

pub async fn chunks_for_extraction(
    pool: &PgPool,
    document_id: Uuid,
) -> AppResult<Vec<ChunkForExtract>> {
    // Only fetch unextracted chunks: claimed unchanged paragraphs carry extracted_at and are skipped (incremental extraction + resumable extraction)
    let rows = sqlx::query_as(
        "SELECT id, seq, text, embedding, origin FROM chunks
         WHERE document_id = $1 AND superseded_at IS NULL AND extracted_at IS NULL
         ORDER BY seq",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The **start** of the file: the existing chunk with the smallest seq, whether or not it has been extracted.
///
/// Cannot substitute the first result of [`chunks_for_extraction`]: that is only the first chunk not yet extracted — when a changed file is
/// re-extracted, chunk 7 would get chunk 3, and on retry after a failure the following chunks would get the failed one
pub async fn opening_chunk(pool: &PgPool, document_id: Uuid) -> AppResult<Option<(Uuid, String)>> {
    let row = sqlx::query_as(
        "SELECT id, text FROM chunks
         WHERE document_id = $1 AND superseded_at IS NULL
         ORDER BY seq LIMIT 1",
    )
    .bind(document_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Advances extraction state (also clears the previous run's failure reason — a re-run starts fresh).
pub async fn set_graph_status(pool: &PgPool, id: Uuid, status: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET graph_status = $2, graph_error = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Chunks of this document that do not yet have an embedding (id + text).
pub async fn chunks_pending_embedding(
    pool: &PgPool,
    document_id: Uuid,
) -> AppResult<Vec<(Uuid, String)>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, text FROM chunks
         WHERE document_id = $1 AND embedding IS NULL AND superseded_at IS NULL ORDER BY seq",
    )
    .bind(document_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn set_embeddings(pool: &PgPool, items: &[(Uuid, Vec<f32>)]) -> AppResult<()> {
    set_embeddings_for_input(pool, items, None).await?;
    Ok(())
}

pub async fn set_embeddings_if_current(
    pool: &PgPool,
    input: &ProcessingInput,
    items: &[(Uuid, Vec<f32>)],
) -> AppResult<bool> {
    set_embeddings_for_input(pool, items, Some(input)).await
}

async fn set_embeddings_for_input(
    pool: &PgPool,
    items: &[(Uuid, Vec<f32>)],
    input: Option<&ProcessingInput>,
) -> AppResult<bool> {
    let mut tx = pool.begin().await?;
    if let Some(input) = input {
        if !lock_processing_input_tx(&mut tx, input).await? {
            return Ok(false);
        }
    }
    for (id, emb) in items {
        sqlx::query("UPDATE chunks SET embedding = $2 WHERE id = $1")
            .bind(id)
            .bind(Vector::from(emb.clone()))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    // The first time a vector of this dimension is written, the index should be added (0035). If it already exists, this statement is just a lookup
    if let Some((_, first)) = items.first() {
        crate::vector_index::request(pool, crate::vector_index::Target::Chunks, first.len())
            .await?;
    }
    Ok(true)
}

/// Vector nearest-neighbour search (cosine distance). The dimension is written as a literal, both sides cast, `relaxed_order` — three
/// rules, see `vector_index`; whether to use the index is decided by the planner.
///
/// The order is re-sorted in an outer step (`vector_index::RESORT`): under `relaxed_order` the order from the index
/// is only approximately by distance, and when distances tie, the exact path and HNSW each sort on their own — the same question on two plans can return a different first
/// place (#652). Ties are broken by id
///
/// `documents`: only search chunks of these documents. The condition goes inside the nearest-neighbour CTE, since `LIMIT` counts only
/// allowed chunks; outside it, other chunks in the store would fill up the quota first
pub async fn vector_search(
    pool: &PgPool,
    kb_id: Uuid,
    embedding: &[f32],
    limit: i64,
    as_of: Option<DateTime<Utc>>,
    documents: Option<&[Uuid]>,
) -> AppResult<Vec<Uuid>> {
    let dims = embedding.len();
    if dims == 0 {
        return Ok(Vec::new());
    }
    let query_vec = Vector::from(embedding.to_vec());
    let mut tx = pool.begin().await?;
    crate::vector_index::relaxed_order(pool, &mut tx).await?;
    let rows: Vec<(Uuid,)> = sqlx::query_as(&format!(
        "WITH nearest AS MATERIALIZED (
             SELECT id, {distance} AS distance FROM chunks c
             WHERE c.kb_id = $1 AND c.embedding IS NOT NULL AND {live}
               AND {same_dims}{only}
             ORDER BY {order}
             LIMIT $3
         )
         SELECT id FROM nearest ORDER BY {resort}",
        live = crate::record_axis::chunk_live_at("c", as_of.map(|_| 4)),
        only = if documents.is_some() {
            " AND c.document_id = ANY($5)"
        } else {
            ""
        },
        same_dims = crate::vector_index::same_dims("c.embedding", dims),
        distance = crate::vector_index::distance("c.embedding", 2, dims),
        order = crate::vector_index::index_order("c.embedding", 2, dims),
        resort = crate::vector_index::RESORT,
    ))
    .bind(kb_id)
    .bind(&query_vec)
    .bind(limit)
    .bind(as_of)
    .bind(documents)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Documents allowed when retrieval filters by source: the source's documents that were alive in the store at that time; if `key_prefix` is given, only those whose
/// `external_key` starts with it. The comparison uses `starts_with`: `_` and `%` in a path become wildcards once they go into LIKE
pub async fn ids_in_source(
    pool: &PgPool,
    kb_id: Uuid,
    source_id: Uuid,
    key_prefix: Option<&str>,
    as_of: Option<DateTime<Utc>>,
) -> AppResult<Vec<Uuid>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(&format!(
        "SELECT d.id FROM documents d
         WHERE d.kb_id = $1 AND d.source_id = $2 AND {doc_live}
           AND ($3::text IS NULL OR starts_with(d.external_key, $3))",
        doc_live = crate::record_axis::document_live_at("d", as_of.map(|_| 4)),
    ))
    .bind(kb_id)
    .bind(source_id)
    .bind(key_prefix)
    .bind(as_of)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Fetches chunks by a set of ids (with document name and source), keeping the input order.
///
/// `as_of`: record axis (0019) — chunks alive at that moment, documents not yet deleted. A document's deletion
/// is likewise an event on the record axis (#268 left a tombstone), so both conditions go back in time together.
pub async fn chunks_by_ids(
    pool: &PgPool,
    kb_id: Uuid,
    ids: &[Uuid],
    as_of: Option<DateTime<Utc>>,
) -> AppResult<Vec<SourcedChunk>> {
    let rows: Vec<SourcedChunk> = sqlx::query_as(&format!(
        "SELECT c.id, c.document_id, c.seq, c.text, d.filename, d.source_id,
                s.kind AS source_kind, d.external_key
         FROM chunks c JOIN documents d ON d.id = c.document_id
         LEFT JOIN sources s ON s.id = d.source_id
         WHERE c.kb_id = $1 AND c.id = ANY($2)
           AND {live} AND {doc_live}",
        live = crate::record_axis::chunk_live_at("c", as_of.map(|_| 3)),
        doc_live = crate::record_axis::document_live_at("d", as_of.map(|_| 3)),
    ))
    .bind(kb_id)
    .bind(ids)
    .bind(as_of)
    .fetch_all(pool)
    .await?;
    // Restore the RRF ranking order
    let mut by_id: std::collections::HashMap<Uuid, SourcedChunk> =
        rows.into_iter().map(|c| (c.chunk.id, c)).collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// A document's current chunks (with document name), ordered by seq. kb_id goes into the WHERE clause rather than being checked after fetching.
pub async fn chunks_in_document(
    pool: &PgPool,
    kb_id: Uuid,
    document_id: Uuid,
) -> AppResult<Vec<ChunkView>> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.document_id, c.seq, c.text, d.filename
         FROM chunks c JOIN documents d ON d.id = c.document_id
         WHERE c.kb_id = $1 AND c.document_id = $2 AND c.superseded_at IS NULL
         ORDER BY c.seq",
    )
    .bind(kb_id)
    .bind(document_id)
    .fetch_all(pool)
    .await?)
}

/// Batch-queues a full re-extraction: ready documents get their incremental markers cleared → graph_status=queued → extraction task created,
/// returns the ids of documents to extract. If `source_id` is given, limit to that source; otherwise the whole store.
///
/// Documents currently being extracted are requeued too — bumping the epoch "retires" the running task (see
/// `extract_epoch`), so there is no need to skip them, and two workers never extract the same document.
/// Extract tasks that have not started yet are deleted too (payload compared as text: a historical dirty payload cannot be converted to a uuid).
///
/// Creating the task and setting the status happen in one transaction: if done separately, a mid-way error would leave a batch of documents with graph_status=queued
/// and no task — no worker will pick them up, and the UI would sit on "queued" forever.
pub async fn queue_extraction(
    pool: &PgPool,
    kb_id: Uuid,
    source_id: Option<Uuid>,
) -> AppResult<Vec<Uuid>> {
    let mut tx = pool.begin().await?;
    // Documents whose source says not to extract are not queued (the SQL version of `Source::extracts`; both check the same key).
    // This check is enforced in the store rather than at each entry point: full rebuild (`rebuild`), per-source re-extraction, and any future
    // entry point all go through here; schema documents never enter the graph (0035 decision 7, #553)
    let ids: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT d.id FROM documents d
         LEFT JOIN sources s ON s.id = d.source_id
         WHERE d.kb_id = $1 AND d.deleted_at IS NULL AND d.status = 'ready'
           AND ($2::uuid IS NULL OR d.source_id = $2)
           AND NOT coalesce(s.config -> 'extract' = 'false'::jsonb, false)
         ORDER BY d.created_at",
    )
    .bind(kb_id)
    .bind(source_id)
    .fetch_all(&mut *tx)
    .await?;
    let ids: Vec<Uuid> = ids.into_iter().map(|(id,)| id).collect();
    if ids.is_empty() {
        tx.commit().await?;
        return Ok(ids);
    }

    sqlx::query(
        "DELETE FROM jobs WHERE kind = 'extract_document' AND status = 'queued'
           AND payload->>'document_id' = ANY($1)",
    )
    .bind(ids.iter().map(|i| i.to_string()).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE chunks SET extracted_at = NULL
         WHERE document_id = ANY($1) AND superseded_at IS NULL",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE documents SET graph_status = 'queued', graph_error = NULL,
                extract_epoch = extract_epoch + 1
         WHERE id = ANY($1)",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    // The payload shape matches jobs::enqueue(json!({"document_id": id})): uuid is serialized as a string
    sqlx::query(
        "INSERT INTO jobs (kind, payload)
         SELECT 'extract_document', jsonb_build_object('document_id', id::text)
         FROM unnest($1::uuid[]) AS t(id)",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    crate::jobs::notify_worker_tx(&mut tx).await?;
    tx.commit().await?;
    Ok(ids)
}

/// Extraction failure: status and reason are written to the store together, so the UI has something to show.
pub async fn set_graph_failed(pool: &PgPool, id: Uuid, error: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE documents SET graph_status = 'failed', graph_error = $2, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Ownership token for an extraction task: incremented on each "start a new extraction round".
///
/// Claiming by graph_status alone is not enough: when an old task reads back, the new task that took over may already have written the status back to
/// extracting, so the old task would wrongly think it is still in charge. The epoch increases monotonically, so an old task sees at once that it has been taken over.
pub async fn extract_epoch(pool: &PgPool, id: Uuid) -> AppResult<i32> {
    let (epoch,): (i32,) = sqlx::query_as("SELECT extract_epoch FROM documents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(epoch)
}

/// All chunks in the store, grouped by document, for rebuilding the retrieval index.
///
/// **Fetches everything at once**: this runs only when startup finds the index empty, and then everything is needed;
/// after it runs, it never runs again.
pub async fn all_chunks_for_index(pool: &PgPool) -> AppResult<Vec<(Uuid, Uuid, Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT kb_id, document_id, id, text FROM chunks
          WHERE superseded_at IS NULL
          ORDER BY document_id, seq",
    )
    .fetch_all(pool)
    .await?)
}

/// How many live chunks the store holds in total. At startup this is checked against the index.
pub async fn live_chunk_count(pool: &PgPool) -> AppResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM chunks WHERE superseded_at IS NULL")
            .fetch_one(pool)
            .await?,
    )
}
