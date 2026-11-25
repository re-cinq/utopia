//! A provenance chain must not cross KBs (0070): new rows are blocked by the trigger, and stored bad rows make the export reject the whole thing.
//!
//! The foreign keys on `fact_evidence`/`chunks` check only the id, not the KB — the native write paths happen to build only same-KB
//! references, but the schema blocks nothing. Two layers of defense:
//!   1. The trigger (0070) sits downstream of all write paths, including SQL that bypasses the store layer;
//!   2. The export side runs a health check and per-page validation — stored bad rows and rows that got past the trigger are rejected as a whole export rather than
//!      minting another KB's object id under this KB's IRI.
//!
//! Bad rows are produced in the test via `SET LOCAL session_replication_role='replica'`: it turns off triggers only
//! for this transaction and does not touch the catalog — `DISABLE TRIGGER` is global, and parallel tests would
//! collide with each other's rejection windows. The things a row points at all really exist, just not in the same KB —
//! exactly the shape production would meet (e.g. old rows restored from a backup taken before 0070).

use sqlx::{Acquire, PgPool};
use uuid::Uuid;

struct TwoKbs {
    org: Uuid,
    a: Uuid,
    b: Uuid,
    doc_a: Uuid,
    doc_b: Uuid,
    chunk_a: Uuid,
    chunk_b: Uuid,
    fact_a: Uuid,
    fact_b: Uuid,
}

/// Two KBs, one document, one chunk, one entity and one fact each — the legal parts needed for a cross-KB reference
async fn seed(pool: &PgPool) -> anyhow::Result<TwoKbs> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (doc_a, doc_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (chunk_a, chunk_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (ent_a, ent_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (fact_a, fact_b) = (Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'crosskb-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'crosskb-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for kb in [a, b] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'crosskb-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    }
    for (id, kb, name) in [(doc_a, a, "a.md"), (doc_b, b, "b.md")] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256, status, external_key)
             VALUES ($1, $2, $3, $4, 'ready', $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(name)
        .bind(format!("sha-{id}"))
        .bind(format!("file:///{name}"))
        .execute(pool)
        .await?;
    }
    for (id, kb, doc) in [(chunk_a, a, doc_a), (chunk_b, b, doc_b)] {
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, 'x')",
        )
        .bind(id)
        .bind(kb)
        .bind(doc)
        .execute(pool)
        .await?;
    }
    for (id, kb) in [(ent_a, a), (ent_b, b)] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
            .bind(id)
            .bind(kb)
            .execute(pool)
            .await?;
    }
    for (id, kb, subject, object) in [(fact_a, a, ent_a, ent_a), (fact_b, b, ent_b, ent_b)] {
        sqlx::query("INSERT INTO facts (id, kb_id, subject_id, object_id, confidence) VALUES ($1, $2, $3, $4, 0.9)")
            .bind(id)
            .bind(kb)
            .bind(subject)
            .bind(object)
            .execute(pool)
            .await?;
    }
    Ok(TwoKbs {
        org,
        a,
        b,
        doc_a,
        doc_b,
        chunk_a,
        chunk_b,
        fact_a,
        fact_b,
    })
}

async fn cleanup(pool: &PgPool, f: &TwoKbs) -> anyhow::Result<()> {
    for kb in [f.a, f.b] {
        sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
            .bind(kb)
            .execute(pool)
            .await?;
    }
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn new_cross_kb_writes_are_rejected() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // The trigger came with 0070: the test database may not be migrated yet, so first make sure the constraint is in place
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // A fact referencing another KB's chunk: the native path happens not to write it this way, but the schema did not block it before — now it does
    let err = sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
         VALUES ($1, $2, 'x', $3, 1)",
    )
    .bind(f.fact_a)
    .bind(f.chunk_b)
    .bind(f.doc_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "fact→foreign chunk 必须被拒");

    // Only the chunk side is bad: the chunk is in the same KB, the document is in another
    let err = sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
         VALUES ($1, $2, 'x', $3, 1)",
    )
    .bind(f.fact_a)
    .bind(f.chunk_a)
    .bind(f.doc_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "evidence.document→foreign 必须被拒");

    // Chunk hangs under another KB's document
    let err = sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text)
         VALUES ($1, $2, $3, 0, 'x')",
    )
    .bind(Uuid::now_v7())
    .bind(f.a)
    .bind(f.doc_b)
    .execute(&pool)
    .await;
    assert!(err.is_err(), "chunk→foreign document 必须被拒");

    // The same constraint on the authoritative write path: add_evidence through the store API is rejected too
    let err = utopia_store::graph::add_evidence(&pool, f.fact_a, f.chunk_b, Some("x"), None).await;
    assert!(err.is_err(), "add_evidence 的跨库配对必须被拒");

    // Transfer: moving a document to another KB is the same as making every row pointing at it bad at once
    let err = sqlx::query("UPDATE documents SET kb_id = $2 WHERE id = $1")
        .bind(f.doc_a)
        .bind(f.b)
        .execute(&pool)
        .await;
    assert!(err.is_err(), "documents.kb_id 过户必须被拒");
    let err = sqlx::query("UPDATE facts SET kb_id = $2 WHERE id = $1")
        .bind(f.fact_a)
        .bind(f.b)
        .execute(&pool)
        .await;
    assert!(err.is_err(), "facts.kb_id 过户必须被拒");

    // Same-KB normal writes are unaffected (guards against false positives)
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, quote, document_id, doc_version)
         VALUES ($1, $2, 'ok', $3, 1)",
    )
    .bind(f.fact_a)
    .bind(f.chunk_a)
    .bind(f.doc_a)
    .execute(&pool)
    .await?;
    utopia_store::graph::add_evidence(&pool, f.fact_b, f.chunk_b, Some("ok"), None).await?;

    cleanup(&pool, &f).await
}

#[tokio::test]
async fn malformed_existing_rows_fail_the_export_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // Stored bad rows: the trigger only blocks writes after it, so these rows can only be made by bypassing it — the export side must catch them.
    // SET LOCAL turns off the trigger only inside this transaction and restores it at commit, so it does not hit parallel tests' assertion windows
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;

    // Only the chunk end is bad: fact_a → chunk_b (document pointer left NULL, isolating the variable)
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id) VALUES ($1, $2)")
        .bind(f.fact_a)
        .bind(f.chunk_b)
        .execute(&mut *tx)
        .await?;
    // Only the redundant document pointer is bad: fact_b → chunk_b itself is in the same KB, but the pointer points at a's document
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id, document_id) VALUES ($1, $2, $3)")
        .bind(f.fact_b)
        .bind(f.chunk_b)
        .bind(f.doc_a)
        .execute(&mut *tx)
        .await?;
    // Chunk hangs under another KB's document
    let stray_chunk = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 9, 'stray')",
    )
    .bind(stray_chunk)
    .bind(f.b)
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    drop(conn);

    // Health check: KB A is bad at evidence.chunk, KB B is bad at evidence.document and chunk.document
    let err_a = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.a).await;
    let msg_a = format!("{err_a:?}");
    assert!(err_a.is_err(), "库 A 的体检必须拒导");
    assert!(
        msg_a.contains("evidence.chunk"),
        "库 A 该报 evidence.chunk: {msg_a}"
    );

    let err_b = utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.b).await;
    let msg_b = format!("{err_b:?}");
    assert!(err_b.is_err(), "库 B 的体检必须拒导");
    assert!(
        msg_b.contains("evidence.document"),
        "库 B 该报 evidence.document: {msg_b}"
    );

    // Per-page validation is equally fail-closed: even a TOCTOU bad row after the health check must not leak out a forged IRI.
    // A bad chunk pointer goes out through facts_page's quote_origins, a bad document pointer goes out through
    // documents[] — both are blocked on the facts page
    assert!(
        utopia_store::export::facts_page(&mut pool.begin().await?, f.a, None)
            .await
            .is_err(),
        "fact_a 的 evidence.chunk 别库：事实页要拦"
    );
    assert!(
        utopia_store::export::facts_page(&mut pool.begin().await?, f.b, None)
            .await
            .is_err(),
        "fact_b 的 evidence.document 别库：事实页要拦"
    );

    // Clear the bad rows and export recovers at once — it is the bad rows being rejected, not the KB itself
    sqlx::query("DELETE FROM fact_evidence WHERE fact_id IN ($1, $2)")
        .bind(f.fact_a)
        .bind(f.fact_b)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM chunks WHERE id = $1")
        .bind(stray_chunk)
        .execute(&pool)
        .await?;
    utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.a).await?;
    utopia_store::export::provenance_integrity(&mut pool.begin().await?, f.b).await?;

    cleanup(&pool, &f).await
}
