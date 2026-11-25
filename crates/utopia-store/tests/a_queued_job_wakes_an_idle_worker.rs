//! #517: a queued task does not wait for polling; an idle worker wakes immediately.
//!
//! When idle, the worker no longer sleeps for two seconds; it waits for notifications on the `utopia_jobs` channel, and the two-second poll is only a fallback
//! (retries scheduled with a future `run_at`, notifications lost during reconnects, and workers woken by one notification that did not get the task).
//! What is guarded here is the wait function the worker really uses, `jobs::wait_for_work`, without starting a full worker:
//! a full worker claims any queued task in the database, and when the test database is shared it would steal work from other tests.
//!
//! Three things:
//! 1. **Enqueue wakes it.** The notification arrives within the poll interval, and the wait ends with `Notified`.
//! 2. **Notification only after commit.** While the insert is still in an uncommitted transaction, the wait can only end by polling; it wakes after the commit.
//!    This is the ordering hazard named in #517: if it woke before the row was visible, it would wake to nothing and go back to polling.
//! 3. **Wakes even without a notification.** The poll arm is still there; after the full interval it ends with `Polled`.
//!
//! The three tests share one channel and run serially: in parallel, their notifications would wake each other.
//! Run it against a database that no other process writes tasks into.

use sqlx::PgPool;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use utopia_store::jobs::{self, Wake};

/// Short poll: the test only cares that notification is faster than polling, without waiting for the real two seconds.
const POLL: Duration = Duration::from_millis(300);
static SERIAL: Mutex<()> = Mutex::const_new(());

async fn connect() -> anyhow::Result<Option<PgPool>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    Ok(Some(PgPool::connect(&url).await?))
}

async fn listener(pool: &PgPool) -> anyhow::Result<sqlx::postgres::PgListener> {
    jobs::listen_for_jobs(pool)
        .await
        .ok_or_else(|| anyhow::anyhow!("the wake-up listener could not be built"))
}

async fn remove(pool: &PgPool, id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_queued_job_wakes_an_idle_worker() -> anyhow::Result<()> {
    let _serial = SERIAL.lock().await;
    let Some(pool) = connect().await? else {
        return Ok(());
    };
    let mut listener = listener(&pool).await?;

    let started = Instant::now();
    let id = jobs::enqueue(&pool, "wake_test", serde_json::json!({})).await?;
    let wake = jobs::wait_for_work(Some(&mut listener), POLL).await;
    let waited = started.elapsed();
    remove(&pool, id).await?;

    assert_eq!(
        wake,
        Wake::Notified,
        "an enqueue must end the idle wait with a notification"
    );
    assert!(
        waited < POLL,
        "woke after {waited:?}, which is not faster than the {POLL:?} poll"
    );
    Ok(())
}

#[tokio::test]
async fn a_notification_never_arrives_before_the_row() -> anyhow::Result<()> {
    let _serial = SERIAL.lock().await;
    let Some(pool) = connect().await? else {
        return Ok(());
    };
    let mut listener = listener(&pool).await?;

    let mut tx = pool.begin().await?;
    let id =
        jobs::enqueue_with_max_attempts_tx(&mut tx, "wake_test", serde_json::json!({}), 3).await?;
    let before_commit = jobs::wait_for_work(Some(&mut listener), POLL).await;
    tx.commit().await?;
    let after_commit = jobs::wait_for_work(Some(&mut listener), POLL).await;
    remove(&pool, id).await?;

    assert_eq!(
        before_commit,
        Wake::Polled,
        "nothing may wake a worker while the row is still invisible"
    );
    assert_eq!(
        after_commit,
        Wake::Notified,
        "the commit is what wakes the worker"
    );
    Ok(())
}

#[tokio::test]
async fn the_poll_still_fires_without_a_notification() -> anyhow::Result<()> {
    let _serial = SERIAL.lock().await;
    let Some(pool) = connect().await? else {
        return Ok(());
    };
    let mut listener = listener(&pool).await?;

    let started = Instant::now();
    let wake = jobs::wait_for_work(Some(&mut listener), POLL).await;
    let waited = started.elapsed();

    assert_eq!(wake, Wake::Polled);
    assert!(
        waited >= POLL,
        "the poll arm returned after {waited:?}, before its {POLL:?}"
    );
    Ok(())
}
