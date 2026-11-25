//! The "document opening" attached to the prompt is the existing chunk with the smallest ordinal, not the first chunk not yet extracted.
//!
//! Extraction takes only chunks with `extracted_at IS NULL`. The opening used to be the first entry of that list: when a changed file
//! re-extracts chunks 3 and 7, chunk 7 took chunk 3 as its opening; on a failed retry, later chunks took the failed one as their opening.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is unset. Creates and tears down its own database, never touching an existing one.

use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn the_opening_is_the_first_live_chunk_whether_or_not_it_was_extracted() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb, doc) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'opening-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'opening-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'opening-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256) VALUES ($1, $2, 'a.html', 'a')",
        )
        .bind(doc)
        .bind(kb)
        .execute(&pool)
        .await?;
        let superseded = Uuid::now_v7();
        let first = Uuid::now_v7();
        // Chunk 0 is left over from the previous version and is superseded; chunk 1 was extracted; chunks 3 and 7 are waiting to be extracted
        for (id, seq, extracted, gone) in [
            (superseded, 0, true, true),
            (first, 1, true, false),
            (Uuid::now_v7(), 3, false, false),
            (Uuid::now_v7(), 7, false, false),
        ] {
            sqlx::query(
                "INSERT INTO chunks (id, kb_id, document_id, seq, text, extracted_at, superseded_at)
                 VALUES ($1, $2, $3, $4, $5,
                         CASE WHEN $6 THEN now() END, CASE WHEN $7 THEN now() END)",
            )
            .bind(id)
            .bind(kb)
            .bind(doc)
            .bind(seq)
            .bind(format!("chunk {seq}"))
            .bind(extracted)
            .bind(gone)
            .execute(&pool)
            .await?;
        }
        let pending = utopia_store::documents::chunks_for_extraction(&pool, doc).await?;
        assert_eq!(
            pending.first().map(|c| c.seq),
            Some(3),
            "等着抽的第一块是第 3 块"
        );
        let opening = utopia_store::documents::opening_chunk(&pool, doc).await?;
        assert_eq!(
            opening.map(|(id, _)| id),
            Some(first),
            "开头是现存的第 1 块，抽没抽过都一样"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
