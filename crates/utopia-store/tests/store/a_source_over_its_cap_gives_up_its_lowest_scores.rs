//! Decay's eviction plan (RC-003), tested against a real database.
//!
//! Behavior to pin:
//!
//! - **Only sources over max_items get queued**; sources without decay on, or not over the limit, are not queued
//! - **Only chooses among documents older than min_age_days**; new ones must not be touched
//! - **Picks the lowest scores, and gives up exactly as many as are over**; those recently retrieved, or whose name contains "decision", score higher and are kept (one that nobody has read for a long time counts as stale, losing a point each)
//! - **Among equal scores, the older one goes first**
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use chrono::{Duration, Utc};
use sqlx::PgPool;
use utopia_store::decay::{plan_eviction, sources_over_cap};
use uuid::Uuid;

#[tokio::test]
async fn a_source_over_its_cap_gives_up_its_lowest_scores() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (capped, plain, roomy) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'decay-sweep-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'decay-sweep-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'decay-sweep-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    for (id, decay) in [
        (capped, Some(r#"{"max_items": 3, "min_age_days": 30}"#)),
        (plain, None),
        (roomy, Some(r#"{"max_items": 10}"#)),
    ] {
        sqlx::query(
            "INSERT INTO sources (id, kb_id, kind, name, decay) VALUES ($1, $2, 'folder', 'f', $3::jsonb)",
        )
        .bind(id)
        .bind(kb)
        .bind(decay)
        .execute(&pool)
        .await?;
    }
    let now = Utc::now();
    let add = |source: Uuid, name: &'static str, days_old: i64| {
        let pool = pool.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status, created_at)
                 VALUES ($1, $2, $3, $4, $1::text, 'ready', $5)",
            )
            .bind(id)
            .bind(kb)
            .bind(source)
            .bind(name)
            .bind(now - Duration::days(days_old))
            .execute(&pool)
            .await?;
            anyhow::Ok(id)
        }
    };

    let run = async {
        let old = add(capped, "notes/a", 200).await?;
        let decision = add(capped, "decision/db", 100).await?;
        let summary = add(capped, "session-summary/d", 150).await?;
        let read = add(capped, "notes/c", 90).await?;
        let young = add(capped, "notes/e", 5).await?;
        add(plain, "notes/p", 400).await?;
        sqlx::query(
            "INSERT INTO document_salience (document_id, retrieval_count, last_retrieved_at, half_life_days)
             VALUES ($1, 25, $2, 60)",
        )
        .bind(read)
        .bind(now - Duration::days(1))
        .execute(&pool)
        .await?;

        let over = sources_over_cap(&pool).await?;
        assert!(over.contains(&capped), "five documents over a cap of three");
        assert!(!over.contains(&plain), "a source that does not decay is never over a cap");
        assert!(!over.contains(&roomy));

        let plan = plan_eviction(&pool, capped, now).await?.expect("a plan");
        assert_eq!(
            (plan.kb_id, plan.documents, plan.lowest_score),
            (kb, vec![old, summary], 0),
            "two go, the oldest first; the topic one, the read one and the young one stay"
        );
        assert!(plan_eviction(&pool, plain, now).await?.is_none());
        assert!(plan_eviction(&pool, roomy, now).await?.is_none());
        let _ = (decision, read, young);
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
