//! Extraction progress for the library: each document is counted once (#715).
//!
//! The progress bar used to take `ready` (ingestion complete) as the numerator and `ready + extracting` as the denominator. A document that is "ingested, graph still extracting"
//! was counted in both dimensions: a store with only one document being extracted showed 1 / 2.
//! Now the numerator and denominator both look only at `graph_status`: `done` and `extracting`. Deleted documents are not counted,
//! and failed extractions are counted separately.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use utopia_store::documents;
use uuid::Uuid;

async fn document(
    pool: &PgPool,
    kb: Uuid,
    name: &str,
    graph_status: &str,
    deleted: bool,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status, graph_status, deleted_at)
         VALUES ($1, $2, $3, $3, 'ready', $4, CASE WHEN $5 THEN now() END)",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(name)
    .bind(graph_status)
    .bind(deleted)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_document_still_extracting_is_not_counted_as_done() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'progress-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'progress-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'progress-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        document(&pool, kb, "done.md", "done", false).await?;
        document(&pool, kb, "extracting.md", "extracting", false).await?;
        document(&pool, kb, "queued.md", "queued", false).await?;
        document(&pool, kb, "failed.md", "failed", false).await?;
        // A deleted document counts in no cell, including the extraction it was running before it was deleted
        document(&pool, kb, "deleted.md", "extracting", true).await?;
        let page = documents::page(&pool, kb, None, None, None, false, 50, 0).await?;
        // The four ingested documents are all ready — the scope of the re-extraction is unchanged
        assert_eq!(page.ready, 4);
        assert_eq!(page.done, 1);
        assert_eq!(
            page.extracting, 2,
            "queued and extracting both count as extracting"
        );
        assert_eq!(page.failed, 1);
        // So the progress bar is done / (done + extracting) = 1 / 3. The old form, ready / (ready + extracting),
        // would be 4 / 6: the two not yet extracted and the one that failed extraction all counted as complete
        anyhow::Ok(())
    }
    .await;

    let _ = sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await;
    run
}
