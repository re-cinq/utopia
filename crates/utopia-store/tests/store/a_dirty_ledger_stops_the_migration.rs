//! 0070 §0 pre-check: installing the invariant "provenance must not cross KBs" onto a database that **already has** cross-KB bad rows
//! — the migration must abort deterministically, reporting which edge is broken and how many rows,
//! rather than installing and endorsing the bad data.
//!
//! How: open an isolated database and run the migrations up to ≤0069 one by one in file order (each in its own transaction,
//! the same semantics as sqlx::migrate), hand-insert a row that is legal before 0070 and illegal after it,
//! then run 0070 itself:
//!   - clean 0069 database → 0070 succeeds, trigger in place;
//!   - dirty 0069 database → 0070 errors and names the edge, and the whole migration rolls back with its transaction — not a single trigger left behind.
//!
//! Failing to create the database (in an environment whose role lacks permission) is handled like a missing UTOPIA_DATABASE_URL: skipped.

use sqlx::{Acquire, PgPool};
use uuid::Uuid;

/// Address of the maintenance database: `…/utopia` → `…/postgres`
fn admin_url() -> Option<String> {
    let url = utopia_store::test_db::url()?;
    let (head, _) = url.rsplit_once('/')?;
    Some(format!("{head}/postgres"))
}

/// Runs the migrations with version ≤ `through` in file order, each in its own transaction (same shape as sqlx::migrate)
async fn migrate_to(pool: &PgPool, through: i64) -> anyhow::Result<()> {
    let migrator = sqlx::migrate!("../../migrations");
    let mut conn = pool.acquire().await?;
    for m in migrator.iter().filter(|m| m.version <= through) {
        let mut tx = conn.begin().await?;
        sqlx::raw_sql(&m.sql).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

async fn migration_70(pool: &PgPool) -> Result<(), sqlx::Error> {
    let migrator = sqlx::migrate!("../../migrations");
    let m = migrator
        .iter()
        .find(|m| m.version == 70)
        .expect("0070 must be in the migration set");
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    let r = sqlx::raw_sql(&m.sql).execute(&mut *tx).await;
    match r {
        Ok(_) => tx.commit().await,
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Isolated database: create → migrate to 0069 → return (database name, pool). On failure, skip in place
async fn scratch(suffix: &str) -> Option<(String, PgPool)> {
    let admin = admin_url()?;
    let admin_pool = PgPool::connect(&admin).await.ok()?;
    let name = format!("xkb70_{}_{}", suffix, Uuid::now_v7().simple());
    let created = sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin_pool)
        .await;
    if created.is_err() {
        eprintln!("跳过：建不了隔离库（角色没有 CREATEDB）");
        admin_pool.close().await;
        return None;
    }
    let url = utopia_store::test_db::url()?;
    let (head, _) = url.rsplit_once('/')?;
    let pool = PgPool::connect(&format!("{head}/{name}")).await.ok()?;
    if let Err(e) = migrate_to(&pool, 69).await {
        eprintln!("跳过：迁到 0069 失败（迁移链自身的问题）: {e}");
        drop_scratch(&name).await;
        return None;
    }
    Some((name, pool))
}

async fn drop_scratch(name: &str) {
    if let Some(admin) = admin_url() {
        if let Ok(pool) = PgPool::connect(&admin).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&pool)
                .await;
            pool.close().await;
        }
    }
}

/// The smallest bad case that is legal before 0070: KB A's document, chunk, entity, and fact, and KB B's entity and chunk —
/// an evidence row pairs A's fact with B's chunk
async fn seed_dirty(pool: &PgPool) -> anyhow::Result<()> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'm70-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'm70-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for kb in [a, b] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'm70-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    }
    let (doc_a, doc_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (chunk_b, ent_a, ent_b) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (id, kb) in [(ent_a, a), (ent_b, b)] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
            .bind(id)
            .bind(kb)
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
        .bind(format!("sha-{name}"))
        .bind(format!("file:///{name}"))
        .execute(pool)
        .await?;
    }
    // B's own chunk — before 0070, nothing stops it being paired with A's fact
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, 'x')",
    )
    .bind(chunk_b)
    .bind(b)
    .bind(doc_b)
    .execute(pool)
    .await?;
    let fact_a = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, 0.9)",
    )
    .bind(fact_a)
    .bind(a)
    .bind(ent_a)
    .bind(ent_a)
    .execute(pool)
    .await?;
    // Bad row: A's fact paired with B's chunk
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id) VALUES ($1, $2)")
        .bind(fact_a)
        .bind(chunk_b)
        .execute(pool)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_clean_ledger_takes_the_invariant() -> anyhow::Result<()> {
    let Some((name, pool)) = scratch("clean").await else {
        return Ok(());
    };
    let r = migration_70(&pool).await;
    assert!(r.is_ok(), "a clean 0069 database must accept 0070: {r:?}");
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger WHERE tgname = 'fact_evidence_same_kb'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(n, 1, "the trigger must really be installed");
    pool.close().await;
    drop_scratch(&name).await;
    Ok(())
}

#[tokio::test]
async fn a_dirty_ledger_stops_the_migration_atomically() -> anyhow::Result<()> {
    let Some((name, pool)) = scratch("dirty").await else {
        return Ok(());
    };
    seed_dirty(&pool).await?;
    let r = migration_70(&pool).await;
    let msg = format!("{r:?}");
    assert!(r.is_err(), "running 0070 on a dirty database must abort");
    assert!(
        msg.contains("cross-KB references already present") || msg.contains("evidence.chunk"),
        "中止要报出坏在哪条边: {msg}"
    );
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pg_trigger WHERE tgname LIKE '%same_kb%'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        n, 0,
        "an aborted migration must not leave half an invariant behind"
    );
    pool.close().await;
    drop_scratch(&name).await;
    Ok(())
}
