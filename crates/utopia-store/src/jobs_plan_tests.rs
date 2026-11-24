//! 排队前的「已有同样的在排或在跑」查找，不该去读那些早已结束的任务：生产的 jobs 表
//! 260 万行、924 MB，几乎全是 done/failed，而每次入队都顺序扫一遍整张表。
//!
//! 计划针对 jobs 的临时副本（统计信息只来自播种），共享库里并发的写入翻不了计划。
use super::ENQUEUE_UNLESS_PENDING_SQL;
use sqlx::postgres::PgPoolOptions;

const FINISHED: u32 = 500_000;
const QUEUED: u32 = 20_000;

#[tokio::test]
async fn a_pending_job_is_found_without_reading_the_finished_ones() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(&format!(
        "CREATE TEMP TABLE jobs (LIKE public.jobs INCLUDING ALL) ON COMMIT DROP;
         INSERT INTO jobs (kind, payload, status)
              SELECT 'kind ' || (n % 12), jsonb_build_object('kb_id', 'kb ' || (n % 4000)),
                     CASE WHEN n % 10 = 0 THEN 'failed' ELSE 'done' END
                FROM generate_series(1, {FINISHED}) n;
         INSERT INTO jobs (kind, payload, status)
              SELECT 'kind ' || (n % 12), jsonb_build_object('kb_id', 'kb ' || (n % 4000)), 'queued'
                FROM generate_series(1, {QUEUED}) n;
         INSERT INTO jobs (kind, payload, status)
              SELECT 'kind ' || n, jsonb_build_object('kb_id', 'running ' || n), 'running'
                FROM generate_series(1, 5) n;
         ANALYZE pg_temp.jobs"
    ))
    .execute(&mut *tx)
    .await?;

    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {ENQUEUE_UNLESS_PENDING_SQL}"
    ))
    .persistent(false)
    .bind("kind 3")
    .bind(serde_json::json!({ "kb_id": "no such kb" }))
    .bind(0.0_f64)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let read = job_rows_read(&plan[0]["Plan"]);
    assert!(read < 1_000, "{read} job rows read: {plan:#}");
    Ok(())
}

/// Rows the plan read from `jobs`, counting what filters threw away, over all loops.
fn job_rows_read(node: &serde_json::Value) -> u64 {
    let own_rows = if node["Relation Name"].as_str() == Some("jobs") {
        ["Actual Rows", "Rows Removed by Filter"]
            .iter()
            .map(|field| node[*field].as_u64().unwrap_or(0))
            .sum::<u64>()
            * node["Actual Loops"].as_u64().unwrap_or(0)
    } else {
        0
    };
    own_rows
        + node["Plans"]
            .as_array()
            .map_or(0, |children| children.iter().map(job_rows_read).sum())
}
