//! Stale documents (RC-003), run against a real database.
//!
//! Behavior pinned: only a document that opened a decaying source can be stale; one not retrieved for a long time (if never retrieved, counted from creation)
//! exceeds the source's `stale_after_days` before it counts; one retrieval makes it not stale again.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own database, never touching an existing one.

use sqlx::PgPool;
use utopia_store::decay::{record_retrievals, stale_documents};
use uuid::Uuid;

#[tokio::test]
async fn a_stale_document_is_found_among_a_decaying_sources() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (decaying, plain) = (Uuid::now_v7(), Uuid::now_v7());
    let (old, recent, read, unwatched) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'stale-document-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'stale-document-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'stale-document-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    for (id, decay) in [
        (decaying, Some(r#"{"stale_after_days": 20}"#)),
        (plain, None),
    ] {
        sqlx::query("INSERT INTO sources (id, kb_id, kind, name, decay) VALUES ($1, $2, 'folder', 'f', $3::jsonb)")
            .bind(id)
            .bind(kb)
            .bind(decay)
            .execute(&pool)
            .await?;
    }
    for (id, source, days_old) in [
        (old, decaying, 25),
        (recent, decaying, 10),
        (read, decaying, 25),
        (unwatched, plain, 400),
    ] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status, created_at)
             VALUES ($1, $2, $3, $1::text, $1::text, 'ready', now() - make_interval(days => $4))",
        )
        .bind(id)
        .bind(kb)
        .bind(source)
        .bind(days_old)
        .execute(&pool)
        .await?;
    }

    let run = async {
        let all = [old, recent, read, unwatched];
        let stale = stale_documents(&pool, &all).await?;
        assert_eq!(
            stale,
            [old, read].into_iter().collect(),
            "past 20 days, in a decaying source"
        );

        record_retrievals(&pool, &[read]).await?;
        let stale = stale_documents(&pool, &all).await?;
        assert_eq!(
            stale,
            [old].into_iter().collect(),
            "a retrieval makes it fresh"
        );
        assert!(stale_documents(&pool, &[]).await?.is_empty());
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
