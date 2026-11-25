//! A self-referencing forward reference within one table is judged at the **commit boundary** (0070 §1b, deferred composite foreign keys).
//!
//! `facts.supersedes`, `facts.from_statement_id`, `relation_types.inverse_of`, and
//! `relation_types.sub_property_of` all point at rows of the same table -- during recovery or bulk load the target may only be flushed after this statement.
//! The composite foreign key `(kb_id, ref)` folds "exists and in the same library" into one constraint, and
//! `DEFERRABLE INITIALLY DEFERRED` makes it re-check at commit, when the whole batch is in place.
//!
//! Writes come in three shapes, each with a different wall:
//!   - **Multi-row INSERT / COPY**: one statement, one batch of rows -- the deferred FK sees the whole batch at the commit boundary;
//!     a cross-library reference fails, a forward chain within one library gets in;
//!   - **Sequential statements**: the deferred FK again defers the check to commit -- within one transaction, "insert the reference first,
//!     the target after" is now legal, and a target that never arrives is blocked at commit;
//!   - **Replica session** (the shape of pg_restore --disable-triggers): this layer has no wall at all --
//!     triggers are off, and the FK's constraint triggers are silent too, so the same-library check does not block the load.
//!     Such bad rows already in storage are caught by the export pre-check and the §0 audit; this file only verifies
//!     the commit boundary of ordinary transactions.

use sqlx::{Acquire, PgPool};
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    a: Uuid,
    b: Uuid,
    ent_a: Uuid,
    ent_b: Uuid,
}

/// Two libraries, one minimal part each: an entity -- the legal form of supersedes also needs them
async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'fwdref-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'fwdref-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for kb in [a, b] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'fwdref-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    }
    let (ent_a, ent_b) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, kb) in [(ent_a, a), (ent_b, b)] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
            .bind(id)
            .bind(kb)
            .execute(pool)
            .await?;
    }
    Ok(Fixture {
        org,
        a,
        b,
        ent_a,
        ent_b,
    })
}

async fn cleanup(pool: &PgPool, f: &Fixture) -> anyhow::Result<()> {
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

/// Multi-row INSERT: the referencing row first, the target row after -- the FK is relaxed at the end of the statement, while a per-row BEFORE check sees
/// the target not yet present. **The deferred constraint at the commit boundary** is where this gets caught: a cross-library one fails,
/// a same-library one gets through
#[tokio::test]
async fn a_multi_row_insert_is_judged_at_commit() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // The referencing row first, the target in another library after it: BEFORE cannot see the target, the deferred constraint sees the whole batch
    let (newer, older) = (Uuid::now_v7(), Uuid::now_v7());
    let r = sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence, supersedes)
         VALUES ($1, $2, $4, $4, 0.9, $3), ($3, $5, $6, $6, 0.9, NULL)",
    )
    .bind(newer)
    .bind(f.a)
    .bind(older)
    .bind(f.ent_a)
    .bind(f.b) // target row lands in another library
    .bind(f.ent_b)
    .execute(&pool)
    .await;
    assert!(r.is_err(), "多行 INSERT 里的跨库 supersedes 必须被拒");

    // The same shape within one library: both rows in the same library, one statement -- legal
    let (newer2, older2) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence, supersedes)
         VALUES ($1, $2, $4, $4, 0.9, $3), ($3, $2, $4, $4, 0.9, NULL)",
    )
    .bind(newer2)
    .bind(f.a)
    .bind(older2)
    .bind(f.ent_a)
    .execute(&pool)
    .await?;

    cleanup(&pool, &f).await
}

/// COPY is the shape of a restore load: relax first, verify once at the statement's commit boundary. A target in another library
/// placed after this batch fails that gate too; a forward chain within one library still gets in
#[tokio::test]
async fn a_copy_batch_is_judged_at_commit() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // Cross-library: newer first, older after, and older belongs to library B -- blocked at commit
    let (newer, older) = (Uuid::now_v7(), Uuid::now_v7());
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let mut copy = tx
        .copy_in_raw(
            "COPY public.facts (id, kb_id, subject_id, object_id, confidence, supersedes) FROM stdin",
        )
        .await?;
    copy.send(
        format!(
            "{newer}\t{a}\t{ent_a}\t{ent_a}\t0.9\t{older}\n{older}\t{b}\t{ent_b}\t{ent_b}\t0.9\t\\N\n",
            a = f.a,
            b = f.b,
            ent_a = f.ent_a,
            ent_b = f.ent_b,
        )
        .into_bytes(),
    )
    .await?;
    copy.finish().await?;
    let r = tx.commit().await;
    assert!(r.is_err(), "COPY 批里的跨库 supersedes 必须在提交时被拦下");

    // Same library: the same forward order, the whole chain is legal
    let (newer2, older2) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = conn.begin().await?;
    let mut copy = tx
        .copy_in_raw(
            "COPY public.facts (id, kb_id, subject_id, object_id, confidence, supersedes) FROM stdin",
        )
        .await?;
    copy.send(
        format!(
            "{newer2}\t{a}\t{ent_a}\t{ent_a}\t0.9\t{older2}\n{older2}\t{a}\t{ent_a}\t{ent_a}\t0.9\t\\N\n",
            a = f.a,
            ent_a = f.ent_a,
        )
        .into_bytes(),
    )
    .await?;
    copy.finish().await?;
    tx.commit().await?;
    drop(conn);

    cleanup(&pool, &f).await
}

/// A forward reference written sequentially is also judged at the commit boundary: the deferred FK makes "insert the reference first, the target after"
/// legal within a transaction; a write whose target never arrives is stopped at COMMIT
#[tokio::test]
async fn a_sequential_forward_reference_is_judged_at_commit() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // The target never arrives: the statement is let through, blocked at commit
    let (newer, older) = (Uuid::now_v7(), Uuid::now_v7());
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence, supersedes)
         VALUES ($1, $2, $3, $3, 0.9, $4)",
    )
    .bind(newer)
    .bind(f.a)
    .bind(f.ent_a)
    .bind(older)
    .execute(&mut *tx)
    .await?;
    let r = tx.commit().await;
    assert!(r.is_err(), "目标始终不到的 supersedes 顺序写，提交时必须报");

    // Target arrives later in the same transaction: the whole chain is complete at commit -- legal
    let (newer2, older2) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = conn.begin().await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence, supersedes)
         VALUES ($1, $2, $3, $3, 0.9, $4)",
    )
    .bind(newer2)
    .bind(f.a)
    .bind(f.ent_a)
    .bind(older2)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
         VALUES ($1, $2, $3, $3, 0.9)",
    )
    .bind(older2)
    .bind(f.a)
    .bind(f.ent_a)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    drop(conn);

    cleanup(&pool, &f).await
}

/// A cross-library supersedes set by an UPDATE: the target already exists, so the per-row BEFORE check can see it --
/// it is reported right away; the layer that really slips past is the one the deferred constraint catches at commit
#[tokio::test]
async fn an_update_to_a_foreign_supersedes_fails() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    let (fa, fb) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
         VALUES ($1, $2, $3, $3, 0.9), ($4, $5, $6, $6, 0.9)",
    )
    .bind(fa)
    .bind(f.a)
    .bind(f.ent_a)
    .bind(fb)
    .bind(f.b)
    .bind(f.ent_b)
    .execute(&pool)
    .await?;

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let upd = sqlx::query("UPDATE facts SET supersedes = $2 WHERE id = $1")
        .bind(fa)
        .bind(fb)
        .execute(&mut *tx)
        .await;
    if upd.is_ok() {
        let r = tx.commit().await;
        assert!(r.is_err(), "UPDATE 装上的跨库 supersedes 最迟在提交时要报");
    }

    cleanup(&pool, &f).await
}

/// from_statement_id is the second self-referencing edge on the same table: the write shape with the statement row at the end of the batch and the typed fact at its head
/// is blocked at commit in the same way when the statement is in another library
#[tokio::test]
async fn a_from_statement_is_judged_at_commit() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // The typed fact first, the statement in another library after: BEFORE cannot see the target, the deferred constraint sees the whole batch
    let (typed, stmt) = (Uuid::now_v7(), Uuid::now_v7());
    let r = sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence,
                            layer, phrase, from_statement_id)
         VALUES ($1, $2, $4, $4, 0.9, 'typed', NULL, $3),
                ($3, $5, $6, $6, 0.9, 'open', 'joined', NULL)",
    )
    .bind(typed)
    .bind(f.a)
    .bind(stmt)
    .bind(f.ent_a)
    .bind(f.b) // statement row lands in another library
    .bind(f.ent_b)
    .execute(&pool)
    .await;
    assert!(
        r.is_err(),
        "多行 INSERT 里的跨库 from_statement_id 必须被拒"
    );

    // The same shape within one library: typed fact first, same-library statement after -- legal
    let (typed2, stmt2) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence,
                            layer, phrase, from_statement_id)
         VALUES ($1, $2, $4, $4, 0.9, 'typed', NULL, $3),
                ($3, $2, $4, $4, 0.9, 'open', 'joined', NULL)",
    )
    .bind(typed2)
    .bind(f.a)
    .bind(stmt2)
    .bind(f.ent_a)
    .execute(&pool)
    .await?;

    cleanup(&pool, &f).await
}

/// The same boundary for the relation self-reference: the inverse_of / sub_property_of reference row first,
/// the target row after -- a cross-library one is blocked at commit, a same-library one is let through
#[tokio::test]
async fn relation_self_links_are_judged_at_commit() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let f = seed(&pool).await?;

    // inverse_of: in one statement the predicate in library A comes first and the target in library B after -- reported at commit
    let (inv, tgt) = (Uuid::now_v7(), Uuid::now_v7());
    let r = sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, inverse_of)
         VALUES ($1, $2, 'inv', 'inv', $3), ($3, $4, 'tgt', 'tgt', NULL)",
    )
    .bind(inv)
    .bind(f.a)
    .bind(tgt)
    .bind(f.b)
    .execute(&pool)
    .await;
    assert!(r.is_err(), "多行 INSERT 里的跨库 inverse_of 必须被拒");

    // sub_property_of in the same library, target row after: legal
    let (child, parent) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, sub_property_of)
         VALUES ($1, $2, 'child', 'child', $3), ($3, $2, 'parent', 'parent', NULL)",
    )
    .bind(child)
    .bind(f.a)
    .bind(parent)
    .execute(&pool)
    .await?;

    cleanup(&pool, &f).await
}
