//! End-to-end check that the `Deferred` marker from `#526` works.
//!
//! Only two boundaries are tested:
//!
//! 1. The same wait condition goes through `Deferred` twice, and `attempts` does **not** climb to `max_attempts` --
//!    each time the job is handed back, **the budget is not spent**. This is the difference between "wait for the ontology vectors to be backfilled" and "retry
//!    a failed call": the former is one clock; the latter should back off and increase.
//! 2. `run_at` really is pushed into the future -- rather than "try again immediately", which would contend with the running
//!    `embed_ontology` job for the same library-level lock.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set (see `utopia_store::test_db`).

use std::time::Duration;

use anyhow::anyhow;
use sqlx::PgPool;
use utopia_core::Deferred;
use utopia_store::jobs;

#[tokio::test]
async fn a_deferred_job_does_not_spend_its_budget() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;

    // Insert one "running" job directly -- attempts has already been incremented once by `claim_one`,
    // and this step's semantics are that `mark_failed(Deferred)` hands it back when it writes back.
    let job: jobs::Job = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, status, attempts, max_attempts, locked_at)
         VALUES ('extract_document', '{\"document_id\":\"00000000-0000-0000-0000-000000000000\"}',
                 'running', 2, 3, now())
         RETURNING id, kind, payload, attempts, max_attempts, locked_at",
    )
    .fetch_one(&pool)
    .await?;
    let id = job.id;

    // First wait: the ontology vectors are being backfilled, 30s.
    let err = anyhow!("waiting on ontology index").context(Deferred::new(Duration::from_secs(30)));
    jobs::mark_failed(&pool, &job, &err).await?;

    let (status, attempts, last_error): (String, i32, String) =
        sqlx::query_as("SELECT status, attempts, last_error FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "queued", "Deferred 应该让任务挂回 queued");
    assert_eq!(attempts, 1, "attempts 应该被退回去：原 2 减 1 等于 1");
    assert!(
        last_error.contains("waiting on ontology index"),
        "last_error 应当是让运维一眼知道在等什么的措辞；实际是：{last_error:?}"
    );

    // `run_at` lands in the future -- not "immediately contend for the lock again".
    let run_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT run_at FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    let now = chrono::Utc::now();
    let ahead = (run_at - now).num_seconds();
    assert!(
        (25..=35).contains(&ahead),
        "run_at 应该比 now 晚约 30s；实际 {ahead}s"
    );

    // Second wait: the same job, the same wait. **The budget is not spent** --
    // this is the key difference from "rate-limit retry".
    let err2 = anyhow!("waiting on ontology index").context(Deferred::new(Duration::from_secs(30)));
    let job2: jobs::Job = sqlx::query_as(
        "UPDATE jobs SET status='running', attempts=attempts+1, locked_at=now()
         WHERE id=$1 AND status='queued'
         RETURNING id, kind, payload, attempts, max_attempts, locked_at",
    )
    .bind(id)
    .fetch_one(&pool)
    .await?;
    jobs::mark_failed(&pool, &job2, &err2).await?;
    let (status2, attempts2, _): (String, i32, String) =
        sqlx::query_as("SELECT status, attempts, last_error FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(status2, "queued");
    assert_eq!(attempts2, 1, "第二次认领再 Deferred 不应消耗重试预算");

    // Cleanup: avoid polluting the next run (other tests share the same database)
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;
    Ok(())
}

/// `Terminal` takes priority over `Deferred`: when both markers are attached, the outcome is decided by `Terminal`.
/// This is the semantic guarantee of `mark_failed`, where `is_terminal` is written first and `is_deferred` after.
#[tokio::test]
async fn terminal_wins_over_deferred_when_both_attached() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;

    let job: jobs::Job = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, status, attempts, max_attempts, locked_at)
         VALUES ('extract_document', '{\"document_id\":\"00000000-0000-0000-0000-000000000000\"}',
                 'running', 1, 3, now())
         RETURNING id, kind, payload, attempts, max_attempts, locked_at",
    )
    .fetch_one(&pool)
    .await?;
    let id = job.id;

    // Attach Deferred first, then Terminal. Terminal is the one that is "the last change of mind".
    let err = anyhow!("balance gone after waiting").context(Deferred::new(Duration::from_secs(30)));
    let err = err.context(utopia_core::Terminal);
    jobs::mark_failed(&pool, &job, &err).await?;

    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "failed", "Terminal 应该让任务直接进 failed");
    // attempts is not handed back -- `failed` takes a different SQL path.
    assert_eq!(attempts, 1);

    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await?;
    Ok(())
}

/// The wait also has a deadline: if still waiting more than `DEFER_WINDOW_SECS` after the first hand-back, it is treated as an ordinary failure,
/// backing off and spending the budget. When the thing being waited on keeps failing itself, the job should not wake every 30 seconds and sit queued forever.
#[tokio::test]
async fn a_wait_past_its_window_is_a_failure() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;

    // Record the moment waiting began, on the first hand-back
    let job: jobs::Job = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, status, attempts, max_attempts, locked_at)
         VALUES ('extract_document', '{}', 'running', 1, 3, now())
         RETURNING id, kind, payload, attempts, max_attempts, locked_at",
    )
    .fetch_one(&pool)
    .await?;
    let fresh = job.id;
    let err = anyhow!("waiting on ontology index").context(Deferred::new(Duration::from_secs(30)));
    jobs::mark_failed(&pool, &job, &err).await?;
    let since: Option<String> =
        sqlx::query_scalar("SELECT payload->>'deferred_since' FROM jobs WHERE id = $1")
            .bind(fresh)
            .fetch_one(&pool)
            .await?;
    assert!(since.is_some(), "第一次等待应当记下 deferred_since");

    // The deadline has passed: attempts is not handed back again, it takes ordinary backoff
    let job: jobs::Job = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, status, attempts, max_attempts, locked_at)
         VALUES ('extract_document',
                 jsonb_build_object('deferred_since', (now() - make_interval(secs => $1::float8))::text),
                 'running', 1, 3, now())
         RETURNING id, kind, payload, attempts, max_attempts, locked_at",
    )
    .bind((jobs::DEFER_WINDOW_SECS + 60) as f64)
    .fetch_one(&pool)
    .await?;
    let stale = job.id;
    jobs::mark_failed(&pool, &job, &err).await?;
    let (status, attempts): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM jobs WHERE id = $1")
            .bind(stale)
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "queued", "还有预算，按退避重排");
    assert_eq!(attempts, 1, "过了期限的等待要烧预算，attempts 不再退回");

    sqlx::query("DELETE FROM jobs WHERE id = ANY($1)")
        .bind(vec![fresh, stale])
        .execute(&pool)
        .await?;
    Ok(())
}
