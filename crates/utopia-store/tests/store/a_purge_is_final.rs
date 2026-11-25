//! #268 second half: a real purge is the end point.
//!
//! Deleting is an event and reversible; purge really wipes the content, and is open only for deleted documents. Five things are guarded here:
//!
//! 1. **Nothing undeleted can be purged.** Purging a live document reports an error.
//! 2. **What is purged is the content, not the record.** Chunk rows, evidence and version rows are gone; the document row remains, marked with `purged_at`,
//!    with `external_key` cleared; facts stay voided; the deletion-ledger row remains.
//! 3. **Original content still referenced elsewhere is not handed out.** A fingerprint of a historical version used by another document is not on the list to delete;
//!    only the fingerprints this document owns exclusively are handed to the caller to delete the files.
//! 4. **Once purged, it does not come back.** restore reports an error, and purging again also reports an error.
//! 5. **The identity is released.** The same content with the same external_key can come in again — it is a new document, not
//!    a revival; the "deleted" view lists only tombstones that have not been purged.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use utopia_store::documents;
use uuid::Uuid;

struct Fx {
    org: Uuid,
    user: Uuid,
    kb: Uuid,
    src: Uuid,
    etype: Uuid,
    rel: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fx> {
    let (org, ws, kb, user) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (src, etype, rel) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'purge-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'purge-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $1 || '@purge.test', 'p', 'x')",
    )
    .bind(user)
    .bind(org)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'purge-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO sources (id, kb_id, kind, name) VALUES ($1, $2, 'folder', 'f')")
        .bind(src)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'thing', 'Thing')",
    )
    .bind(etype)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'knows', 'knows')",
    )
    .bind(rel)
    .bind(kb)
    .execute(pool)
    .await?;
    Ok(Fx {
        org,
        user,
        kb,
        src,
        etype,
        rel,
    })
}

async fn document(
    pool: &PgPool,
    f: &Fx,
    name: &str,
    sha: &str,
    key: Option<&str>,
) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status, external_key)
         VALUES ($1, $2, $3, $4, $5, 'ready', $6)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(f.src)
    .bind(name)
    .bind(sha)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn chunk(pool: &PgPool, f: &Fx, doc: Uuid) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text) VALUES ($1, $2, $3, 0, 'gone soon')",
    )
    .bind(id)
    .bind(f.kb)
    .bind(doc)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn version(pool: &PgPool, doc: Uuid, sha: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256) VALUES ($1, $2, 1, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(doc)
    .bind(sha)
    .execute(pool)
    .await?;
    Ok(())
}

async fn entity(pool: &PgPool, f: &Fx, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(f.etype)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(id)
}

async fn fact(pool: &PgPool, f: &Fx, s: Uuid, o: Uuid, chunk: Uuid) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(id)
    .bind(f.kb)
    .bind(s)
    .bind(f.rel)
    .bind(o)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO fact_evidence (fact_id, chunk_id, quote) VALUES ($1, $2, 'q')")
        .bind(id)
        .bind(chunk)
        .execute(pool)
        .await?;
    Ok(id)
}

async fn count(pool: &PgPool, sql: &str, id: Uuid) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(sql).bind(id).fetch_one(pool).await?)
}

/// (deleted_at set, purged_at set, external_key)
async fn tombstone(pool: &PgPool, id: Uuid) -> anyhow::Result<(bool, bool, Option<String>)> {
    Ok(sqlx::query_as(
        "SELECT deleted_at IS NOT NULL, purged_at IS NOT NULL, external_key
           FROM documents WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn a_purge_is_final() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    // Original content is addressed by content and shared across stores: the fingerprint carries this run's random suffix, so it does not collide with leftovers in the store
    let tag = Uuid::now_v7().simple().to_string();
    let (sha_a, sha_old, sha_b) = (
        format!("sha-a-{tag}"),
        format!("sha-old-{tag}"),
        format!("sha-b-{tag}"),
    );
    let run = async {
        // A: current content sha-a, historical version sha-old; C's current content is also sha-old — that original is not exclusively owned
        let a = document(&pool, &f, "a.md", &sha_a, Some("key-a")).await?;
        version(&pool, a, &sha_old).await?;
        let _c = document(&pool, &f, "c.md", &sha_old, None).await?;
        // B: deleted but not purged, it should stay in the "deleted" view
        let b = document(&pool, &f, "b.md", &sha_b, None).await?;
        let a1 = chunk(&pool, &f, a).await?;
        let (x, y) = (entity(&pool, &f, "X").await?, entity(&pool, &f, "Y").await?);
        let only_a = fact(&pool, &f, x, y, a1).await?;

        // 1. Cannot purge what is not deleted
        assert!(documents::purge(&pool, f.kb, a).await.is_err());

        documents::delete(&pool, f.kb, a, Some(f.user)).await?;
        documents::delete(&pool, f.kb, b, Some(f.user)).await?;

        // 2 + 3. Purge content, keep records; hand out only the exclusively owned original
        let report = documents::purge(&pool, f.kb, a).await?;
        assert_eq!(report.chunks, 1);
        assert_eq!(
            report.blobs,
            vec![sha_a.clone()],
            "sha-old is still c's content, so it stays"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM chunks WHERE document_id = $1",
                a
            )
            .await?,
            0
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM fact_evidence WHERE fact_id = $1",
                only_a
            )
            .await?,
            0,
            "the quote goes with the chunk"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM document_versions WHERE document_id = $1",
                a
            )
            .await?,
            0
        );
        assert_eq!(tombstone(&pool, a).await?, (true, true, None));
        let retired: bool =
            sqlx::query_scalar("SELECT invalidated_at IS NOT NULL FROM facts WHERE id = $1")
                .bind(only_a)
                .fetch_one(&pool)
                .await?;
        assert!(retired, "the fact stays retired");
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM document_deletions WHERE document_id = $1",
                a
            )
            .await?,
            1,
            "the deletion stays on record"
        );

        // 4. Does not come back
        assert!(documents::restore(&pool, f.kb, a).await.is_err());
        assert!(documents::purge(&pool, f.kb, a).await.is_err());

        // 5. Identity released: the same content and key coming in again is a new document; the "deleted" view has only B
        let again = documents::create(
            &pool,
            f.kb,
            "a.md",
            "text/markdown",
            9,
            &sha_a,
            Some(f.src),
            None,
            Some("key-a"),
        )
        .await?;
        assert_ne!(again.id, a, "a new document, not a revival");
        assert!(again.deleted_at.is_none());
        let deleted = documents::page(&pool, f.kb, None, None, None, true, 50, 0).await?;
        let ids: Vec<Uuid> = deleted.docs.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![b], "purged tombstones are not listed");
        assert_eq!(deleted.deleted, 1);
        let live = documents::page(&pool, f.kb, None, None, None, false, 50, 0).await?;
        let ids: Vec<Uuid> = live.docs.iter().map(|d| d.id).collect();
        assert!(ids.contains(&again.id));
        assert!(!ids.contains(&a) && !ids.contains(&b));
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Deleting an organization does not cascade into the store: the store deletes itself, so no tombstones are left in the shared test store
    let _ = sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await;
    run
}
