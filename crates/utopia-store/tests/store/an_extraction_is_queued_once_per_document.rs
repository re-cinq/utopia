//! RC-009：一篇文档排着一份抽取时，再变一次不再多排一份。
//!
//! `extract_document` 只带文档 id，跑的时候读文档当下的块，所以同一篇排着的第二份读到的和
//! 第一份一模一样。两处入队的载荷不同（一处多带提议人），按 `document_id` 比，不按整份载荷。
//! 在跑的那份不算：它可能在新版本到之前就读完了，新版本得有自己的一次。

use sqlx::PgPool;
use utopia_store::jobs;
use uuid::Uuid;

async fn queued_for(pool: &PgPool, document: Uuid) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT status FROM jobs
          WHERE kind = 'extract_document' AND payload->>'document_id' = $1 ORDER BY id",
    )
    .bind(document.to_string())
    .fetch_all(pool)
    .await?)
}

async fn clean(pool: &PgPool, document: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM jobs WHERE kind = 'extract_document' AND payload->>'document_id' = $1",
    )
    .bind(document.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_second_change_while_its_extraction_waits_queues_nothing() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let document = Uuid::now_v7();

    let first =
        jobs::enqueue_extraction(&pool, serde_json::json!({ "document_id": document }), 3).await?;
    let second = jobs::enqueue_extraction(
        &pool,
        serde_json::json!({ "document_id": document, "proposed_by": null, "proposed_token": null }),
        3,
    )
    .await?;
    let statuses = queued_for(&pool, document).await?;

    clean(&pool, document).await?;
    pool.close().await;
    assert!(first.is_some());
    assert_eq!((second, statuses), (None, vec!["queued".to_string()]));
    Ok(())
}

#[tokio::test]
async fn a_change_while_its_extraction_runs_queues_one_more() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let document = Uuid::now_v7();
    let payload = serde_json::json!({ "document_id": document });

    jobs::enqueue_extraction(&pool, payload.clone(), 3).await?;
    sqlx::query(
        "UPDATE jobs SET status = 'running', locked_at = now()
          WHERE kind = 'extract_document' AND payload->>'document_id' = $1",
    )
    .bind(document.to_string())
    .execute(&pool)
    .await?;
    let again = jobs::enqueue_extraction(&pool, payload, 3).await?;
    let statuses = queued_for(&pool, document).await?;

    clean(&pool, document).await?;
    pool.close().await;
    assert!(again.is_some());
    assert_eq!(statuses, vec!["running".to_string(), "queued".to_string()]);
    Ok(())
}
