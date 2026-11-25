//! An upload has no date (0045 decision 3, #714).
//!
//! The moment of upload or sync is the recording time, not the document's date: a document from which no date was read has `doc_time` empty,
//! source 'none', and `dated_at` None, for every writer. A date read from the body is stored as
//! 'content'; one the source system gives is stored as 'source'; only these two count as the document's date. Old values
//! 'upload_time' / 'file_mtime' read as no date. Time context goes in and comes out unchanged.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use chrono::{TimeZone, Utc};
use sqlx::PgPool;
use utopia_store::documents;
use uuid::Uuid;

const ORG: &str = "upload-has-no-date-test";

#[tokio::test]
async fn version_repair_records_only_unversioned_retained_originals() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let mut tx = pool.begin().await?;
    // Temp copies retain the real migrated column types and checks. The repair
    // deliberately scans all documents; it must not touch concurrent fixtures.
    sqlx::raw_sql(
        "CREATE TEMP TABLE documents (LIKE public.documents INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE document_versions (LIKE public.document_versions INCLUDING ALL) ON COMMIT DROP;",
    )
    .execute(&mut *tx)
    .await?;
    let (current, recorded, purged, kb, version_id) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let written = Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap();
    sqlx::query(
        "INSERT INTO documents(id,kb_id,filename,sha256,size_bytes,doc_time,doc_time_source,purged_at)
         VALUES ($1,$4,'current.txt','current',7,$5,'content',NULL),
                ($2,$4,'recorded.txt','current-2',9,NULL,'none',NULL),
                ($3,$4,'purged.txt','purged',6,NULL,'none',now())",
    )
    .bind(current).bind(recorded).bind(purged).bind(kb).bind(written)
    .execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO document_versions(id,document_id,version,sha256,size_bytes,ingested_at)
         VALUES ($1,$2,3,'historical',4,'2020-01-01T00:00:00Z')",
    )
    .bind(version_id)
    .bind(recorded)
    .execute(&mut *tx)
    .await?;
    let migration =
        include_str!("../../../../migrations/0111_an_uploaded_original_has_a_version.sql");
    for _ in 0..2 {
        sqlx::raw_sql(migration).execute(&mut *tx).await?;
    }
    let current_version: (i32, String, i64, Option<chrono::DateTime<Utc>>, bool) = sqlx::query_as(
        "SELECT version,sha256,size_bytes,doc_time,ingested_at>=transaction_timestamp()
         FROM document_versions WHERE document_id=$1",
    )
    .bind(current)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(
        current_version,
        (1, "current".into(), 7, Some(written), true)
    );
    let versions: Vec<(Uuid, Uuid, i32, String)> = sqlx::query_as(
        "SELECT id,document_id,version,sha256 FROM document_versions WHERE document_id<>$1",
    )
    .bind(current)
    .fetch_all(&mut *tx)
    .await?;
    assert_eq!(
        versions,
        vec![(version_id, recorded, 3, "historical".into())]
    );
    tx.rollback().await?;
    Ok(())
}

async fn seed(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid)> {
    let (org, ws, kb, src) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(ORG)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, name) VALUES ($1, $2, '临时来源')")
        .bind(src)
        .bind(kb)
        .execute(pool)
        .await?;
    Ok((kb, src))
}

#[tokio::test]
async fn an_undated_document_has_no_date_from_any_writer() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    let (kb, src) = seed(&pool).await?;

    let run = async {
        // Uploaded, no date read from the body
        let doc =
            documents::create_from_upload(&pool, kb, "a.txt", "text/plain", 1, "sha-a", None, None)
                .await?;
        assert_eq!((doc.doc_time, doc.doc_time_source.as_str()), (None, "none"));
        assert!(doc.dated_at().is_none());
        assert!(doc.time_context.is_none() && doc.time_context_at.is_none());

        // Synced, no date given
        let synced = documents::create(
            &pool,
            kb,
            "b.txt",
            "text/plain",
            1,
            "sha-b",
            Some(src),
            None,
            Some("key-b"),
        )
        .await?;
        assert_eq!(
            (synced.doc_time, synced.doc_time_source.as_str()),
            (None, "none")
        );
        let queued = documents::create_with_version_and_processing(
            &pool,
            kb,
            "c.txt",
            "text/plain",
            1,
            "sha-c",
            Some(src),
            None,
            Some("key-c"),
        )
        .await?;
        assert_eq!(
            (queued.doc_time, queued.doc_time_source.as_str()),
            (None, "none")
        );
        let mut tx = pool.begin().await?;
        let upserted = documents::upsert_source_document_tx(
            &mut tx,
            kb,
            src,
            "key-d",
            "d.txt",
            "text/plain",
            1,
            "sha-d",
            None,
        )
        .await?;
        tx.commit().await?;
        assert_eq!(
            (upserted.doc_time, upserted.doc_time_source.as_str()),
            (None, "none")
        );

        // A date the source system gives keeps its source
        let published = Utc.with_ymd_and_hms(2011, 3, 4, 8, 0, 0).unwrap();
        let dated = documents::create(
            &pool,
            kb,
            "e.txt",
            "text/plain",
            1,
            "sha-e",
            Some(src),
            Some(published),
            Some("key-e"),
        )
        .await?;
        assert_eq!(
            (dated.doc_time, dated.doc_time_source.as_str()),
            (Some(published), "source")
        );
        assert_eq!(dated.dated_at(), Some(published));

        // A date read from the body
        let written = Utc.with_ymd_and_hms(2024, 2, 29, 0, 0, 0).unwrap();
        documents::set_content_date(&pool, doc.id, written).await?;
        let doc = documents::get(&pool, doc.id).await?;
        assert_eq!(
            (doc.dated_at(), doc.doc_time_source.as_str()),
            (Some(written), "content")
        );

        // Time context goes in and comes out unchanged
        let context = serde_json::json!({
            "date": "2024-02-29",
            "periods": [{ "name": "fiscal 2024", "from": "2023-01-29", "to": "2024-01-28" }],
            "anchors": [{ "mention": "the acquisition", "date": "2011-03-04" }]
        });
        documents::set_time_context(&pool, doc.id, &context).await?;
        let doc = documents::get(&pool, doc.id).await?;
        assert_eq!(doc.time_context, Some(context));
        assert!(doc.time_context_at.is_some());

        // Old values read as no date
        for old in ["upload_time", "file_mtime"] {
            let mut d = doc.clone();
            d.doc_time = Some(Utc::now());
            d.doc_time_source = old.into();
            assert!(
                d.dated_at().is_none(),
                "{old} is recorded time, not a document date"
            );
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = $1")
        .bind(ORG)
        .execute(&pool)
        .await?;
    run
}
