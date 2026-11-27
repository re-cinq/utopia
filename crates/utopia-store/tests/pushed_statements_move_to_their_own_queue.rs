//! Migration 9018 moves the extractions already queued for documents of a `statements` source to `extract_statements`;
//! a running one is left alone, other documents are left alone, and running it twice changes nothing.
//!
//! The UPDATE does not filter by kb and looks at extractions queued across the whole DB, so it is its own binary (for why, see `store/main.rs`).

use sqlx::PgPool;
use uuid::Uuid;

const MIGRATION: &str =
    include_str!("../../../migrations/9018_pushed_statements_are_claimed_before_model_work.sql");

async fn document_of(pool: &PgPool, kb: Uuid, source: Uuid) -> anyhow::Result<Uuid> {
    let document = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'a.md', $4, 'ready')",
    )
    .bind(document)
    .bind(kb)
    .bind(source)
    .bind(format!("sha-{document}"))
    .execute(pool)
    .await?;
    Ok(document)
}

async fn queue(pool: &PgPool, document: Uuid, status: &str) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO jobs (kind, payload, status) VALUES ('extract_document', $1, $2)")
        .bind(serde_json::json!({ "document_id": document }))
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

async fn kinds_of(pool: &PgPool, documents: &[Uuid]) -> anyhow::Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT kind, status FROM jobs WHERE payload->>'document_id' = ANY($1)
          ORDER BY array_position($1, payload->>'document_id'), id",
    )
    .bind(documents.iter().map(Uuid::to_string).collect::<Vec<_>>())
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
async fn queued_extractions_of_statements_documents_become_extract_statements() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let [org, ws, kb, statements, api] = std::array::from_fn(|_| Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'pushed')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'pushed')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'pushed')")
        .bind(kb)
        .bind(ws)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO sources (id, kind, kb_id, name) VALUES ($1, 'statements', $3, 'pushed'), ($2, 'api', $3, 'pushed')",
    )
    .bind(statements)
    .bind(api)
    .bind(kb)
    .execute(&pool)
    .await?;
    let waiting = document_of(&pool, kb, statements).await?;
    let busy = document_of(&pool, kb, statements).await?;
    let model_work = document_of(&pool, kb, api).await?;
    queue(&pool, waiting, "queued").await?;
    queue(&pool, busy, "running").await?;
    queue(&pool, model_work, "queued").await?;
    let documents = [waiting, busy, model_work];

    sqlx::raw_sql(MIGRATION).execute(&pool).await?;
    sqlx::raw_sql(MIGRATION).execute(&pool).await?;
    let kinds = kinds_of(&pool, &documents).await?;

    sqlx::query("DELETE FROM jobs WHERE payload->>'document_id' = ANY($1)")
        .bind(documents.iter().map(Uuid::to_string).collect::<Vec<_>>())
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    pool.close().await;
    assert_eq!(
        kinds,
        [
            ("extract_statements", "queued"),
            ("extract_document", "running"),
            ("extract_document", "queued"),
        ]
        .map(|(kind, status)| (kind.to_string(), status.to_string()))
    );
    Ok(())
}
