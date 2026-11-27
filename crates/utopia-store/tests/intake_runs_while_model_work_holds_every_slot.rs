//! #1300, #1351: when the regular slots are all held by model tasks, sync and materialization after human judgment still run in their slots.
//!
//! Starts the real `run_worker`, which claims any queued task in the database, so it is its own binary (see
//! `tests/store/main.rs`). The handler only accepts tasks carrying this test's marker; others' tasks all error out, so it does no one else's work.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use utopia_store::jobs;

#[tokio::test]
async fn intake_runs_while_model_work_holds_every_slot() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let mark = uuid::Uuid::now_v7().to_string();
    let (started, mut extracting) = tokio::sync::mpsc::unbounded_channel();
    let (finished, mut ran) = tokio::sync::mpsc::unbounded_channel();
    let ours = mark.clone();
    let worker = tokio::spawn(jobs::run_worker(
        pool.clone(),
        Arc::new(AtomicUsize::new(1)),
        || None,
        move |job| {
            let (ours, started, finished) = (ours.clone(), started.clone(), finished.clone());
            async move {
                anyhow::ensure!(
                    job.payload["lane_test"] == ours.as_str(),
                    "not this test's job"
                );
                if job.kind == "extract_document" {
                    started.send(())?;
                    // Holds the only regular slot all the time, like an extraction waiting for model permission
                    std::future::pending::<()>().await;
                }
                finished.send(job.kind)?;
                Ok(())
            }
        },
    ));
    let queue = |kind: &'static str| {
        // Put a hundred years in the past, ahead of everything else in the database at the same priority
        sqlx::query("INSERT INTO jobs (kind, payload, run_at) VALUES ($1, $2, now() - interval '100 years')")
            .bind(kind)
            .bind(serde_json::json!({ "lane_test": mark }))
            .execute(&pool)
    };
    let result = async {
        queue("extract_document").await?;
        tokio::time::timeout(Duration::from_secs(10), extracting.recv()).await?;
        for kind in [
            "sync_source",
            utopia_store::phrase_bindings::MATERIALIZE_KIND,
        ] {
            queue(kind).await?;
            let claimed = tokio::time::timeout(Duration::from_secs(10), ran.recv()).await;
            anyhow::ensure!(
                matches!(&claimed, Ok(Some(claimed_kind)) if claimed_kind == kind),
                "{kind} waited behind the extraction: {claimed:?}"
            );
        }
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
