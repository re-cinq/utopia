//! RC-009：迁移 9011 删掉同一篇文档排着的重复抽取，留最早那份，在跑的不动，别的文档不动。
//!
//! 这条 DELETE 不按 kb 过滤、看全库排着的抽取，所以自成一个二进制（为什么见 `store/main.rs`）。

use sqlx::PgPool;
use uuid::Uuid;

const MIGRATION: &str =
    include_str!("../../../migrations/9011_a_document_waits_for_one_extraction.sql");

async fn queue(pool: &PgPool, document: Uuid, status: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "INSERT INTO jobs (kind, payload, status) VALUES ('extract_document', $1, $2) RETURNING id",
    )
    .bind(serde_json::json!({ "document_id": document }))
    .bind(status)
    .fetch_one(pool)
    .await?)
}

async fn jobs_of(pool: &PgPool, document: Uuid) -> anyhow::Result<Vec<(i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, status FROM jobs
          WHERE kind = 'extract_document' AND payload->>'document_id' = $1 ORDER BY id",
    )
    .bind(document.to_string())
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn the_repeats_queued_for_one_document_are_removed_keeping_the_oldest() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (busy, alone) = (Uuid::now_v7(), Uuid::now_v7());
    let running = queue(&pool, busy, "running").await?;
    let oldest = queue(&pool, busy, "queued").await?;
    queue(&pool, busy, "queued").await?;
    queue(&pool, busy, "queued").await?;
    let single = queue(&pool, alone, "queued").await?;

    sqlx::raw_sql(MIGRATION).execute(&pool).await?;
    sqlx::raw_sql(MIGRATION).execute(&pool).await?;
    let kept = (jobs_of(&pool, busy).await?, jobs_of(&pool, alone).await?);

    sqlx::query(
        "DELETE FROM jobs WHERE kind = 'extract_document' AND payload->>'document_id' IN ($1, $2)",
    )
    .bind(busy.to_string())
    .bind(alone.to_string())
    .execute(&pool)
    .await?;
    pool.close().await;
    assert_eq!(
        kept,
        (
            vec![
                (running, "running".to_string()),
                (oldest, "queued".to_string())
            ],
            vec![(single, "queued".to_string())],
        )
    );
    Ok(())
}
