//! Decay-eviction records (RC-003), run against a real database.
//!
//! Behavior pinned: eviction records one entry; restoring (by anyone) clears that entry; a document that was never evicted has no entry.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own database, never touching an existing one.

use sqlx::PgPool;
use utopia_store::decay::{mark_evicted, was_evicted};
use uuid::Uuid;

#[tokio::test]
async fn an_evicted_document_is_remembered_until_it_is_restored() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb, source) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (evicted, untouched) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'decay-eviction-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'decay-eviction-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'decay-eviction-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'api', 'f')")
        .bind(source)
        .bind(kb)
        .execute(&pool)
        .await?;
    for id in [evicted, untouched] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
             VALUES ($1, $2, $3, $1::text, $1::text, 'ready')",
        )
        .bind(id)
        .bind(kb)
        .bind(source)
        .execute(&pool)
        .await?;
    }

    let run = async {
        assert!(
            !was_evicted(&pool, evicted).await?,
            "nothing is evicted yet"
        );

        utopia_store::documents::delete(&pool, kb, evicted, None).await?;
        mark_evicted(&pool, evicted).await?;
        mark_evicted(&pool, evicted).await?;
        assert!(was_evicted(&pool, evicted).await?);
        assert!(!was_evicted(&pool, untouched).await?);

        utopia_store::documents::restore(&pool, kb, evicted).await?;
        assert!(
            !was_evicted(&pool, evicted).await?,
            "a restore clears the record"
        );
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
