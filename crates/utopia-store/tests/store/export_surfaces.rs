//! Export read surfaces: a single-transaction snapshot and cursor boundaries.
//!
//! The judgment lives in the export read layer: `provenance_integrity` and each page function take the caller's transaction:
//! seeding rows, reading pages and asserting all happen on one connection. Apart from the "commit mid-way" probe (which needs two connections),
//! all seeds and bad rows are built inside one **rolled-back transaction**, so running this set of tests on a snapshot database leaves
//! no row behind.
//!
//! Bad rows are built with `SET LOCAL session_replication_role='replica'`: it disables only this transaction's
//! triggers (including those that 0070 installed), and rollback restores them. What is being simulated is exactly
//! stored bad rows that slipped in by bypassing the triggers.

use sqlx::{Acquire, PgPool, Postgres, Transaction};
use utopia_store::export;
use uuid::Uuid;

struct Fixture {
    a: Uuid,
    b: Uuid,
    doc_a: Uuid,
    attr_a: Uuid,
}

/// Two libraries; library A has one document, one passage, one entity, one fact, one piece of evidence and one attribute; library B is left empty for reference.
/// Everything is written inside the caller's transaction: rollback clears the scene, no DELETE needed
async fn seed_tx(tx: &mut Transaction<'_, Postgres>) -> anyhow::Result<Fixture> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (doc_a, doc_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (chunk_a, ent_a, ent_b, fact_a, attr_a) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'export-test')")
        .bind(org)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'export-test')")
        .bind(ws)
        .bind(org)
        .execute(&mut **tx)
        .await?;
    for kb in [a, b] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'export-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text, doc_version)
         VALUES ($1, $2, $3, 0, 'x', 1)",
    )
    .bind(chunk_a)
    .bind(a)
    .bind(doc_a)
    .execute(&mut **tx)
    .await?;
    for (id, kb) in [(ent_a, a), (ent_b, b)] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
            .bind(id)
            .bind(kb)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, 0.9)",
    )
    .bind(fact_a)
    .bind(a)
    .bind(ent_a)
    .bind(ent_a)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, document_id, doc_version)
         VALUES ($1, $2, $3, 1)",
    )
    .bind(fact_a)
    .bind(chunk_a)
    .bind(doc_a)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype)
         VALUES ($1, $2, 'headcount', 'headcount', 'attribute', 'number')",
    )
    .bind(attr_a)
    .bind(a)
    .execute(&mut **tx)
    .await?;
    Ok(Fixture {
        a,
        b,
        doc_a,
        attr_a,
    })
}

/// The extreme form of document transfer (replica): after the document moves to B, A's evidence row still points at it:
/// A's export would rather refuse than mint another library's document into this library's IRI; B does not point at it, so it passes as usual
#[tokio::test]
async fn a_reassigned_document_breaks_the_edges_that_point_at_it() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE documents SET kb_id = $2 WHERE id = $1")
        .bind(f.doc_a)
        .bind(f.b)
        .execute(&mut *tx)
        .await?;

    // A's export is refused as a whole: evidence.document still points at a document that no longer belongs to this library
    let err = export::provenance_integrity(&mut tx, f.a).await;
    let msg = format!("{err:?}");
    assert!(
        err.is_err(),
        "after the document is transferred, A's provenance chain must be refused on export"
    );
    assert!(
        msg.contains("evidence.document"),
        "该报 evidence.document: {msg}"
    );
    assert!(export::facts_page(&mut tx, f.a, None).await.is_err());

    // B has received the document, but no row points at A from it: its health check still passes
    export::provenance_integrity(&mut tx, f.b).await?;
    tx.rollback().await?;
    Ok(())
}

/// A write that lands mid-export does not get into this copy. After the tx starts a REPEATABLE READ snapshot,
/// another connection commits a new predicate plus a derivation that references it: this transaction's vocabulary page and derivation page
/// both cannot see it: no half-arrived reference, and no dangling wasGeneratedBy.
/// This case needs two connections, so the seed must commit: clean-up still runs as usual
#[tokio::test]
async fn a_mid_stream_commit_stays_outside_the_snapshot() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, a) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let ent_a = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'export-race')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'export-race')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'export-race')",
    )
    .bind(a)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
        .bind(ent_a)
        .bind(a)
        .execute(&pool)
        .await?;

    // First bury one predicate and one derivation that references it, as the in-snapshot baseline
    let (rule0, pred0) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind)
         VALUES ($1, $2, 'p0', 'p0', 'relation')",
    )
    .bind(pred0)
    .bind(a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule0)
    .bind(a)
    .bind(pred0)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(a)
    .bind(ent_a)
    .bind(pred0)
    .bind(ent_a)
    .bind(rule0)
    .execute(&pool)
    .await?;

    // Export transaction: read-only REPEATABLE READ, the snapshot is pinned from the first statement
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    export::provenance_integrity(&mut tx, a).await?;
    let _ = export::entities_page(&mut tx, a, None).await?;

    // Midway: another connection commits a new predicate plus a derived fact that references it
    let (rule_late, pred_late, derived_late) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind)
         VALUES ($1, $2, 'p_late', 'p_late', 'relation')",
    )
    .bind(pred_late)
    .bind(a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule_late)
    .bind(a)
    .bind(pred_late)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(derived_late)
    .bind(a)
    .bind(ent_a)
    .bind(pred_late)
    .bind(ent_a)
    .bind(rule_late)
    .execute(&pool)
    .await?;

    // In the snapshot: neither the vocabulary page nor the derivation page should have rows that arrived midway: consistency is for the whole
    let relations = export::relations(&mut tx, a).await?;
    assert!(
        !relations.iter().any(|r| r.id == pred_late),
        "中途提交的谓词不许进这份导出"
    );
    let derived = export::derived_page(&mut tx, a, None).await?;
    assert!(
        !derived.iter().any(|d| d.id == derived_late),
        "中途提交的派生不许进这份导出——也就不会有悬空的 wasGeneratedBy"
    );
    assert_eq!(
        derived.len(),
        1,
        "the one inside the snapshot is still there"
    );
    tx.rollback().await?;
    drop(conn);

    // A new transaction is a new snapshot: both should be present
    let mut tx2 = pool.begin().await?;
    let relations = export::relations(&mut tx2, a).await?;
    assert!(relations.iter().any(|r| r.id == pred_late));
    let derived = export::derived_page(&mut tx2, a, None).await?;
    assert_eq!(derived.len(), 2);
    tx2.rollback().await?;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(a)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}

/// NIL is a legal uuid: the schema does not block it as a primary key. Sorted, it comes first; if the first-page predicate is
/// `id > sentinel`, this row never makes it onto any page. And references pointing at it still resolve:
/// the node is absent while the edge is present, so the export has a dangling reference with no entity behind it. Every read
/// that pages by id (entities, facts, derivations, documents) must page it out on its first page
#[tokio::test]
async fn a_nil_id_row_still_reaches_the_first_page() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;
    let nil = Uuid::nil();

    // Each table that uses an id cursor gets one NIL primary-key row; references all point back at these NIL rows themselves,
    // and the foreign keys are not broken by NIL: they are as legal as any other row
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'nil-e')")
        .bind(nil)
        .bind(f.a)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status, external_key)
         VALUES ($1, $2, 'nil.md', 'nil-sha', 'ready', 'file:///nil.md')",
    )
    .bind(nil)
    .bind(f.a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $3, 0.9)",
    )
    .bind(nil)
    .bind(f.a)
    .bind(nil)
    .bind(f.attr_a)
    .execute(&mut *tx)
    .await?;
    let (pred_r, rule_r) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind)
         VALUES ($1, $2, 'rel_p', 'rel_p', 'relation')",
    )
    .bind(pred_r)
    .bind(f.a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule_r)
    .bind(f.a)
    .bind(pred_r)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $3, $5)",
    )
    .bind(nil)
    .bind(f.a)
    .bind(nil)
    .bind(pred_r)
    .bind(rule_r)
    .execute(&mut *tx)
    .await?;

    assert!(
        export::entities_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|e| e.id == nil),
        "entities 首页漏掉 NIL 行"
    );
    assert!(
        export::documents_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|d| d.id == nil),
        "documents 首页漏掉 NIL 行"
    );
    assert!(
        export::facts_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|x| x.id == nil),
        "facts 首页漏掉 NIL 行"
    );
    assert!(
        export::derived_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|d| d.id == nil),
        "derived 首页漏掉 NIL 行"
    );
    tx.rollback().await?;
    Ok(())
}
