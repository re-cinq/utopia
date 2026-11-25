//! A person's judgment and its recompute job are one commit (0051).
//!
//! Four things: the judgment and the job are written together, and the payload points at this library; if queuing fails, the judgment is gone too; when someone else holds
//! the materialization lock, `try_materialize` steps aside at once instead of waiting; and two judgments' jobs converge the typed graph on the last judgment
//! whichever one is processed first. Skipped when `UTOPIA_DATABASE_URL` is not set.

use super::{decide_with_delivery, decide_with_delivery_budget, Decision, MATERIALIZE_KIND};
use crate::{materialize, phrase_bindings};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    kb: Uuid,
    property: Uuid,
    sig: phrase_bindings::PhraseSignature,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb, subject, object, property, statement) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations(id,name) VALUES($1,'phrase-delivery')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces(id,org_id,name) VALUES($1,$2,'phrase-delivery')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'phrase-delivery')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, name) in [(subject, "Acme"), (object, "London")] {
        sqlx::query("INSERT INTO entities(id,kb_id,canonical_name) VALUES($1,$2,$3)")
            .bind(id)
            .bind(kb)
            .bind(name)
            .execute(pool)
            .await?;
    }
    sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,temporal) VALUES($1,$2,'based_in','based in','state')")
        .bind(property).bind(kb).execute(pool).await?;
    sqlx::query("INSERT INTO facts(id,kb_id,subject_id,object_id,layer,phrase) VALUES($1,$2,$3,$4,'open','based in')")
        .bind(statement).bind(kb).bind(subject).bind(object).execute(pool).await?;
    let sig = phrase_bindings::signatures(pool, kb).await?.remove(0);
    Ok(Fx {
        org,
        kb,
        property,
        sig,
    })
}

async fn cleanup(pool: &PgPool, f: &Fx) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM jobs WHERE payload->>'kb_id'=$1")
        .bind(f.kb.to_string())
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(f.org)
        .execute(pool)
        .await?;
    Ok(())
}

fn bound(property: Uuid) -> Decision<'static> {
    Decision {
        relation_type_id: Some(property),
        direction: Some("forward"),
        status: "bound",
        votes: &serde_json::Value::Null,
        decided_by: "person",
        basis: None,
        marks: None,
        marks_asked: false,
    }
}

fn none() -> Decision<'static> {
    Decision {
        relation_type_id: None,
        direction: None,
        status: "none",
        votes: &serde_json::Value::Null,
        decided_by: "person",
        basis: None,
        marks: None,
        marks_asked: false,
    }
}

async fn jobs_for(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<(i64, String, String)>> {
    Ok(
        sqlx::query_as("SELECT id, kind, status FROM jobs WHERE payload->>'kb_id'=$1 ORDER BY id")
            .bind(kb.to_string())
            .fetch_all(pool)
            .await?,
    )
}

#[tokio::test]
async fn a_decision_and_its_job_commit_together() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        let id = decide_with_delivery(&pool, f.kb, &f.sig, bound(f.property))
            .await?
            .expect("a person's decision is always written");
        let jobs = jobs_for(&pool, f.kb).await?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, id);
        assert_eq!(jobs[0].1, MATERIALIZE_KIND);
        assert_eq!(jobs[0].2, "queued");
        let bindings = phrase_bindings::bindings(&pool, f.kb).await?;
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].status, "bound");
        // The payload carries only the library: the job reads the current binding, and does not replay this judgment's properties
        let payload: serde_json::Value = sqlx::query_scalar("SELECT payload FROM jobs WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(payload, json!({ "kb_id": f.kb }));
        anyhow::Ok(())
    }
    .await;
    cleanup(&pool, &f).await?;
    run
}

#[tokio::test]
async fn an_enqueue_failure_takes_the_decision_with_it() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        // Budget 0 is rejected by enqueue: a real failure occurring after the judgment write
        assert!(
            decide_with_delivery_budget(&pool, f.kb, &f.sig, bound(f.property), 0)
                .await
                .is_err()
        );
        assert!(phrase_bindings::bindings(&pool, f.kb).await?.is_empty());
        assert!(jobs_for(&pool, f.kb).await?.is_empty());
        anyhow::Ok(())
    }
    .await;
    cleanup(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_busy_projection_is_declined_not_waited_for() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        decide_with_delivery(&pool, f.kb, &f.sig, bound(f.property)).await?;
        // Another connection holds the lock: the try version returns None at once, without taking a second connection to queue
        let mut gate = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('typed_materialize'), hashtext($1))")
            .bind(f.kb.to_string())
            .execute(&mut *gate)
            .await?;
        let started = std::time::Instant::now();
        assert!(materialize::try_materialize(&pool, f.kb).await?.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        gate.commit().await?;
        let outcome = materialize::try_materialize(&pool, f.kb)
            .await?
            .expect("lock released");
        assert_eq!(outcome.added, 1);
        anyhow::Ok(())
    }
    .await;
    cleanup(&pool, &f).await?;
    run
}

#[tokio::test]
async fn jobs_processed_in_any_order_converge_on_the_last_decision() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        let first = decide_with_delivery(&pool, f.kb, &f.sig, bound(f.property)).await?;
        // Claim the first job: a running job does not absorb new judgments (0051), so the second judgment gets a job of its own
        claim(&pool, first).await?;
        let second = decide_with_delivery(&pool, f.kb, &f.sig, none()).await?;
        assert!(first < second);
        // Both jobs exist (one running, one queued); process the later one first, then the earlier one -- each reads the current binding
        assert_eq!(jobs_for(&pool, f.kb).await?.len(), 2);
        materialize::try_materialize(&pool, f.kb).await?;
        materialize::try_materialize(&pool, f.kb).await?;
        assert_eq!(
            materialize::count(&pool, f.kb).await?,
            0,
            "the last decision was none"
        );
        // The reverse: bind, then unbind, and process the old one first
        decide_with_delivery(&pool, f.kb, &f.sig, bound(f.property)).await?;
        materialize::try_materialize(&pool, f.kb).await?;
        assert_eq!(materialize::count(&pool, f.kb).await?, 1);
        anyhow::Ok(())
    }
    .await;
    cleanup(&pool, &f).await?;
    run
}

async fn claim(pool: &PgPool, job: Option<i64>) -> anyhow::Result<()> {
    sqlx::query("UPDATE jobs SET status = 'running' WHERE id = $1")
        .bind(job)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_running_job_does_not_absorb_the_next_decision() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        let running = decide_with_delivery(&pool, f.kb, &f.sig, bound(f.property)).await?;
        claim(&pool, running).await?;
        let next = decide_with_delivery(&pool, f.kb, &f.sig, none()).await?;
        assert_ne!(next, running);
        anyhow::Ok(())
    }
    .await;
    cleanup(&pool, &f).await?;
    run
}

#[tokio::test]
async fn a_decision_covered_by_a_queued_job_returns_its_id_and_is_materialized_by_it(
) -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    crate::db::migrate(&pool).await?;
    let f = seed(&pool).await?;
    let run = async {
        let first = decide_with_delivery(&pool, f.kb, &f.sig, none()).await?;
        let second = decide_with_delivery(&pool, f.kb, &f.sig, bound(f.property)).await?;
        assert_eq!(second, first);
        assert_eq!(jobs_for(&pool, f.kb).await?.len(), 1);
        materialize::try_materialize(&pool, f.kb).await?;
        assert_eq!(materialize::count(&pool, f.kb).await?, 1);
        anyhow::Ok(())
    }
    .await;
    cleanup(&pool, &f).await?;
    run
}
