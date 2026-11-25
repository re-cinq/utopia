//! Retrieval also rewinds (0019 open question ②), run against a real database.
//!
//! Two things are pinned here:
//!
//! 1. **Re-parsing no longer drops the old chunk's vector.** Previously a losing chunk had `embedding = NULL`, which saved
//!    the smaller of the two halves — the body was kept anyway — at the cost of the whole history becoming unsearchable.
//! 2. **Retrieval with a moment follows the liveness at that time**: the chunks alive then, the documents not yet deleted then.
//!    Deletion leaves a tombstone (#268), so a "document deleted last week" still hits in retrieval from two weeks ago.
//!
//! The full-text path is not here: the Tantivy index has only one "now" version, so with a moment, what it finds is correct
//! but not complete (as stated in the module header). This test exercises the vector and chunk-fetch paths.

use pgvector::Vector;
use sqlx::PgPool;
use utopia_ingest::ChunkPiece;
use uuid::Uuid;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

struct Fixture {
    org: Uuid,
    kb: Uuid,
    /// The document currently in force
    doc: Uuid,
    /// The old chunk overwritten by re-parsing in May
    old_chunk: Uuid,
    /// The new chunk replacing it
    new_chunk: Uuid,
    /// Belongs to a document deleted in June
    deleted_doc_chunk: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (doc, deleted_doc) = (Uuid::now_v7(), Uuid::now_v7());
    let (old_chunk, deleted_doc_chunk) = (Uuid::now_v7(), Uuid::now_v7());
    let tag = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'as-of-search-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'as-of-search-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name)
         VALUES ($1, $2, 'as-of-search-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, name, deleted) in [
        (doc, "handbook.md", None),
        (deleted_doc, "retired.md", Some("2026-06-01T00:00:00Z")),
    ] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256, status, created_at, deleted_at)
             VALUES ($1, $2, $3, $4, 'ready', $5, $6)",
        )
        .bind(id)
        .bind(kb)
        .bind(name)
        .bind(format!("sha-{tag}-{name}"))
        .bind(t("2026-02-01T00:00:00Z"))
        .bind(deleted.map(t))
        .execute(pool)
        .await?;
    }

    let embed = |v: [f32; 3]| Vector::from(v.to_vec());
    for (id, document, text, vector) in [
        (
            old_chunk,
            doc,
            "The 2025 handbook says the office is in Shanghai.",
            embed([1.0, 0.0, 0.0]),
        ),
        (
            deleted_doc_chunk,
            deleted_doc,
            "A retired note about the Shanghai office.",
            embed([0.9, 0.1, 0.0]),
        ),
    ] {
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text, embedding, created_at)
             VALUES ($1, $2, $3, 0, $4, $5, $6)",
        )
        .bind(id)
        .bind(kb)
        .bind(document)
        .bind(text)
        .bind(vector)
        .bind(t("2026-02-01T00:00:00Z"))
        .execute(pool)
        .await?;
    }

    // May: re-parse. The old chunk loses out and is soft-deleted, the new chunk takes its place — **the old chunk's vector must still be there**
    let pieces = vec![ChunkPiece {
        seq: 0,
        text: "The 2026 handbook says the office moved to Shenzhen.".into(),
        char_start: 0,
        char_end: 51,
        heading: None,
        provenance: utopia_ingest::Provenance::stated(),
    }];
    utopia_store::documents::replace_chunks(pool, kb, doc, &pieces).await?;
    let new_chunk: (Uuid,) = sqlx::query_as(
        "SELECT id FROM chunks WHERE document_id = $1 AND superseded_at IS NULL LIMIT 1",
    )
    .bind(doc)
    .fetch_one(pool)
    .await?;
    // The new chunk's vector is filled in afterwards by the ingestion pipeline; give one here directly so it can enter vector recall
    sqlx::query("UPDATE chunks SET embedding = $2, created_at = $3 WHERE id = $1")
        .bind(new_chunk.0)
        .bind(embed([0.0, 1.0, 0.0]))
        .bind(t("2026-05-01T00:00:00Z"))
        .execute(pool)
        .await?;
    sqlx::query("UPDATE chunks SET superseded_at = $2 WHERE id = $1")
        .bind(old_chunk)
        .bind(t("2026-05-01T00:00:00Z"))
        .execute(pool)
        .await?;

    Ok(Fixture {
        org,
        kb,
        doc,
        old_chunk,
        new_chunk: new_chunk.0,
        deleted_doc_chunk,
    })
}

#[tokio::test]
async fn a_superseded_chunk_keeps_its_vector_and_answers_as_of_then() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    // 1. The chunk that was overwritten still carries its vector. Without the vector, everything below would be meaningless
    let kept: (bool,) = sqlx::query_as("SELECT embedding IS NOT NULL FROM chunks WHERE id = $1")
        .bind(f.old_chunk)
        .fetch_one(&pool)
        .await?;
    assert!(
        kept.0,
        "重解析不该顺手丢掉旧块的向量——历史就是靠它才搜得出来"
    );

    // 2. Now: vector recall gives the new chunk, and cannot give the overwritten old one
    let query = vec![1.0_f32, 0.0, 0.0];
    let now_hits =
        utopia_store::documents::vector_search(&pool, f.kb, &query, 10, None, None).await?;
    assert!(
        !now_hits.contains(&f.old_chunk),
        "current search must not surface old versions"
    );
    assert!(now_hits.contains(&f.new_chunk));

    // 3. March: then the old chunk was current and the new chunk did not yet exist
    let then = Some(t("2026-03-01T00:00:00Z"));
    let then_hits =
        utopia_store::documents::vector_search(&pool, f.kb, &query, 10, then, None).await?;
    assert!(
        then_hits.contains(&f.old_chunk),
        "三月该搜得到当时那一版——这一条在丢向量的实现上不可能成立"
    );
    assert!(
        !then_hits.contains(&f.new_chunk),
        "五月才有的块不该出现在三月"
    );

    // 4. Fetching chunks also rewinds: a document deleted in June is still returned by a search in May (#268 left a tombstone)
    let live_now = utopia_store::documents::chunks_by_ids(
        &pool,
        f.kb,
        &[f.deleted_doc_chunk, f.new_chunk],
        None,
    )
    .await?;
    let ids: Vec<Uuid> = live_now.iter().map(|c| c.chunk.id).collect();
    assert!(
        !ids.contains(&f.deleted_doc_chunk),
        "删掉的文档不出现在现在"
    );
    assert!(ids.contains(&f.new_chunk));

    let live_then = utopia_store::documents::chunks_by_ids(
        &pool,
        f.kb,
        &[f.deleted_doc_chunk, f.old_chunk],
        Some(t("2026-05-15T00:00:00Z")),
    )
    .await?;
    let ids: Vec<Uuid> = live_then.iter().map(|c| c.chunk.id).collect();
    assert!(
        ids.contains(&f.deleted_doc_chunk),
        "五月中那篇文档还没被删，它的块该拿得回来"
    );
    assert!(
        !ids.contains(&f.old_chunk),
        "五月初已被顶掉的块不属于五月中"
    );

    cleanup(&pool, &f).await
}

/// When fetching just one, the nearest is the chunk of the deleted document; restricted to current documents, the slot goes to its chunk —
/// the filter applies before `LIMIT`, not after taking one and filtering down to zero.
#[tokio::test]
async fn a_document_filter_spends_the_limit_on_its_own_chunks() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;
    let query = vec![1.0_f32, 0.0, 0.0];
    let search =
        |documents| utopia_store::documents::vector_search(&pool, f.kb, &query, 1, None, documents);
    assert_eq!(search(None).await?, vec![f.deleted_doc_chunk]);
    assert_eq!(search(Some(&[f.doc][..])).await?, vec![f.new_chunk]);
    cleanup(&pool, &f).await
}

async fn cleanup(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(pool)
        .await?;
    let gone = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(pool)
        .await?;
    assert_eq!(gone.rows_affected(), 1, "one-off org was not deleted");
    Ok(())
}
