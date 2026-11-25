//! A source's decay switch and a document's retrieval record (RC-003), hit against a real database.
//!
//! Behaviour to pin:
//!
//! - **Off is the default**: a newly created source has `decay` NULL, and nothing extra is written unless someone asks
//! - **The switch is an object**: `decay` accepts only a JSON object; the database rejects other shapes
//! - **The half-life has a range**: the database rejects `document_salience.half_life_days` outside 1 to 365
//! - **Follows the document**: delete the document and its retrieval record goes with it
//!
//! Without `UTOPIA_DATABASE_URL` it skips rather than fails. It sets up and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn a_source_decays_only_when_asked() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, kb, source, document) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'source-decay-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'source-decay-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'source-decay-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'folder', 'f')")
        .bind(source)
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'a.md', 'x', 'ready')",
    )
    .bind(document)
    .bind(kb)
    .bind(source)
    .execute(&pool)
    .await?;

    let run = async {
        let decay: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT decay FROM sources WHERE id = $1")
                .bind(source)
                .fetch_one(&pool)
                .await?;
        assert_eq!(decay, None, "a new source does not decay");

        let not_an_object = sqlx::query("UPDATE sources SET decay = '[]'::jsonb WHERE id = $1")
            .bind(source)
            .execute(&pool)
            .await;
        assert!(not_an_object.is_err(), "decay is an object or nothing");
        sqlx::query("UPDATE sources SET decay = '{}'::jsonb WHERE id = $1")
            .bind(source)
            .execute(&pool)
            .await?;

        for half_life in [0.5_f32, 366.0] {
            let out_of_range = sqlx::query(
                "INSERT INTO document_salience (document_id, half_life_days) VALUES ($1, $2)",
            )
            .bind(document)
            .bind(half_life)
            .execute(&pool)
            .await;
            assert!(out_of_range.is_err(), "{half_life} days is out of range");
        }
        sqlx::query("INSERT INTO document_salience (document_id, half_life_days) VALUES ($1, 62)")
            .bind(document)
            .execute(&pool)
            .await?;

        sqlx::query("DELETE FROM documents WHERE id = $1")
            .bind(document)
            .execute(&pool)
            .await?;
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM document_salience WHERE document_id = $1")
                .bind(document)
                .fetch_one(&pool)
                .await?;
        assert_eq!(remaining, 0, "a deleted document leaves no record");
        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
