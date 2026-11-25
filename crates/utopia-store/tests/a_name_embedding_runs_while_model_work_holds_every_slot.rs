//! re:cinq fork: `embed_names` only calls the embedding model; even when the regular slots are full of extraction, the reserved lane still claims it (migration 9003).
//!
//! starts the real `run_worker`, so it is its own binary. The handler only accepts tasks carrying this test's marker.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use utopia_store::jobs;

#[tokio::test]
async fn a_name_embedding_runs_while_model_work_holds_every_slot() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let priority: i16 = sqlx::query_scalar("SELECT job_priority('embed_names')")
        .fetch_one(&pool)
        .await?;
    assert_eq!(priority, 0);
    let mark = uuid::Uuid::now_v7().to_string();
    let (started, mut extracting) = tokio::sync::mpsc::unbounded_channel();
    let (finished, mut ran) = tokio::sync::mpsc::unbounded_channel();
    let ours = mark.clone();
    let worker = tokio::spawn(jobs::run_worker(
        pool.clone(),
        Arc::new(AtomicUsize::new(1)),
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
        sqlx::query("INSERT INTO jobs (kind, payload, run_at) VALUES ($1, $2, now() - interval '100 years')")
            .bind(kind)
            .bind(serde_json::json!({ "lane_test": mark }))
            .execute(&pool)
    };
    let result = async {
        queue("extract_document").await?;
        tokio::time::timeout(Duration::from_secs(10), extracting.recv()).await?;
        queue("embed_names").await?;
        let embedded = tokio::time::timeout(Duration::from_secs(10), ran.recv()).await;
        anyhow::ensure!(
            matches!(&embedded, Ok(Some(kind)) if kind == "embed_names"),
            "the name embedding waited behind the extraction: {embedded:?}"
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
