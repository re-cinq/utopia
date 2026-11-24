//! 认领顺序（#1300、#1351）：不调对话模型的任务排在模型任务前面，哪怕它来得晚。
//!
//! `claim_one` 认领的是全库的任务，所以这两条放在一百年前，排在库里别的一切前面。

use super::claim_one;
use sqlx::{Connection, PgConnection, PgPool};

#[tokio::test]
async fn a_later_intake_job_is_claimed_before_an_earlier_extraction() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    for kind in ["sync_source", crate::phrase_bindings::MATERIALIZE_KIND] {
        let ids: Vec<i64> = sqlx::query_scalar(
            r#"INSERT INTO jobs (kind, payload, run_at) VALUES
               ('extract_document', '{"test": "claim-order"}', now() - interval '101 years'),
               ($1,                 '{"test": "claim-order"}', now() - interval '100 years')
           RETURNING id"#,
        )
        .bind(kind)
        .fetch_all(&pool)
        .await?;

        let claimed = claim_one(&pool, false).await?.map(|job| job.kind);
        sqlx::query("DELETE FROM jobs WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&pool)
            .await?;
        assert_eq!(claimed.as_deref(), Some(kind));
    }
    Ok(())
}

#[tokio::test]
async fn an_upgrade_reindexes_already_queued_materialization() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let schema = format!("claim_upgrade_{}", uuid::Uuid::now_v7().simple());
    let result: anyhow::Result<Vec<String>> = async {
        let mut tx = pool.begin().await?;
        // 旧函数和旧索引只装进这个 schema，不改并发测试正在用的队列。
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema};
             SET LOCAL search_path TO {schema};
             CREATE TABLE jobs (kind text, run_at timestamptz, status text DEFAULT 'queued');"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../../migrations/0116_intake_jobs_are_claimed_before_model_work.sql"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO jobs (kind, run_at) VALUES
               ('extract_document', now() - interval '1 day'),
               ($1, now())",
        )
        .bind(crate::phrase_bindings::MATERIALIZE_KIND)
        .execute(&mut *tx)
        .await?;
        // 旧数据在升级前已经提交；新进程迁移时没有旧函数的内联计划缓存。
        tx.commit().await?;
        let mut upgrade = PgConnection::connect(&url).await?;
        sqlx::raw_sql(&format!(
            "SET search_path TO {schema}; {}",
            include_str!("../../../migrations/0123_human_phrase_materialization_is_intake.sql")
        ))
        .execute(&mut upgrade)
        .await?;
        upgrade.close().await?;

        // 认领由升级后的新 worker 连接执行，直接验证已有队列的索引键。
        let mut fresh = PgConnection::connect(&url).await?;
        sqlx::raw_sql(&format!(
            "SET search_path TO {schema};
             SET enable_seqscan = off; SET enable_bitmapscan = off;"
        ))
        .execute(&mut fresh)
        .await?;
        let eligible = sqlx::query_scalar(
            "SELECT kind FROM jobs WHERE status = 'queued' AND job_priority(kind) = 0
             ORDER BY job_priority(kind), run_at",
        )
        .fetch_all(&mut fresh)
        .await?;
        fresh.close().await?;
        Ok(eligible)
    }
    .await;

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await?;
    assert_eq!(result?, vec![crate::phrase_bindings::MATERIALIZE_KIND]);
    Ok(())
}

#[tokio::test]
async fn a_done_job_older_than_the_retention_is_deleted_and_a_recent_one_kept() -> anyhow::Result<()>
{
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let kind = format!("prune_done_{}", uuid::Uuid::now_v7().simple());
    let ids: Vec<i64> = sqlx::query_scalar(
        r#"INSERT INTO jobs (kind, payload, status, updated_at) VALUES
           ($1, '{}', 'done', now() - interval '15 days'),
           ($1, '{}', 'done', now() - interval '1 day')
       RETURNING id"#,
    )
    .bind(&kind)
    .fetch_all(&pool)
    .await?;
    let recent = ids[1];

    let pruned = super::prune_done(&pool, std::time::Duration::from_secs(14 * 24 * 3600)).await;
    let remaining: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs WHERE kind = $1 ORDER BY id")
        .bind(&kind)
        .fetch_all(&pool)
        .await?;
    sqlx::query("DELETE FROM jobs WHERE kind = $1")
        .bind(&kind)
        .execute(&pool)
        .await?;
    pruned?;
    assert_eq!(remaining, vec![recent]);
    Ok(())
}

const RETENTION: std::time::Duration = std::time::Duration::from_secs(14 * 24 * 3600);

/// 按 status 与 age（SQL interval）播 `count` 条该 kind 的任务，清理一次，返回
/// （prune 的返回值，剩余行数），并把这个 kind 的行删干净。
async fn prune_seeded(
    kind_prefix: &str,
    status: &str,
    age: &str,
    count: i64,
) -> anyhow::Result<(u64, i64)> {
    let url = crate::test_db::url().expect("db url checked by caller");
    let pool = PgPool::connect(&url).await?;
    let kind = format!("{kind_prefix}_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(
        "INSERT INTO jobs (kind, payload, status, updated_at)
         SELECT $1, jsonb_build_object('n', n), $2, now() - $3::interval
           FROM generate_series(1, $4) n",
    )
    .bind(&kind)
    .bind(status)
    .bind(age)
    .bind(count as i32)
    .execute(&pool)
    .await?;
    let pruned = super::prune_done(&pool, RETENTION).await;
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1")
        .bind(&kind)
        .fetch_one(&pool)
        .await?;
    sqlx::query("DELETE FROM jobs WHERE kind = $1")
        .bind(&kind)
        .execute(&pool)
        .await?;
    Ok((pruned?, remaining))
}

#[tokio::test]
async fn a_failed_job_older_than_the_retention_is_kept() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (_, remaining) = prune_seeded("prune_failed", "failed", "15 days", 1).await?;
    assert_eq!(remaining, 1);
    Ok(())
}

#[tokio::test]
async fn a_queued_job_older_than_the_retention_is_kept() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (_, remaining) = prune_seeded("prune_queued", "queued", "15 days", 1).await?;
    assert_eq!(remaining, 1);
    Ok(())
}

#[tokio::test]
async fn a_running_job_older_than_the_retention_is_kept() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (_, remaining) = prune_seeded("prune_running", "running", "15 days", 1).await?;
    assert_eq!(remaining, 1);
    Ok(())
}

#[tokio::test]
async fn a_done_job_just_inside_the_retention_is_kept() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (_, remaining) = prune_seeded("prune_inside", "done", "13 days 23 hours", 1).await?;
    assert_eq!(remaining, 1);
    Ok(())
}

#[tokio::test]
async fn prune_done_returns_the_number_of_rows_deleted() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (pruned, _) = prune_seeded("prune_count", "done", "15 days", 7).await?;
    assert_eq!(pruned, 7);
    Ok(())
}

#[tokio::test]
async fn prune_done_deletes_every_row_when_more_than_one_batch_is_old() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (_, remaining) = prune_seeded("prune_batches", "done", "15 days", 12_000).await?;
    assert_eq!(remaining, 0);
    Ok(())
}

/// 先放一条已有的任务，再 `enqueue_unless_pending` 同 kind 的 `{"kb": "a"}`；
/// 返回新任务有没有排上。
async fn enqueues_after_existing(
    existing_status: &str,
    existing_payload: serde_json::Value,
) -> anyhow::Result<bool> {
    let url = crate::test_db::url().expect("db url checked by caller");
    let pool = PgPool::connect(&url).await?;
    let kind = format!("enqueue_dedupe_{}", uuid::Uuid::now_v7().simple());
    sqlx::query("INSERT INTO jobs (kind, payload, status) VALUES ($1, $2, $3)")
        .bind(&kind)
        .bind(existing_payload)
        .bind(existing_status)
        .execute(&pool)
        .await?;
    let enqueued = super::enqueue_unless_pending(
        &pool,
        &kind,
        serde_json::json!({"kb": "a"}),
        std::time::Duration::ZERO,
    )
    .await;
    sqlx::query("DELETE FROM jobs WHERE kind = $1")
        .bind(&kind)
        .execute(&pool)
        .await?;
    Ok(enqueued?.is_some())
}

#[tokio::test]
async fn a_queued_job_with_another_payload_does_not_block_an_enqueue() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    assert!(enqueues_after_existing("queued", serde_json::json!({"kb": "b"})).await?);
    Ok(())
}

#[tokio::test]
async fn a_done_job_with_the_same_payload_does_not_block_an_enqueue() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    assert!(enqueues_after_existing("done", serde_json::json!({"kb": "a"})).await?);
    Ok(())
}

#[tokio::test]
async fn a_running_job_with_the_same_payload_blocks_an_enqueue() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    assert!(!enqueues_after_existing("running", serde_json::json!({"kb": "a"})).await?);
    Ok(())
}
