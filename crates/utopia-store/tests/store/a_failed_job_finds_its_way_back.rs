//! #216: failed tasks can find their way back to the queue.
//!
//! Previously `failed` was a terminal state: when credits ran out a batch of documents all failed, and after topping up you could only click each one, or re-extract the whole source
//! (which also reruns the model on those that had already succeeded). `jobs::requeue_failed` requeues by scope: library, kind, and failure time
//! are all nullable. Three things are guarded here:
//!
//! 1. **Scoping by library is precise.** A task's payload carries only one of `document_id` / `source_id` / `kb_id`,
//!    so requeueing by library must resolve to a library: none of another library's tasks are touched, and tasks without a library (system tasks) are touched only when the library is unlimited.
//! 2. **Only failed ones are touched**: `done` and `queued` are left alone; after requeueing, `attempts` resets to zero and they are due immediately.
//! 3. **Kind and time window**: `kind` requeues only that kind; `failed_since` requeues only those that failed after that moment:
//!    the "run again" on an alert covers exactly the tasks from that outage window.
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use chrono::{Duration, Utc};
use sqlx::PgPool;
use utopia_store::jobs::{self, RequeueScope};
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb1: Uuid,
    kb2: Uuid,
    doc1: Uuid,
    src2: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb1, kb2) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (src1, doc1, src2) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'requeue-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'requeue-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for kb in [kb1, kb2] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'requeue-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    }
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'folder', 'f')")
        .bind(src1)
        .bind(kb1)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'a.md', 'requeue', 'ready')",
    )
    .bind(doc1)
    .bind(kb1)
    .bind(src1)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'url', 'u')")
        .bind(src2)
        .bind(kb2)
        .execute(pool)
        .await?;
    Ok(Fx {
        org,
        kb1,
        kb2,
        doc1,
        src2,
    })
}

async fn job(
    pool: &PgPool,
    kind: &str,
    payload: serde_json::Value,
    status: &str,
    failed_ago: Duration,
) -> anyhow::Result<i64> {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO jobs (kind, payload, status, attempts, last_error, updated_at)
         VALUES ($1, $2, $3, 3, 'out of credit', $4) RETURNING id",
    )
    .bind(kind)
    .bind(payload)
    .bind(status)
    .bind(Utc::now() - failed_ago)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn state(pool: &PgPool, id: i64) -> anyhow::Result<(String, i32)> {
    Ok(
        sqlx::query_as("SELECT status, attempts FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[tokio::test]
async fn a_failed_job_finds_its_way_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let mut ids: Vec<i64> = Vec::new();

    let run = async {
        let m = Duration::minutes(1);
        // kb1: document tasks, a library-level task, one that failed long ago, and one that already succeeded
        let d1 = job(
            &pool,
            "process_document",
            serde_json::json!({ "document_id": f.doc1 }),
            "failed",
            m,
        )
        .await?;
        let b1 = job(
            &pool,
            "bootstrap_ontology",
            serde_json::json!({ "kb_id": f.kb1 }),
            "failed",
            m,
        )
        .await?;
        let old1 = job(
            &pool,
            "process_document",
            serde_json::json!({ "document_id": f.doc1 }),
            "failed",
            Duration::days(3),
        )
        .await?;
        let done1 = job(
            &pool,
            "process_document",
            serde_json::json!({ "document_id": f.doc1 }),
            "done",
            m,
        )
        .await?;
        // kb2: a source task
        let s2 = job(
            &pool,
            "sync_source",
            serde_json::json!({ "source_id": f.src2 }),
            "failed",
            m,
        )
        .await?;
        // A system task with no library
        let sys = job(&pool, "noop", serde_json::json!({}), "failed", m).await?;
        ids.extend([d1, b1, old1, done1, s2, sys]);

        assert_eq!(jobs::failed_count(&pool, Some(f.kb1)).await?, 3);
        assert_eq!(jobs::failed_count(&pool, Some(f.kb2)).await?, 1);

        // 3. Time window: only requeue kb1 tasks that failed within the last hour
        let n = jobs::requeue_failed(
            &pool,
            RequeueScope {
                kb_id: Some(f.kb1),
                kind: None,
                failed_since: Some(Utc::now() - Duration::hours(1)),
            },
        )
        .await?;
        assert_eq!(
            n, 2,
            "the document job and the base-level job, not the old one"
        );
        assert_eq!(state(&pool, d1).await?, ("queued".into(), 0));
        assert_eq!(state(&pool, b1).await?, ("queued".into(), 0));
        assert_eq!(state(&pool, old1).await?.0, "failed");
        assert_eq!(state(&pool, done1).await?.0, "done", "done stays done");
        assert_eq!(
            state(&pool, s2).await?.0,
            "failed",
            "another base is untouched"
        );
        assert_eq!(
            state(&pool, sys).await?.0,
            "failed",
            "a system job has no base"
        );

        // 3. Kind: in kb1 only process_document is requeued; the old one comes back this time
        let n = jobs::requeue_failed(
            &pool,
            RequeueScope {
                kb_id: Some(f.kb1),
                kind: Some("process_document"),
                failed_since: None,
            },
        )
        .await?;
        assert_eq!(n, 1);
        assert_eq!(state(&pool, old1).await?, ("queued".into(), 0));
        assert_eq!(jobs::failed_count(&pool, Some(f.kb1)).await?, 0);

        // 1. By library: kb2's source task
        let n = jobs::requeue_failed(
            &pool,
            RequeueScope {
                kb_id: Some(f.kb2),
                kind: None,
                failed_since: None,
            },
        )
        .await?;
        assert_eq!(n, 1);
        assert_eq!(state(&pool, s2).await?, ("queued".into(), 0));

        // System tasks are touched only when the library is unlimited
        let before = jobs::failed_count(&pool, None).await?;
        let n = jobs::requeue_failed(
            &pool,
            RequeueScope {
                kb_id: None,
                kind: Some("noop"),
                failed_since: Some(Utc::now() - Duration::hours(1)),
            },
        )
        .await?;
        assert!(n >= 1);
        assert_eq!(state(&pool, sys).await?.0, "queued");
        assert!(jobs::failed_count(&pool, None).await? < before);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Teardown: the task table does not cascade with the organization, so clear it by hand
    let _ = sqlx::query("DELETE FROM jobs WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await;
    run
}
