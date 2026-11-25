//! #1304: a store that is due is queued only once while its inference task is still pending.
//!
//! `queue_due_inferences` scans every store that is due, and a store just created by a concurrently running test would also get a task queued by it, so
//! this test is its own binary (why, see `store/main.rs`), and cleans up **every** `materialize_inferences` it queued at the end,
//! not only the one for its own store.

use sqlx::PgPool;
use utopia_store::reasoning::queue_due_inferences;
use uuid::Uuid;

#[tokio::test]
async fn a_due_base_is_queued_once_while_its_job_waits() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','inference-schedule');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','inference-schedule');
         INSERT INTO knowledge_bases(id,workspace_id,name,materialize_inferences)
              VALUES ('{kb}','{ws}','inference-schedule',true);"
    ))
    .execute(&pool)
    .await?;
    let before: i64 = sqlx::query_scalar("SELECT coalesce(max(id), 0) FROM jobs")
        .fetch_one(&pool)
        .await?;

    // Nobody claimed it between the two passes: a store never pushed is due on both passes
    queue_due_inferences(&pool).await?;
    queue_due_inferences(&pool).await?;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs
          WHERE kind = 'materialize_inferences' AND status = 'queued' AND payload->>'kb_id' = $1",
    )
    .bind(kb.to_string())
    .fetch_one(&pool)
    .await?;

    sqlx::query("DELETE FROM jobs WHERE kind = 'materialize_inferences' AND id > $1")
        .bind(before)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    pool.close().await;
    assert_eq!(queued, 1);
    Ok(())
}
