//! Retrieval records a hit on each document, but only for documents from a source with decay turned on (RC-003), tested against a real database.
//!
//! Behavior to pin:
//!
//! - **A source that is switched off records nothing**
//! - **First retrieval**: count 1, and the half-life goes up by 2 days from the source's value
//! - **Each further retrieval**: count plus one, half-life plus 2 days again, and the last-retrieved time is updated
//! - **Half-life capped at 365**
//! - **Several documents in one go are recorded together**, and ids that do not exist are ignored
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use utopia_store::decay::record_retrievals;
use uuid::Uuid;

async fn salience(pool: &PgPool, document: Uuid) -> anyhow::Result<Option<(i32, f32, bool)>> {
    Ok(sqlx::query_as(
        "SELECT retrieval_count, half_life_days, last_retrieved_at IS NOT NULL
         FROM document_salience WHERE document_id = $1",
    )
    .bind(document)
    .fetch_optional(pool)
    .await?)
}

#[tokio::test]
async fn a_retrieval_is_recorded_only_for_a_decaying_source() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (decaying, plain) = (Uuid::now_v7(), Uuid::now_v7());
    let (decaying_doc, plain_doc, old_doc) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'record-retrieval-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'record-retrieval-test')",
    )
    .bind(ws)
    .bind(org)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'record-retrieval-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    for (id, decay) in [
        (decaying, Some(r#"{"half_life_days": 100}"#)),
        (plain, None),
    ] {
        sqlx::query("INSERT INTO sources (id, kb_id, kind, name, decay) VALUES ($1, $2, 'folder', 'f', $3::jsonb)")
            .bind(id)
            .bind(kb)
            .bind(decay)
            .execute(&pool)
            .await?;
    }
    for (id, source) in [
        (decaying_doc, decaying),
        (plain_doc, plain),
        (old_doc, decaying),
    ] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
             VALUES ($1, $2, $3, $4, $4, 'ready')",
        )
        .bind(id)
        .bind(kb)
        .bind(source)
        .bind(id.to_string())
        .execute(&pool)
        .await?;
    }

    let run = async {
        let missing = Uuid::now_v7();
        let written = record_retrievals(&pool, &[decaying_doc, plain_doc, missing]).await?;
        assert_eq!(written, 1, "only the decaying source's document is written");
        assert_eq!(salience(&pool, plain_doc).await?, None);
        assert_eq!(salience(&pool, decaying_doc).await?, Some((1, 102.0, true)));

        record_retrievals(&pool, &[decaying_doc]).await?;
        assert_eq!(salience(&pool, decaying_doc).await?, Some((2, 104.0, true)));

        sqlx::query("INSERT INTO document_salience (document_id, half_life_days) VALUES ($1, 364)")
            .bind(old_doc)
            .execute(&pool)
            .await?;
        record_retrievals(&pool, &[old_doc]).await?;
        assert_eq!(
            salience(&pool, old_doc).await?,
            Some((1, 365.0, true)),
            "capped"
        );
        assert_eq!(record_retrievals(&pool, &[]).await?, 0);
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
