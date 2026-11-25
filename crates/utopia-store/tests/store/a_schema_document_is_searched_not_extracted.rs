//! A document whose source says not to extract is not queued by `queue_extraction` -- tested against a real database (0035 decision 7, #553).
//!
//! When a data source is attached, its schema is ingested as a markdown document, so question answering can retrieve the table structure. Previously it went through extraction like
//! any other document, and the extractor treated every column name as an entity: on a wide-table corpus, twenty-eight of forty concept entities
//! were column names. "Retrieve, do not learn" is recorded on the source's config, and this filter lives in the database
//! rather than in each entry point -- full rebuilds, per-source re-extraction, and any future entry point all go through this one query.

use sqlx::PgPool;
use uuid::Uuid;

async fn base(pool: &PgPool) -> anyhow::Result<Uuid> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'skip-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'skip-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'skip-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    Ok(kb)
}

async fn ready_document(pool: &PgPool, kb: Uuid, source: Uuid, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, $4, $5, 'ready')",
    )
    .bind(id)
    .bind(kb)
    .bind(source)
    .bind(name)
    .bind(format!("sha-{id}"))
    .execute(pool)
    .await?;
    Ok(id)
}

#[tokio::test]
async fn a_source_that_does_not_extract_keeps_its_documents_out_of_the_queue() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let kb = base(&pool).await?;

    let run = async {
        // Identical to the one `sync_schema_doc` creates: a folder, with the config stating outright not to extract
        let schemas = utopia_store::sources::create(
            &pool,
            kb,
            "folder",
            "Data schemas",
            &serde_json::json!({ "extract": false }),
            Some("database"),
            None,
            None,
        )
        .await?;
        // Old source: the config is `{}`, so it is still extracted as before
        let uploads = utopia_store::sources::create(
            &pool,
            kb,
            "folder",
            "Uploads",
            &serde_json::json!({}),
            None,
            None,
            None,
        )
        .await?;
        assert!(!schemas.extracts() && uploads.extracts());

        let schema_doc = ready_document(&pool, kb, schemas.id, "wide-schema.md").await?;
        let prose_doc = ready_document(&pool, kb, uploads.id, "report.md").await?;

        // A full rebuild goes through exactly this path (source_id = None)
        let queued = utopia_store::documents::queue_extraction(&pool, kb, None).await?;
        assert_eq!(
            queued,
            vec![prose_doc],
            "only documents under a normal source should be enqueued"
        );
        assert!(!queued.contains(&schema_doc));

        // Re-extracting by source: the source itself says not to extract, so what gets queued is empty
        let queued = utopia_store::documents::queue_extraction(&pool, kb, Some(schemas.id)).await?;
        assert!(
            queued.is_empty(),
            "a non-extracted source re-extracted by source should also be empty"
        );

        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Only the knowledge base is deleted, not the org -- users are soft-deleted, and the test should not invent an action that does not exist in the product
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}
