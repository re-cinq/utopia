//! A worker running nothing still claims one job while memory is short, because the memory is held by the rest of the process and waiting
//! would stall the queue forever; while that job runs, no further job is claimed, and once it finishes the idle worker claims the next.
//!
//! Starts the real `run_worker`, which claims any queued task in the database, so it is its own binary (see `tests/store/main.rs`).
//! The handler only accepts tasks carrying this test's marker.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use utopia_store::jobs::{self, MemoryUse};

const SHORT_MEMORY: MemoryUse = MemoryUse {
    working_set_bytes: 900,
    limit_bytes: 1000,
};

#[tokio::test]
async fn an_idle_worker_claims_one_job_while_memory_is_short() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let mark = uuid::Uuid::now_v7().to_string();
    let (started, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let (ours, held) = (mark.clone(), release.clone());
    let worker = tokio::spawn(jobs::run_worker(
        pool.clone(),
        Arc::new(AtomicUsize::new(4)),
        || Some(SHORT_MEMORY),
        move |job| {
            let (ours, started, held) = (ours.clone(), started.clone(), held.clone());
            async move {
                anyhow::ensure!(
                    job.payload["lane_test"] == ours.as_str(),
                    "not this test's job"
                );
                started.send(job.id)?;
                held.acquire().await?.forget();
                Ok(())
            }
        },
    ));
    let queue = || {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO jobs (kind, payload, run_at) VALUES ('sync_source', $1, now() - interval '100 years') RETURNING id",
        )
        .bind(serde_json::json!({ "lane_test": mark }))
        .fetch_one(&pool)
    };
    let result = async {
        let first_id = queue().await?;
        let second_id = queue().await?;
        let first_entered = tokio::time::timeout(Duration::from_secs(10), entered.recv()).await;
        anyhow::ensure!(
            matches!(first_entered, Ok(Some(id)) if id == first_id),
            "idle worker did not claim the first job while memory was short: {first_entered:?}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        let while_running: (String, i32) =
            sqlx::query_as("SELECT status, attempts FROM jobs WHERE id = $1")
                .bind(second_id)
                .fetch_one(&pool)
                .await?;
        anyhow::ensure!(
            while_running == ("queued".to_string(), 0),
            "claimed a second job while one was running and memory was short: {while_running:?}"
        );
        release.add_permits(1);
        let second_entered = tokio::time::timeout(Duration::from_secs(10), entered.recv()).await;
        anyhow::ensure!(
            matches!(second_entered, Ok(Some(id)) if id == second_id),
            "idle again, the worker did not claim the next job: {second_entered:?}"
        );
        anyhow::Ok(())
    }
    .await;
    worker.abort();
    let _ = worker.await;
    sqlx::query("DELETE FROM jobs WHERE payload->>'lane_test' = $1")
        .bind(&mark)
        .execute(&pool)
        .await?;
    result
}
