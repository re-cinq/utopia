//! The headroom gate covers the reserved no-chat lane too: with the only regular slot held, a queued priority-0 job is still not claimed while
//! the injected memory reader reports the process close to its limit, and is claimed once the reading drops.
//!
//! Starts the real `run_worker`, which claims any queued task in the database, so it is its own binary (see `tests/store/main.rs`).
//! The handler only accepts tasks carrying this test's marker.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use utopia_store::jobs::{self, MemoryUse};

const LIMIT_BYTES: u64 = 1000;
const SHORT_WORKING_SET: u64 = 900;
const ROOMY_WORKING_SET: u64 = 500;

#[tokio::test]
async fn the_reserved_lane_pauses_while_memory_is_short() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let mark = uuid::Uuid::now_v7().to_string();
    let working_set = Arc::new(AtomicU64::new(ROOMY_WORKING_SET));
    let reading = working_set.clone();
    let (started, mut extracting) = tokio::sync::mpsc::unbounded_channel();
    let (finished, mut ran) = tokio::sync::mpsc::unbounded_channel();
    let ours = mark.clone();
    let worker = tokio::spawn(jobs::run_worker(
        pool.clone(),
        Arc::new(AtomicUsize::new(1)),
        move || {
            Some(MemoryUse {
                working_set_bytes: reading.load(Ordering::Relaxed),
                limit_bytes: LIMIT_BYTES,
            })
        },
        move |job| {
            let (ours, started, finished) = (ours.clone(), started.clone(), finished.clone());
            async move {
                anyhow::ensure!(
                    job.payload["lane_test"] == ours.as_str(),
                    "not this test's job"
                );
                if job.kind == "extract_document" {
                    started.send(())?;
                    std::future::pending::<()>().await;
                }
                finished.send(job.kind)?;
                Ok(())
            }
        },
    ));
    let queue = |kind: &'static str| {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO jobs (kind, payload, run_at) VALUES ($1, $2, now() - interval '100 years') RETURNING id",
        )
        .bind(kind)
        .bind(serde_json::json!({ "lane_test": mark }))
        .fetch_one(&pool)
    };
    let result = async {
        queue("extract_document").await?;
        tokio::time::timeout(Duration::from_secs(10), extracting.recv()).await?;
        working_set.store(SHORT_WORKING_SET, Ordering::Relaxed);
        let job_id = queue("embed_names").await?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let while_short: (String, i32) =
            sqlx::query_as("SELECT status, attempts FROM jobs WHERE id = $1")
                .bind(job_id)
                .fetch_one(&pool)
                .await?;
        anyhow::ensure!(
            while_short == ("queued".to_string(), 0),
            "the reserved lane claimed while memory was short: {while_short:?}"
        );
        working_set.store(ROOMY_WORKING_SET, Ordering::Relaxed);
        let claimed = tokio::time::timeout(Duration::from_secs(10), ran.recv()).await;
        anyhow::ensure!(
            matches!(&claimed, Ok(Some(kind)) if kind == "embed_names"),
            "not claimed after memory dropped: {claimed:?}"
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
