//! Claim order (#1300, #1351): tasks that do not call the chat model go ahead of model tasks, even when they arrive later.
//!
//! `claim_one` claims tasks across the whole database, so these two are set a hundred years in the past, ahead of everything else in it.

use super::claim_one;
use sqlx::postgres::PgPoolOptions;
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

/// A pool of one connection: the temp table only shadows public.jobs on this connection, so other tasks in the database cannot reach it.
/// Each row is (label, kind, run_at relative to now).
async fn isolated_queue(rows: &[(&str, &str, &str)]) -> anyhow::Result<Option<PgPool>> {
    let Some(url) = crate::test_db::url() else {
        return Ok(None);
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    sqlx::raw_sql("CREATE TEMP TABLE jobs (LIKE public.jobs INCLUDING ALL)")
        .execute(&pool)
        .await?;
    for (label, kind, offset) in rows {
        sqlx::query(
            "INSERT INTO jobs (kind, payload, run_at)
             VALUES ($1, jsonb_build_object('label', $2::text), now() + $3::interval)",
        )
        .bind(kind)
        .bind(label)
        .bind(offset)
        .execute(&pool)
        .await?;
    }
    Ok(Some(pool))
}

/// Claims until the queue is empty, returning labels in claim order.
async fn claimed_labels(pool: &PgPool, fast_lane: bool) -> anyhow::Result<Vec<String>> {
    let mut labels = Vec::new();
    while let Some(job) = claim_one(pool, fast_lane).await? {
        labels.push(
            sqlx::query_scalar("SELECT payload->>'label' FROM jobs WHERE id = $1")
                .bind(job.id)
                .fetch_one(pool)
                .await?,
        );
    }
    Ok(labels)
}

#[tokio::test]
async fn a_later_time_resolution_is_claimed_before_an_earlier_extraction() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("extraction", "extract_document", "-2 hours"),
        ("resolution", "resolve_time", "-1 hour"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(
        claimed_labels(&pool, false).await?,
        ["resolution", "extraction"]
    );
    Ok(())
}

#[tokio::test]
async fn a_later_retention_prune_is_claimed_before_an_earlier_extraction() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("extraction", "extract_document", "-2 hours"),
        ("prune", "prune_superseded", "-1 hour"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(claimed_labels(&pool, false).await?, ["prune", "extraction"]);
    Ok(())
}

#[tokio::test]
async fn a_later_intake_job_is_claimed_before_an_earlier_time_resolution() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("resolution", "resolve_time", "-2 hours"),
        ("intake", "sync_source", "-1 hour"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(
        claimed_labels(&pool, false).await?,
        ["intake", "resolution"]
    );
    Ok(())
}

#[tokio::test]
async fn the_fast_lane_leaves_a_lone_time_resolution_queued() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[("resolution", "resolve_time", "-1 hour")]).await? else {
        return Ok(());
    };
    assert_eq!(claimed_labels(&pool, true).await?, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn a_time_resolution_due_later_is_not_claimed_while_an_extraction_is_due(
) -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("extraction", "extract_document", "-1 hour"),
        ("resolution", "resolve_time", "1 hour"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(claimed_labels(&pool, false).await?, ["extraction"]);
    Ok(())
}

#[tokio::test]
async fn the_earlier_of_two_time_resolutions_is_claimed_first() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("later", "resolve_time", "-1 hour"),
        ("earlier", "resolve_time", "-2 hours"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(claimed_labels(&pool, false).await?, ["earlier", "later"]);
    Ok(())
}

#[tokio::test]
async fn other_model_work_keeps_run_at_order_beside_extraction() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("extraction", "extract_document", "-1 hour"),
        ("phrases", "align_phrases", "-3 hours"),
        ("govern", "govern", "-2 hours"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(
        claimed_labels(&pool, false).await?,
        ["phrases", "govern", "extraction"]
    );
    Ok(())
}

#[tokio::test]
async fn a_later_pushed_statement_extraction_is_claimed_before_an_earlier_model_extraction(
) -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("extraction", "extract_document", "-2 hours"),
        ("statements", "extract_statements", "-1 hour"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(
        claimed_labels(&pool, false).await?,
        ["statements", "extraction"]
    );
    Ok(())
}

#[tokio::test]
async fn a_later_intake_job_is_claimed_before_an_earlier_pushed_statement_extraction(
) -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[
        ("statements", "extract_statements", "-2 hours"),
        ("intake", "sync_source", "-1 hour"),
    ])
    .await?
    else {
        return Ok(());
    };
    assert_eq!(
        claimed_labels(&pool, false).await?,
        ["intake", "statements"]
    );
    Ok(())
}

#[tokio::test]
async fn the_fast_lane_leaves_a_lone_pushed_statement_extraction_queued() -> anyhow::Result<()> {
    let Some(pool) = isolated_queue(&[("statements", "extract_statements", "-1 hour")]).await?
    else {
        return Ok(());
    };
    assert_eq!(claimed_labels(&pool, true).await?, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn extract_statements_has_priority_1_and_extract_document_stays_at_2() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let priorities: Vec<i16> = sqlx::query_scalar(
        "SELECT job_priority(kind) FROM unnest(ARRAY['extract_statements', 'extract_document']) AS kind",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(priorities, [1, 2]);
    Ok(())
}

#[tokio::test]
async fn reapplying_the_time_resolution_migration_keeps_the_priorities() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let mut conn = PgConnection::connect(&url).await?;
    let mut tx = conn.begin().await?;
    let migration = include_str!(
        "../../../migrations/9016_a_time_resolution_is_claimed_before_new_extraction.sql"
    );
    sqlx::raw_sql(migration).execute(&mut *tx).await?;
    sqlx::raw_sql(migration).execute(&mut *tx).await?;
    let priorities: Vec<i16> = sqlx::query_scalar(
        "SELECT job_priority(kind) FROM unnest(ARRAY['sync_source', 'resolve_time', 'extract_document']) AS kind",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    assert_eq!(priorities, [0, 1, 2]);
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
        // The old function and old index are loaded only into this schema, so the queue used by the concurrency tests is not changed.
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
        // Old data was committed before the upgrade; the new process running the migration has no inlined plan cache for the old function.
        tx.commit().await?;
        let mut upgrade = PgConnection::connect(&url).await?;
        sqlx::raw_sql(&format!(
            "SET search_path TO {schema}; {}",
            include_str!("../../../migrations/0123_human_phrase_materialization_is_intake.sql")
        ))
        .execute(&mut upgrade)
        .await?;
        upgrade.close().await?;

        // Claims are run by the upgraded new worker connection, directly verifying the index key of the existing queue.
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

/// Seeds `count` tasks of this kind by status and age (SQL interval), prunes once, returns
/// (prune's return value, remaining row count), and deletes every row of this kind.
async fn prune_seeded(
    kind_prefix: &str,
    status: &str,
    age: &str,
    count: i64,
) -> anyhow::Result<(u64, i64)> {
    // prune_done sweeps every old done row of the shared jobs table, so two of these
    // running together delete each other's rows and can deadlock on batch row locks.
    static ONE_PRUNE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _only_prune_running = ONE_PRUNE_AT_A_TIME.lock().await;
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

/// First insert an existing task, then `enqueue_unless_pending` the same kind with `{"kb": "a"}`;
/// returns whether the new task got queued.
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

/// First insert a queued existing task, then call `enqueue_unless_queued` for the same kind;
/// returns (new task's id, number of queued tasks with the same payload as the new task).
async fn enqueue_unless_queued_over(
    kind: &str,
    existing: serde_json::Value,
    payload: serde_json::Value,
) -> anyhow::Result<(Option<i64>, i64)> {
    let url = crate::test_db::url().expect("db url checked by caller");
    let pool = PgPool::connect(&url).await?;
    let existing_id: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, payload, status) VALUES ($1, $2, 'queued') RETURNING id",
    )
    .bind(kind)
    .bind(existing)
    .fetch_one(&pool)
    .await?;
    let enqueued = super::enqueue_unless_queued(&pool, kind, payload.clone()).await;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs
          WHERE kind = $1 AND status = 'queued' AND payload - 'deferred_since' = $2",
    )
    .bind(kind)
    .bind(&payload)
    .fetch_one(&pool)
    .await?;
    sqlx::query("DELETE FROM jobs WHERE id = $1 OR id = $2")
        .bind(existing_id)
        .bind(enqueued.as_ref().ok().copied().flatten())
        .execute(&pool)
        .await?;
    Ok((enqueued?, queued))
}

#[tokio::test]
async fn a_materialize_enqueue_is_absorbed_by_a_deferred_job_queued_for_the_same_base(
) -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kb_id = fresh_base();
    let deferred = serde_json::json!({"kb_id": kb_id, "deferred_since": DEFERRED_SINCE});
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let (_, queued) =
        enqueue_unless_queued_over(kind, deferred, serde_json::json!({"kb_id": kb_id})).await?;
    assert_eq!(queued, 1);
    Ok(())
}

const DEFERRED_SINCE: &str = "2026-11-25T10:00:00+00:00";

#[derive(Debug, PartialEq)]
enum Enqueued {
    /// returned the id of the existing task
    Absorbed,
    /// returned a new task of this database itself
    New,
    /// returned someone else's task — a materialize task queued by another database in the shared database
    Stranger,
}

/// First insert an existing task (payload built from the database id), then call `enqueue_queued_once_tx` with `{"kb_id": base}`,
/// and see which task it got.
async fn queued_once_over(
    existing_kind: &str,
    existing_status: &str,
    existing_payload: impl FnOnce(&str) -> serde_json::Value,
) -> anyhow::Result<Enqueued> {
    let url = crate::test_db::url().expect("db url checked by caller");
    let pool = PgPool::connect(&url).await?;
    let base = fresh_base();
    let existing_id: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, payload, status) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(existing_kind)
    .bind(existing_payload(&base))
    .bind(existing_status)
    .fetch_one(&pool)
    .await?;
    let mut tx = pool.begin().await?;
    let returned_id = super::enqueue_queued_once_tx(
        &mut tx,
        crate::phrase_bindings::MATERIALIZE_KIND,
        serde_json::json!({ "kb_id": base }),
        3,
    )
    .await;
    tx.commit().await?;
    let returned_base: Option<String> =
        sqlx::query_scalar("SELECT payload->>'kb_id' FROM jobs WHERE id = $1")
            .bind(returned_id.as_ref().ok())
            .fetch_optional(&pool)
            .await?;
    sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id' = $1 OR id = $2")
        .bind(&base)
        .bind(existing_id)
        .execute(&pool)
        .await?;
    Ok(if returned_id? == existing_id {
        Enqueued::Absorbed
    } else if returned_base.as_deref() == Some(&base) {
        Enqueued::New
    } else {
        Enqueued::Stranger
    })
}

fn fresh_base() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn same_base(base: &str) -> serde_json::Value {
    serde_json::json!({"kb_id": base})
}

#[tokio::test]
async fn a_queued_once_enqueue_returns_the_id_of_the_queued_job_of_the_same_base(
) -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let outcome = queued_once_over(kind, "queued", same_base).await?;
    assert_eq!(outcome, Enqueued::Absorbed);
    Ok(())
}

#[tokio::test]
async fn a_queued_once_enqueue_returns_the_id_of_a_deferred_queued_job_of_the_same_base(
) -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let deferred =
        |base: &str| serde_json::json!({"kb_id": base, "deferred_since": DEFERRED_SINCE});
    let outcome = queued_once_over(kind, "queued", deferred).await?;
    assert_eq!(outcome, Enqueued::Absorbed);
    Ok(())
}

#[tokio::test]
async fn a_running_materialize_job_does_not_absorb_a_queued_once_enqueue() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let outcome = queued_once_over(kind, "running", same_base).await?;
    assert_eq!(outcome, Enqueued::New);
    Ok(())
}

#[tokio::test]
async fn a_done_materialize_job_does_not_absorb_a_queued_once_enqueue() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let outcome = queued_once_over(kind, "done", same_base).await?;
    assert_eq!(outcome, Enqueued::New);
    Ok(())
}

#[tokio::test]
async fn a_failed_materialize_job_does_not_absorb_a_queued_once_enqueue() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let outcome = queued_once_over(kind, "failed", same_base).await?;
    assert_eq!(outcome, Enqueued::New);
    Ok(())
}

#[tokio::test]
async fn a_queued_materialize_job_of_another_base_does_not_absorb_a_queued_once_enqueue(
) -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let outcome = queued_once_over(kind, "queued", |_| same_base(&fresh_base())).await?;
    assert_eq!(outcome, Enqueued::New);
    Ok(())
}

#[tokio::test]
async fn a_queued_job_of_another_kind_for_the_same_base_does_not_absorb_a_queued_once_enqueue(
) -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let outcome = queued_once_over("materialize_inferences", "queued", same_base).await?;
    assert_eq!(outcome, Enqueued::New);
    Ok(())
}

#[tokio::test]
async fn a_deferred_job_of_another_kind_absorbs_an_enqueue_unless_queued() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kind = format!("enqueue_dedupe_{}", uuid::Uuid::now_v7().simple());
    let deferred = serde_json::json!({"kb": "a", "deferred_since": DEFERRED_SINCE});
    let (enqueued, _) =
        enqueue_unless_queued_over(&kind, deferred, serde_json::json!({"kb": "a"})).await?;
    assert_eq!(enqueued, None);
    Ok(())
}

#[tokio::test]
async fn migration_9015_keeps_the_oldest_queued_materialize_job_per_base() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let schema = format!("dedupe_cleanup_{}", uuid::Uuid::now_v7().simple());
    let kind = crate::phrase_bindings::MATERIALIZE_KIND;
    let result: anyhow::Result<Vec<String>> = async {
        let mut tx = pool.begin().await?;
        // The migration runs only on the jobs copy in this schema, without touching other tasks in the shared queue.
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema};
             SET LOCAL search_path TO {schema};
             CREATE TABLE jobs (id bigserial PRIMARY KEY, kind text, status text, payload jsonb);"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO jobs (kind, status, payload) VALUES
               ('materialize_inferences', 'queued', '{\"kb_id\": \"a\"}'),
               ($1, 'running', '{\"kb_id\": \"a\"}'),
               ($1, 'queued',  '{\"kb_id\": \"a\"}'),
               ($1, 'queued',  '{\"kb_id\": \"a\"}'),
               ($1, 'queued',  '{\"kb_id\": \"a\", \"deferred_since\": \"2026-11-25\"}'),
               ($1, 'queued',  '{\"kb_id\": \"b\"}'),
               ($1, 'running', '{\"kb_id\": \"a\"}'),
               ('materialize_inferences', 'queued', '{\"kb_id\": \"a\"}')",
        )
        .bind(kind)
        .execute(&mut *tx)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../../migrations/9015_a_deferred_job_is_found_by_its_kind_and_payload.sql"
        ))
        .execute(&mut *tx)
        .await?;
        let remaining = sqlx::query_scalar(
            "SELECT kind || '/' || status || '/' || (payload->>'kb_id') || '#' || id FROM jobs ORDER BY id",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.rollback().await?;
        Ok(remaining)
    }
    .await;
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await?;
    let remaining = result?;
    assert_eq!(
        remaining,
        [
            "materialize_inferences/queued/a#1",
            "materialize_typed/running/a#2",
            "materialize_typed/queued/a#3",
            "materialize_typed/queued/b#6",
            "materialize_typed/running/a#7",
            "materialize_inferences/queued/a#8",
        ]
    );
    Ok(())
}
