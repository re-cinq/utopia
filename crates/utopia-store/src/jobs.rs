//! Task queue: Postgres `FOR UPDATE SKIP LOCKED` consumption.
//! The worker runs in the same process as the API (tokio task); failures retry with a backoff of 30s * attempts²,
//! unless the handler marks the task as `utopia_core::Terminal`, in which case it stops after one attempt (see [`retry_delay`]).
//! Concurrent consumption: the scheduling loop tops up "running < target count", and each task runs in its own task;
//! the target count is hot-read through an AtomicUsize, so changing concurrency in system settings takes effect immediately, no restart needed.

use chrono::{DateTime, Utc};
use sqlx::{postgres::PgListener, PgPool, Postgres, Transaction};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use utopia_core::AppResult;
use uuid::Uuid;

pub const JOB_CHANNEL: &str = "utopia_jobs";

/// Extraction of a document that goes through the chat model.
pub const EXTRACT_DOCUMENT: &str = "extract_document";
/// Extraction of a document of a `statements` source: no model call, so it is claimed ahead of model work (migration 9018).
pub const EXTRACT_STATEMENTS: &str = "extract_statements";
/// Both extraction kinds; a document has at most one queued job of either.
pub const EXTRACTION_KINDS: [&str; 2] = [EXTRACT_DOCUMENT, EXTRACT_STATEMENTS];

/// SQL for the extraction kind of the document whose id the SQL expression `document_id` yields:
/// by its source, and `extract_document` when it has none. Shared by every place that queues an extraction.
pub fn extraction_kind_sql(document_id: &str) -> String {
    format!(
        "coalesce((SELECT CASE WHEN s.kind = 'statements' THEN '{EXTRACT_STATEMENTS}' END
                     FROM documents d JOIN sources s ON s.id = d.source_id
                    WHERE d.id = {document_id}), '{EXTRACT_DOCUMENT}')"
    )
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub attempts: i32,
    pub max_attempts: i32,
    pub locked_at: DateTime<Utc>,
}

/// The dedup comparison: the payload with `deferred_since` removed equals `$2`. Deferral (`persist_outcome`) writes it into the queued
/// task payload. It is bookkeeping, not part of what the task does, so dedup must ignore it, otherwise a deferred task can never absorb new enqueues.
/// A macro is used so the constant SQL can be built with `concat!`; the `md5(...)` half must match migration 9015's index expression word for word,
/// or the planner would not use the index.
macro_rules! same_payload_ignoring_deferral {
    () => {
        "md5((payload - 'deferred_since')::text) = md5($2::text) AND (payload - 'deferred_since') = $2"
    };
}

/// The SQL of `enqueue_unless_pending` (text unchanged; extracted only so plan tests can use it).
const ENQUEUE_UNLESS_PENDING_SQL: &str = concat!(
    "INSERT INTO jobs (kind, payload, run_at)
         SELECT $1, $2, now() + make_interval(secs => $3)
          WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND ",
    same_payload_ignoring_deferral!(),
    " AND status IN ('queued', 'running'))
         RETURNING id"
);

const ENQUEUE_UNLESS_QUEUED_SQL: &str = concat!(
    "INSERT INTO jobs (kind, payload)
         SELECT $1, $2
          WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND ",
    same_payload_ignoring_deferral!(),
    " AND status = 'queued')
         RETURNING id"
);

const ENQUEUE_UNLESS_QUEUED_AFTER_SQL: &str = concat!(
    "INSERT INTO jobs (kind, payload, run_at)
         SELECT $1, $2, now() + make_interval(secs => $3)
          WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND ",
    same_payload_ignoring_deferral!(),
    " AND status = 'queued')
         RETURNING id"
);

const ENQUEUE_QUEUED_ONCE_SQL: &str = concat!(
    "INSERT INTO jobs (kind, payload, max_attempts)
         SELECT $1, $2, $3
          WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE kind = $1 AND ",
    same_payload_ignoring_deferral!(),
    " AND status = 'queued')
         RETURNING id"
);

const FIND_QUEUED_SQL: &str = concat!(
    "SELECT id FROM jobs WHERE kind = $1 AND ",
    same_payload_ignoring_deferral!(),
    " AND status = 'queued' ORDER BY id LIMIT 1"
);

/// The same kind of task with the same payload already queued is not queued again: when a batch of documents each finish extraction and all want to trigger the same library-level task
/// (type resolution, reconciliation), queuing ten times runs it ten times, and the last nine see the library as the first run left it.
/// Returns None = one is already queued
/// Queue a thing unless the same thing is **already queued or running**.
///
/// The only difference from [`enqueue_unless_queued`] is the "running" half, and that half is what batch edits
/// trip over: the ontology page creates 28 attributes in one go, queuing once per attribute; after the first starts running, the other 27 no longer
/// collide with a "queued" one, so they become 28 queued copies that run serially 28 times. Tasks that use this entry point must **look back at the end
/// for new work themselves** (both aligned tasks do this), otherwise changes that arrived while it was running would be lost.
///
/// `after` is **debounce**: in a batch of edits, the first one queues a task to run a few seconds later, and every other one hits
/// this "queued" and does not queue again. Without it, on an empty database a run takes only a few hundred milliseconds, and 42 creations would still queue
/// a dozen copies: the "running" half only blocks when the task is really running.
pub async fn enqueue_unless_pending(
    pool: &PgPool,
    kind: &str,
    payload: serde_json::Value,
    after: Duration,
) -> AppResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    let row: Option<(i64,)> = sqlx::query_as(ENQUEUE_UNLESS_PENDING_SQL)
        .bind(kind)
        .bind(payload)
        .bind(after.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
    if row.is_some() {
        notify_worker_tx(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(row.map(|(id,)| id))
}

pub async fn enqueue_unless_queued(
    pool: &PgPool,
    kind: &str,
    payload: serde_json::Value,
) -> AppResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    let id = enqueue_unless_queued_tx(&mut tx, kind, payload).await?;
    tx.commit().await?;
    Ok(id)
}

/// Same as [`enqueue_unless_queued`], but queued inside the caller's transaction: the task commits and rolls back together with the other writes in the transaction.
/// When a person defines a category word, the judgment and the task aligned to its phrase are either both present or both absent: committing the judgment first and queuing after
/// means a process exit between the two steps leaves only the judgment, and nobody dedupes the signature it changed.
pub async fn enqueue_unless_queued_tx(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    payload: serde_json::Value,
) -> AppResult<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(ENQUEUE_UNLESS_QUEUED_SQL)
        .bind(kind)
        .bind(payload)
        .fetch_optional(&mut **tx)
        .await?;
    if row.is_some() {
        notify_worker_tx(tx).await?;
    }
    Ok(row.map(|(id,)| id))
}

/// Same as [`enqueue_unless_queued_tx`], but when merged into the queued copy, returns **its id** (the caller has to hand the job id out).
/// Only matches queued copies, not running ones (decision 0051: a running task may already have read the state from before this judgment).
/// The trailing `None` branch is a race: the queued copy was claimed between the check and the insert, so queue a new one honestly; tests cannot construct this.
pub async fn enqueue_queued_once_tx(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    payload: serde_json::Value,
    max_attempts: i32,
) -> AppResult<i64> {
    if !(1..=32).contains(&max_attempts) {
        return Err(utopia_core::AppError::Validation(
            "max_attempts must be between 1 and 32".into(),
        ));
    }
    let inserted: Option<i64> = sqlx::query_scalar(ENQUEUE_QUEUED_ONCE_SQL)
        .bind(kind)
        .bind(&payload)
        .bind(max_attempts)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(id) = inserted {
        notify_worker_tx(tx).await?;
        return Ok(id);
    }
    let existing: Option<i64> = sqlx::query_scalar(FIND_QUEUED_SQL)
        .bind(kind)
        .bind(&payload)
        .fetch_optional(&mut **tx)
        .await?;
    match existing {
        Some(id) => Ok(id),
        None => enqueue_with_max_attempts_tx(tx, kind, payload, max_attempts).await,
    }
}

/// Same as [`enqueue_unless_queued`], but runs later. It only blocks queued copies, not running ones: the case where the caller
/// is exactly the running task that wants to queue another one for itself later; use this rather than `enqueue_unless_pending`
pub async fn enqueue_unless_queued_after(
    pool: &PgPool,
    kind: &str,
    payload: serde_json::Value,
    after: Duration,
) -> AppResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    let row: Option<(i64,)> = sqlx::query_as(ENQUEUE_UNLESS_QUEUED_AFTER_SQL)
        .bind(kind)
        .bind(payload)
        .bind(after.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
    if row.is_some() {
        notify_worker_tx(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(row.map(|(id,)| id))
}

/// Whether this library has this kind of task queued or running (judged by kb_id in the payload, not by the whole payload:
/// the same kind of task's payload may carry an extra reask)
pub async fn pending_for_kb(pool: &PgPool, kind: &str, kb_id: Uuid) -> AppResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM jobs
                         WHERE kind = $1 AND status IN ('queued', 'running')
                           AND payload->>'kb_id' = $2)",
    )
    .bind(kind)
    .bind(kb_id.to_string())
    .fetch_one(pool)
    .await?)
}

pub async fn enqueue(pool: &PgPool, kind: &str, payload: serde_json::Value) -> AppResult<i64> {
    enqueue_with_max_attempts(pool, kind, payload, 3).await
}

/// Enqueue with an explicit, finite retry budget. Callers cannot turn an external system's task into infinite retries.
pub async fn enqueue_with_max_attempts(
    pool: &PgPool,
    kind: &str,
    payload: serde_json::Value,
    max_attempts: i32,
) -> AppResult<i64> {
    if !(1..=32).contains(&max_attempts) {
        return Err(utopia_core::AppError::Validation(
            "max_attempts must be between 1 and 32".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let id = enqueue_with_max_attempts_tx(&mut tx, kind, payload, max_attempts).await?;
    tx.commit().await?;
    Ok(id)
}

/// Shares a transaction with the ledger change; the job must not be written before the entry, otherwise a process exit in the window would leave
/// a hydration task that cannot be found from the entry.
pub async fn enqueue_with_max_attempts_tx(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    payload: serde_json::Value,
    max_attempts: i32,
) -> AppResult<i64> {
    if !(1..=32).contains(&max_attempts) {
        return Err(utopia_core::AppError::Validation(
            "max_attempts must be between 1 and 32".into(),
        ));
    }
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, max_attempts) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(kind)
    .bind(payload)
    .bind(max_attempts)
    .fetch_one(&mut **tx)
    .await?;
    notify_worker_tx(tx).await?;
    Ok(id)
}

/// Queue one extraction (`extract_document` or `extract_statements`, by the document's source) unless the same document already has one queued (RC-009).
///
/// Extraction carries only the document id and reads the current chunks when it runs, so a second queued copy reads the same as the first; match by
/// `document_id` rather than the whole payload, since the two enqueue sites have different payloads. A running one does not count: it may have finished reading before
/// the new version was written. Returns `None` = merged into the queued one.
pub async fn enqueue_extraction(
    pool: &PgPool,
    payload: serde_json::Value,
    max_attempts: i32,
) -> AppResult<Option<i64>> {
    let mut tx = pool.begin().await?;
    let id = enqueue_extraction_tx(&mut tx, payload, max_attempts).await?;
    tx.commit().await?;
    Ok(id)
}

/// Same as [`enqueue_extraction`], but queued inside the caller's transaction.
pub async fn enqueue_extraction_tx(
    tx: &mut Transaction<'_, Postgres>,
    payload: serde_json::Value,
    max_attempts: i32,
) -> AppResult<Option<i64>> {
    let kind = extraction_kind_sql("($1->>'document_id')::uuid");
    let row: Option<(i64,)> = sqlx::query_as(&format!(
        "INSERT INTO jobs (kind, payload, max_attempts)
         SELECT {kind}, $1, $2
          WHERE NOT EXISTS (SELECT 1 FROM jobs
                             WHERE kind = ANY($3) AND status = 'queued'
                               AND payload->>'document_id' = $1->>'document_id')
         RETURNING id"
    ))
    .bind(payload)
    .bind(max_attempts)
    .bind(&EXTRACTION_KINDS[..])
    .fetch_optional(&mut **tx)
    .await?;
    if row.is_some() {
        notify_worker_tx(tx).await?;
    }
    Ok(row.map(|(id,)| id))
}

/// Same as [`enqueue_with_max_attempts_tx`], but due only after `after`. It is counted from the moment of insertion
/// (`clock_timestamp()`): `now()` is the transaction start time; if the transaction is long, the commit-time delay has already used up
pub async fn enqueue_with_max_attempts_after_tx(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    payload: serde_json::Value,
    max_attempts: i32,
    after: Duration,
) -> AppResult<i64> {
    if !(1..=32).contains(&max_attempts) {
        return Err(utopia_core::AppError::Validation(
            "max_attempts must be between 1 and 32".into(),
        ));
    }
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, max_attempts, run_at)
         VALUES ($1, $2, $3, clock_timestamp() + make_interval(secs => $4))
         RETURNING id",
    )
    .bind(kind)
    .bind(payload)
    .bind(max_attempts)
    .bind(after.as_secs_f64())
    .fetch_one(&mut **tx)
    .await?;
    notify_worker_tx(tx).await?;
    Ok(id)
}

/// Wake idle workers only after the transaction containing the job is committed.
/// PostgreSQL delivers `NOTIFY` at commit, so a worker can never wake up before
/// the row it needs to claim is visible.
pub(crate) async fn notify_worker_tx(tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    sqlx::query("SELECT pg_notify($1, '')")
        .bind(JOB_CHANNEL)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Idle polling interval. Not needed when notifications arrive; it covers retries whose `run_at` is in the future,
/// notifications lost during reconnection, and those a few workers were woken for by the same notification but did not win (#517).
pub const IDLE_POLL: Duration = Duration::from_secs(2);

/// How an idle wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// Someone enqueued and the notification arrived
    Notified,
    /// A full poll interval passed and nothing came
    Polled,
}

/// Set up the wake-up listener. `None` = could not be set up; the worker relies on polling alone, behaving as if there were no notifications.
///
/// Once this connection is taken from the pool it is **held the whole time**: LISTEN is connection-level state, and returning it loses it.
/// The pool cap (`db.rs`) is one fewer for others from then on; jobs runs only one worker, so only this one is lost.
pub async fn listen_for_jobs(pool: &PgPool) -> Option<PgListener> {
    let mut listener = match PgListener::connect_with(pool).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(error = %e, "jobs worker failed to set up notification listener, falling back to polling");
            return None;
        }
    };
    if let Err(e) = listener.listen(JOB_CHANNEL).await {
        tracing::error!(error = %e, "jobs worker failed to listen for notifications, falling back to polling");
        return None;
    }
    Some(listener)
}

/// Idle wait: wake up if a notification arrives first, otherwise wait for the full `poll`.
///
/// A listening error must not immediately claim again: if the database can claim but the listen connection cannot reconnect (a full pool is one such case),
/// the loop would become a tight loop of "claim once, warn one line". On error, sleep for one poll cycle;
/// the next `recv` reconnects by itself.
pub async fn wait_for_work(listener: Option<&mut PgListener>, poll: Duration) -> Wake {
    let Some(listener) = listener else {
        tokio::time::sleep(poll).await;
        return Wake::Polled;
    };
    match tokio::time::timeout(poll, listener.recv()).await {
        Ok(Ok(_)) => Wake::Notified,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "jobs worker notification listen failed, continuing to poll");
            tokio::time::sleep(poll).await;
            Wake::Polled
        }
        Err(_) => Wake::Polled,
    }
}

/// Scope for requeueing failed tasks (#216). All three conditions are nullable; empty = unlimited.
///
/// **Per-library scoping resolves the payload**: the task table has no kb column; a payload carries only one of `document_id` /
/// `source_id` / `kb_id`, each resolved to its library. System tasks without a library are only touched when the library is unlimited.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequeueScope<'a> {
    pub kb_id: Option<Uuid>,
    pub kind: Option<&'a str>,
    /// Only enqueue those that failed after this moment: the "run again" scope on the alert is exactly the failure window
    pub failed_since: Option<DateTime<Utc>>,
}

/// The library-scope SQL predicate; `$N` is the library id; `requeue_failed` and `failed_count` share
const KB_SCOPE: &str = "(
       (j.payload ? 'kb_id' AND j.payload->>'kb_id' = $KB::text)
    OR (j.payload ? 'document_id' AND EXISTS (
            SELECT 1 FROM documents d
             WHERE d.id::text = j.payload->>'document_id' AND d.kb_id = $KB))
    OR (j.payload ? 'source_id' AND EXISTS (
            SELECT 1 FROM sources s
             WHERE s.id::text = j.payload->>'source_id' AND s.kb_id = $KB)))";

/// Put the failed tasks in scope back on the queue: `attempts` reset to zero, due immediately.
///
/// RSS hydration is excluded: its source admission policy owns retries.
/// Other handlers are all idempotent (this is what makes reclaiming orphans at startup safe);
/// before this, `failed` was a terminal state: when credits ran out, a batch of documents all failed, and after topping up you could only click each one or re-extract the whole source
pub async fn requeue_failed(pool: &PgPool, scope: RequeueScope<'_>) -> AppResult<u64> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "UPDATE jobs j
            SET status = 'queued', attempts = 0, run_at = now(), updated_at = now()
          WHERE j.status = 'failed' AND j.kind <> 'hydrate_rss_entry'
            AND ($1::text IS NULL OR j.kind = $1)
            AND ($2::timestamptz IS NULL OR j.updated_at >= $2)
            AND ($3::uuid IS NULL OR {})",
        KB_SCOPE.replace("$KB", "$3")
    );
    let res = sqlx::query(&sql)
        .bind(scope.kind)
        .bind(scope.failed_since)
        .bind(scope.kb_id)
        .execute(&mut *tx)
        .await?;
    let count = res.rows_affected();
    if count > 0 {
        notify_worker_tx(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(count)
}

/// Count of failed items in scope: the settings page's line "N failed tasks"
pub async fn failed_count(pool: &PgPool, kb_id: Option<Uuid>) -> AppResult<i64> {
    let sql = format!(
        "SELECT count(*) FROM jobs j
          WHERE j.status = 'failed' AND ($1::uuid IS NULL OR {})",
        KB_SCOPE.replace("$KB", "$1")
    );
    Ok(sqlx::query_scalar(&sql).bind(kb_id).fetch_one(pool).await?)
}

/// What one task looks like right now, for the question "did the thing I just queued finish?" (0051).
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct JobStatus {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    pub run_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Read a task by id, **and only if it belongs to this library**. Authorization follows the library: whoever can see the library can see
/// its tasks; a task id from another library, even if guessed correctly, returns only None, and answers 404 just like an invisible document.
pub async fn status_in_kb(pool: &PgPool, kb_id: Uuid, id: i64) -> AppResult<Option<JobStatus>> {
    let sql = format!(
        "SELECT j.id, j.kind, j.status, j.attempts, j.max_attempts, j.last_error, j.run_at, j.updated_at
           FROM jobs j WHERE j.id = $1 AND {}",
        KB_SCOPE.replace("$KB", "$2")
    );
    Ok(sqlx::query_as(&sql)
        .bind(id)
        .bind(kb_id)
        .fetch_optional(pool)
        .await?)
}

/// Claim one due task; return None if there is none.
///
/// Tasks that do not call the chat model are queued first (`job_priority`, migration 0116, #1300): synchronous and document-ingest work, a few seconds
/// of work, should not wait behind hundreds of earlier extractions. `no_chat_only` claims only these, for the reserved lane
async fn claim_one(pool: &PgPool, no_chat_only: bool) -> AppResult<Option<Job>> {
    // Written as a literal rather than a bound parameter: this makes it an index condition, so the scan stops at the end of priority 0; a bound parameter can only
    // filter row by row, and when model tasks pile up to tens of thousands, after the regular slots fill up it would scan them all every 200ms
    let only = if no_chat_only {
        "AND job_priority(kind) = 0"
    } else {
        ""
    };
    let job = sqlx::query_as(&format!(
        "UPDATE jobs SET status = 'running', locked_at = now(),
                attempts = attempts + 1, updated_at = now()
         WHERE id = (
             SELECT id FROM jobs
             WHERE status = 'queued' AND run_at <= now() {only}
             ORDER BY job_priority(kind), run_at
             FOR UPDATE SKIP LOCKED
             LIMIT 1
         )
         RETURNING id, kind, payload, attempts, max_attempts, locked_at"
    ))
    .fetch_optional(pool)
    .await?;
    Ok(job)
}

/// How long to wait before the next retry; `None` = stop here.
///
/// Two reasons to stop here: **the attempts are used up**, or **the handler says this will not get better by retrying**
/// (`utopia_core::Terminal`, see issue #195). The latter did not exist before, so a task whose credits ran out
/// still ran through three backoffs: credits do not regrow in seven minutes, so those three retries just repeated the same
/// error three times and delayed the "failed" that operators should see by seven minutes.
///
/// Rate limiting is the opposite: it is exactly what this backoff serves, since quotas recover by themselves. #176 split the two classes precisely so
/// they could take separate paths, but the retry policy had not caught up at the time.
///
/// Made a pure function so it can be tested: the decision is made here, and the database write only records it.
fn retry_delay(attempts: i32, max_attempts: i32, terminal: bool) -> Option<i64> {
    if terminal || attempts >= max_attempts {
        return None;
    }
    Some(30i64 * i64::from(attempts) * i64::from(attempts))
}

/// The wait in seconds before the next retry when a handler returns `Deferred { retry_in }`. **Independent of the failure count**:
/// this is the key difference between `Deferred` and the default backoff: the default `30s × attempts²` grows as "try once more, maybe it will be better",
/// whereas `Deferred` is "the conditions are not met now, try again after `retry_in`", regardless of which attempt this is.
/// The same wait condition requeued twice gets the same `run_at`
/// offset, with no growing 60s, 120s (#526).
///
/// The longest a task may keep waiting in a row (see `mark_failed`). One hour is enough for a large ontology to be backfilled on a remote embedding model;
/// if it is still not done after that, most likely the backfill job itself is failing, and this extraction should be treated as failed so a person sees it
pub const DEFER_WINDOW_SECS: i64 = 60 * 60;

fn deferred_retry_secs(retry_in: std::time::Duration) -> i64 {
    // Truncate to seconds: the underlying `run_at` is timestamptz, which cannot store sub-second precision, and tens of milliseconds are not worth
    // a float conversion. Round up: waiting one second longer is better than running one second early; the former is harmless, the latter would knock the still-running
    // embedding job back into its lock.
    let secs = retry_in.as_secs_f64().ceil();
    if !secs.is_finite() || secs < 1.0 {
        1
    } else {
        secs as i64
    }
}

enum JobOutcome {
    Done,
    Failed {
        error: String,
        backoff_secs: Option<i64>,
        deferred_secs: Option<i64>,
    },
}

impl JobOutcome {
    fn failed(job: &Job, err: &anyhow::Error) -> Self {
        let terminal = utopia_core::is_terminal(err);
        // Terminal first; result write-back retries use the same classification and the full original error text.
        let deferred_secs = if terminal {
            None
        } else {
            utopia_core::is_deferred(err).map(deferred_retry_secs)
        };
        Self::Failed {
            error: format!("{err:#}"),
            backoff_secs: retry_delay(job.attempts, job.max_attempts, terminal),
            deferred_secs,
        }
    }
}

/// true = this write; false = already finished or claim invalid. Either way, this write-back ends.
async fn persist_outcome(
    pool: &PgPool,
    job: &Job,
    outcome: &JobOutcome,
    completed_at: DateTime<Utc>,
) -> AppResult<bool> {
    let JobOutcome::Failed {
        error,
        backoff_secs,
        deferred_secs,
    } = outcome
    else {
        // On success, clear the previous error, so a task showing done does not still carry a failure reason.
        let res = sqlx::query(
            "UPDATE jobs SET status = 'done', last_error = NULL, updated_at = now()
             WHERE id = $1 AND status = 'running' AND locked_at = $2",
        )
        .bind(job.id)
        .bind(job.locked_at)
        .execute(pool)
        .await?;
        return Ok(res.rows_affected() > 0);
    };

    if let Some(secs) = deferred_secs {
        // Deferred gives back the attempts consumed by claiming; only once the wait window is exhausted does the normal failure budget apply.
        // The wait window and next run time are fixed when the handler finishes, and do not slide with write-back retries.
        let res = sqlx::query(
            "UPDATE jobs SET status = 'queued', last_error = $2,
                    attempts = GREATEST(0, attempts - 1),
                    run_at = $4::timestamptz + make_interval(secs => $5::float8),
                    payload = payload || jsonb_build_object('deferred_since',
                        COALESCE(payload->>'deferred_since', $4::timestamptz::text)),
                    updated_at = now()
             WHERE id = $1 AND status = 'running' AND locked_at = $3
               AND COALESCE((payload->>'deferred_since')::timestamptz, $4)
                   > $4 - make_interval(secs => $6::float8)",
        )
        .bind(job.id)
        .bind(error)
        .bind(job.locked_at)
        .bind(completed_at)
        .bind(*secs as f64)
        .bind(DEFER_WINDOW_SECS as f64)
        .execute(pool)
        .await?;
        if res.rows_affected() > 0 {
            return Ok(true);
        }
        // Zero rows may also mean a stale claim; the normal failure branch keeps the same guard and must not overwrite a new execution.
    }

    let status = if backoff_secs.is_some() {
        "queued"
    } else {
        "failed"
    };
    let res = sqlx::query(
        "UPDATE jobs SET status = $3, last_error = $4,
                run_at = CASE WHEN $5::float8 IS NULL THEN run_at
                    ELSE $6::timestamptz + make_interval(secs => $5::float8) END,
                updated_at = now()
         WHERE id = $1 AND status = 'running' AND locked_at = $2",
    )
    .bind(job.id)
    .bind(job.locked_at)
    .bind(status)
    .bind(error)
    .bind(backoff_secs.map(|secs| secs as f64))
    .bind(completed_at)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// The single write-back entry point for integration tests; in the worker, persist_with_retry writes back after the result is saved.
#[doc(hidden)]
pub async fn mark_failed(pool: &PgPool, job: &Job, err: &anyhow::Error) -> AppResult<()> {
    persist_outcome(pool, job, &JobOutcome::failed(job, err), Utc::now()).await?;
    Ok(())
}

async fn persist_with_retry(
    pool: &PgPool,
    job: &Job,
    outcome: &JobOutcome,
    completed_at: DateTime<Utc>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        if pool.is_closed() {
            tracing::warn!(
                job_id = job.id,
                "数据库连接池已关闭，未确认的任务留待启动回收"
            );
            return;
        }
        let result = tokio::select! {
            _ = pool.close_event() => {
                tracing::warn!(job_id = job.id, "database connection pool closed, unacknowledged jobs left for startup recovery");
                return;
            }
            result = persist_outcome(pool, job, outcome, completed_at) => result,
        };
        match result {
            Ok(updated) => {
                if !updated {
                    tracing::debug!(
                        job_id = job.id,
                        claimed_at = %job.locked_at,
                        "任务已结束或领取已失效，停止写回旧结果"
                    );
                }
                return;
            }
            Err(e) => {
                tracing::error!(job_id = job.id, error = %e, retry_in_secs = delay.as_secs(), "failed to write back job status, will retry the same result");
            }
        }
        tokio::select! {
            _ = pool.close_event() => {}
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// Retention period for completed tasks. Previously done rows were never deleted: the internal cluster accumulated 2.5 million rows, 924 MB,
/// and every jobs query was paying for them. failed rows are not deleted, because `requeue_failed` reads them.
const DONE_JOB_RETENTION: Duration = Duration::from_secs(14 * 24 * 3600);

/// Interval for cleaning up completed tasks.
const DONE_JOB_PRUNE_EVERY: Duration = Duration::from_secs(3600);

/// Maximum rows deleted per DELETE statement: each batch is its own short statement, no long transaction.
const PRUNE_BATCH: i64 = 5000;

/// Delete done tasks whose `updated_at` is older than the retention period; return the total deleted.
pub async fn prune_done(pool: &PgPool, older_than: Duration) -> AppResult<u64> {
    let cutoff = Utc::now() - older_than;
    let mut total = 0u64;
    loop {
        let deleted = sqlx::query(
            "DELETE FROM jobs WHERE id IN (
                 SELECT id FROM jobs WHERE status = 'done' AND updated_at < $1
                 ORDER BY id LIMIT $2)",
        )
        .bind(cutoff)
        .bind(PRUNE_BATCH)
        .execute(pool)
        .await?
        .rows_affected();
        total += deleted;
        if deleted < PRUNE_BATCH as u64 {
            return Ok(total);
        }
        tokio::task::yield_now().await;
    }
}

/// After the regular slots are fully occupied, how many more tasks that do not call the chat model can still be claimed (#1300).
///
/// Sorting alone is not enough: extraction holds a worker slot even while waiting for model permission. On a local model the slots are often filled by
/// extractions waiting for permission, so the highest-priority synchronous task can only wait until one extraction finishes. These reserved slots are for tasks with `job_priority` 0 only,
/// so `worker_concurrency` governs everything else, plus at most two more running at once
pub const NO_CHAT_LANE: usize = 2;

/// A container's memory as the orchestrator judges it: the working set against the limit it is killed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct MemoryUse {
    pub working_set_bytes: u64,
    pub limit_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom {
    pub free_connections: u32,
    pub memory: Option<MemoryUse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    Connections { free: u32 },
    Memory(MemoryUse),
}

const MIN_FREE_CONNECTIONS: u32 = 2;
const MAX_MEMORY_PERCENT: u128 = 85;

/// Whether the worker may claim another job: the pool must keep connections free for the API and memory must stay under the limit's share.
pub fn may_claim(headroom: Headroom) -> Result<(), PauseReason> {
    if headroom.free_connections < MIN_FREE_CONNECTIONS {
        return Err(PauseReason::Connections {
            free: headroom.free_connections,
        });
    }
    match headroom.memory {
        Some(memory)
            if memory.limit_bytes > 0
                && u128::from(memory.working_set_bytes) * 100
                    > u128::from(memory.limit_bytes) * MAX_MEMORY_PERCENT =>
        {
            Err(PauseReason::Memory(memory))
        }
        _ => Ok(()),
    }
}

/// A memory shortage is no reason to wait when nothing of ours is running: the memory is held elsewhere, and waiting would never end.
fn pause_for(shortage: Option<PauseReason>, jobs_running: usize) -> Option<PauseReason> {
    match shortage {
        Some(PauseReason::Memory(_)) if jobs_running == 0 => None,
        other => other,
    }
}

/// Connections the pool could still hand out: the ones idle plus the ones it has not opened yet.
pub fn free_connections(pool: &PgPool) -> u32 {
    pool.options()
        .get_max_connections()
        .saturating_sub(pool.size())
        .saturating_add(pool.num_idle() as u32)
}

/// Worker scheduling loop: while the running task count is below the target concurrency, keep claiming (if work exists, dispatch again immediately),
/// when idle wait for a queue notification, with a 2s poll as backup (#517); each task runs in its own tokio task, so long extractions no longer block synchronous work.
/// When the regular slots are full, [`NO_CHAT_LANE`] further slots claim only tasks that do not call the chat model.
/// Before every claim [`may_claim`] checks free connections and memory; while either is short the worker pauses, for the regular slots and the reserved lane alike.
/// `concurrency` is hot-read every round: changing the concurrency count in system settings takes effect immediately.
/// Task dispatch logic is injected by the caller as a handler (the store does not depend on upper-layer crates).
pub async fn run_worker<M, F, Fut>(
    pool: PgPool,
    concurrency: Arc<AtomicUsize>,
    memory_use: M,
    handler: F,
) where
    M: Fn() -> Option<MemoryUse> + Send + 'static,
    F: Fn(Job) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let running = Arc::new(AtomicUsize::new(0));
    let in_lane = Arc::new(AtomicUsize::new(0));
    // Orphan reclaim: when the process is killed, running tasks have no one to clean up, so documents stay stuck in extracting forever.
    // In a single-process deployment, any task still marked running at startup must be an orphan: requeue all of them (handlers are all idempotent).
    match sqlx::query(
        "UPDATE jobs SET status = 'queued', locked_at = NULL, updated_at = now()
         WHERE status = 'running'",
    )
    .execute(&pool)
    .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::warn!(
                count = r.rows_affected(),
                "回收孤儿任务（上次进程退出时正在运行）"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "failed to reclaim orphaned jobs"),
    }
    tracing::info!(
        concurrency = concurrency.load(Ordering::Relaxed),
        "jobs worker 已启动"
    );
    let mut listener = listen_for_jobs(&pool).await;
    let mut last_prune: Option<std::time::Instant> = None;
    let mut paused: Option<PauseReason> = None;
    loop {
        if pool.is_closed() {
            return;
        }
        if last_prune.is_none_or(|at| at.elapsed() >= DONE_JOB_PRUNE_EVERY) {
            last_prune = Some(std::time::Instant::now());
            let pool = pool.clone();
            tokio::spawn(async move {
                match prune_done(&pool, DONE_JOB_RETENTION).await {
                    Ok(0) => {}
                    Ok(count) => tracing::info!(count, "cleaning up old completed jobs"),
                    Err(e) => tracing::error!(error = %e, "failed to clean up old completed jobs"),
                }
            });
        }
        let shortage = pause_for(
            may_claim(Headroom {
                free_connections: free_connections(&pool),
                memory: memory_use(),
            })
            .err(),
            running.load(Ordering::Relaxed) + in_lane.load(Ordering::Relaxed),
        );
        match (paused, shortage) {
            (None, Some(PauseReason::Connections { free })) => {
                tracing::warn!(
                    free,
                    "job claiming paused: too few free database connections"
                )
            }
            (None, Some(PauseReason::Memory(memory))) => tracing::warn!(
                working_set_bytes = memory.working_set_bytes,
                limit_bytes = memory.limit_bytes,
                "job claiming paused: memory use is close to the limit"
            ),
            (Some(_), None) => tracing::info!("job claiming resumed"),
            _ => {}
        }
        paused = shortage;
        if shortage.is_some() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        let cap = concurrency.load(Ordering::Relaxed).max(1);
        // When the regular slots are full, claim only from the reserved lane, and stop only when that lane is full too
        let lane = running.load(Ordering::Relaxed) >= cap;
        if lane && in_lane.load(Ordering::Relaxed) >= NO_CHAT_LANE {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        match claim_one(&pool, lane).await {
            Ok(Some(job)) => {
                // Claimed from whichever side's slot; give it back to that same side when done.
                let running = if lane { &in_lane } else { &running }.clone();
                running.fetch_add(1, Ordering::Relaxed);
                let pool = pool.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    // **Handler panics must also be cleaned up.** If we call `handler(job).await` directly,
                    // a panic would take down this spawned future with it: `mark_failed`
                    // would not run (the task row stays running forever, no error and no retry), and `running`
                    // would not decrement either (each panic permanently costs one concurrency slot, and after enough of them the cap is reached and
                    // the whole queue no longer claims any task). Wrapping it in a spawn turns a panic into a
                    // JoinError that comes back, so both of those are still handled.
                    let inner = {
                        let (handler, job) = (handler.clone(), job.clone());
                        tokio::spawn(async move { handler(job).await })
                    };
                    let result = match inner.await {
                        Ok(r) => r,
                        Err(join) => Err(anyhow::anyhow!(
                            "job handler panicked (see stderr for details): {join}"
                        )),
                    };
                    let completed_at = Utc::now();
                    let outcome = match result {
                        Ok(()) => JobOutcome::Done,
                        Err(e) => {
                            tracing::warn!(job_id = job.id, kind = %job.kind, error = %e, "job execution failed");
                            JobOutcome::failed(&job, &e)
                        }
                    };
                    persist_with_retry(&pool, &job, &outcome, completed_at).await;
                    running.fetch_sub(1, Ordering::Relaxed);
                });
            }
            // Regular slots are full: one may free up at any moment, so check again in 200ms as before
            Ok(None) if lane => tokio::time::sleep(Duration::from_millis(200)).await,
            Ok(None) => {
                wait_for_work(listener.as_mut(), IDLE_POLL).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to claim job, retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "jobs_writeback_tests.rs"]
mod writeback_tests;

#[cfg(test)]
#[path = "jobs_claim_tests.rs"]
mod claim_tests;

#[cfg(test)]
#[path = "jobs_plan_tests.rs"]
mod plan_tests;

#[cfg(test)]
#[path = "jobs_headroom_tests.rs"]
mod headroom_tests;

#[cfg(test)]
mod tests {
    use super::{deferred_retry_secs, retry_delay};
    use std::time::Duration;

    /// Backoff unchanged: 30s, 120s, 270s, give up after the third.
    #[test]
    fn an_ordinary_failure_backs_off_and_then_gives_up() {
        assert_eq!(retry_delay(1, 3, false), Some(30));
        assert_eq!(retry_delay(2, 3, false), Some(120));
        assert_eq!(retry_delay(3, 3, false), None);
    }

    /// Marked as hopeless: **stop after the first attempt**; those three backoffs add up to seven minutes,
    /// and credits do not regrow by themselves in seven minutes (#195).
    #[test]
    fn a_terminal_failure_does_not_spend_the_budget() {
        assert_eq!(retry_delay(1, 3, true), None);
    }

    /// `Deferred` does not grow with the failure count: the same wait condition queued twice gets the same `run_at`
    /// offset, with no 60s, 120s growth (#526).
    #[test]
    fn deferred_retry_is_independent_of_attempts() {
        assert_eq!(
            deferred_retry_secs(Duration::from_secs(30)),
            deferred_retry_secs(Duration::from_secs(30))
        );
        // Truncate to seconds, rounding up: 29.5s becomes 30s
        assert_eq!(deferred_retry_secs(Duration::from_millis(29_500)), 30);
        // 0, negative, and NaN all get a 1s floor: wait at least one second; anything shorter than the poll interval for an immediate retry is wasteful
        assert_eq!(deferred_retry_secs(Duration::ZERO), 1);
        assert_eq!(deferred_retry_secs(Duration::from_nanos(500)), 1);
    }
}
