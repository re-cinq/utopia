//! #516: purge decides all fingerprints in one statement, with the same answer as asking one by one.
//!
//! Original content is addressed by content and shared across stores; a fingerprint is handed to the caller to delete the file only when **nothing anywhere** still references it.
//! The two directions of a wrong judgment are asymmetric: keeping too much leaks disk, handing out too much deletes original content someone else still uses. Before, each fingerprint
//! was asked of the store separately, and the more questions, the longer the lock was held; now one unnest asks them all, and what is guarded here is that the answer does not move:
//!
//! 1. A fingerprint another document still holds is not handed out.
//! 2. One held by a document in another store is not handed out either — that question deliberately omits kb_id.
//! 3. A fingerprint only an old version ever held, and that no one holds now, is handed out: the judgment happens after the version rows are deleted.
//! 4. A purged document does not count as a reference: its row remains, but the fingerprint no longer belongs to it.
//! 5. The same fingerprint appearing in several versions appears on the list only once.
//! 6. The answer of one statement equals the answer of asking one by one; this is the correctness argument for the whole change.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use sqlx::PgPool;
use utopia_store::documents;
use uuid::Uuid;

struct Fx {
    pool: PgPool,
    org: Uuid,
    ws: Uuid,
    kb: Uuid,
    tag: String,
}

async fn seed() -> anyhow::Result<Option<Fx>> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(None);
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'purge-blobs-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'purge-blobs-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    let kb = base(&pool, ws).await?;
    // Fingerprint carries this run's random suffix, so it does not collide with leftovers in the store
    let tag = Uuid::now_v7().simple().to_string();
    Ok(Some(Fx {
        pool,
        org,
        ws,
        kb,
        tag,
    }))
}

async fn base(pool: &PgPool, ws: Uuid) -> anyhow::Result<Uuid> {
    let kb = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'purge-blobs-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    Ok(kb)
}

impl Fx {
    fn sha(&self, name: &str) -> String {
        format!("sha-{name}-{}", self.tag)
    }

    async fn document(&self, kb: Uuid, name: &str, sha: &str) -> anyhow::Result<Uuid> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256, status)
             VALUES ($1, $2, $3, $4, 'ready')",
        )
        .bind(id)
        .bind(kb)
        .bind(name)
        .bind(sha)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn version(&self, doc: Uuid, version: i32, sha: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO document_versions (id, document_id, version, sha256) VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(doc)
        .bind(version)
        .bind(sha)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn purge(&self, kb: Uuid, doc: Uuid) -> anyhow::Result<Vec<String>> {
        documents::delete(&self.pool, kb, doc, None).await?;
        Ok(documents::purge(&self.pool, kb, doc).await?.blobs)
    }

    /// The old method: ask the store once per fingerprint; item 6 takes it as the ground truth
    async fn judged_one_by_one(&self, shas: &[String]) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for s in shas {
            let (referenced,): (bool,) = sqlx::query_as(
                "SELECT EXISTS (SELECT 1 FROM documents WHERE sha256 = $1 AND purged_at IS NULL)
                     OR EXISTS (SELECT 1 FROM document_versions WHERE sha256 = $1)",
            )
            .bind(s)
            .fetch_one(&self.pool)
            .await?;
            if !referenced {
                out.push(s.clone());
            }
        }
        Ok(out)
    }

    async fn cleanup(self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_blob_another_document_still_holds_is_never_collected() -> anyhow::Result<()> {
    let Some(f) = seed().await? else {
        return Ok(());
    };
    // Two live documents in the same store cannot share a sha (documents_kb_sha_idx), so the other
    // holder in the same store can only be someone else's historical version
    let shared = f.sha("shared");
    let a = f.document(f.kb, "a.md", &shared).await?;
    let b = f.document(f.kb, "b.md", &f.sha("b-now")).await?;
    f.version(b, 1, &shared).await?;
    assert!(
        f.purge(f.kb, a).await?.is_empty(),
        "b still holds the blob in a version"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_blob_shared_across_knowledge_bases_is_never_collected() -> anyhow::Result<()> {
    let Some(f) = seed().await? else {
        return Ok(());
    };
    let shared = f.sha("shared");
    let other_kb = base(&f.pool, f.ws).await?;
    let a = f.document(f.kb, "a.md", &shared).await?;
    let _elsewhere = f.document(other_kb, "e.md", &shared).await?;
    assert!(
        f.purge(f.kb, a).await?.is_empty(),
        "the holder in another base counts; the check carries no kb_id on purpose"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_blob_only_an_old_version_held_is_collected() -> anyhow::Result<()> {
    let Some(f) = seed().await? else {
        return Ok(());
    };
    let (current, old) = (f.sha("current"), f.sha("old"));
    let a = f.document(f.kb, "a.md", &current).await?;
    f.version(a, 1, &old).await?;
    let mut blobs = f.purge(f.kb, a).await?;
    blobs.sort();
    let mut expected = vec![current, old];
    expected.sort();
    assert_eq!(
        blobs, expected,
        "the old version's blob is judged after the version row is gone"
    );
    f.cleanup().await
}

#[tokio::test]
async fn a_purged_document_no_longer_holds_its_blob() -> anyhow::Result<()> {
    let Some(f) = seed().await? else {
        return Ok(());
    };
    // Two live documents with the same sha can only be in two stores (documents_kb_sha_idx)
    let shared = f.sha("shared");
    let other_kb = base(&f.pool, f.ws).await?;
    let x = f.document(f.kb, "x.md", &shared).await?;
    let y = f.document(other_kb, "y.md", &shared).await?;
    assert!(f.purge(f.kb, x).await?.is_empty(), "y still holds it");
    assert_eq!(
        f.purge(other_kb, y).await?,
        vec![shared],
        "x's row is still there but purged, so it is not a holder"
    );
    f.cleanup().await
}

#[tokio::test]
async fn every_sha_is_judged_exactly_once() -> anyhow::Result<()> {
    let Some(f) = seed().await? else {
        return Ok(());
    };
    let repeated = f.sha("repeated");
    let a = f.document(f.kb, "a.md", &repeated).await?;
    f.version(a, 1, &repeated).await?;
    f.version(a, 2, &repeated).await?;
    assert_eq!(
        f.purge(f.kb, a).await?,
        vec![repeated],
        "one blob, one line, however many versions carried it"
    );
    f.cleanup().await
}

#[tokio::test]
async fn the_batched_answer_equals_the_one_by_one_answer() -> anyhow::Result<()> {
    let Some(f) = seed().await? else {
        return Ok(());
    };
    let other_kb = base(&f.pool, f.ws).await?;
    // A holds five fingerprints: one current, four historical; two of them are held elsewhere, and one a purged document once held
    let (own, shared_doc, shared_far, held_by_purged, old_only) = (
        f.sha("own"),
        f.sha("shared-doc"),
        f.sha("shared-far"),
        f.sha("held-by-purged"),
        f.sha("old-only"),
    );
    let a = f.document(f.kb, "a.md", &own).await?;
    f.version(a, 1, &shared_doc).await?;
    f.version(a, 2, &shared_far).await?;
    f.version(a, 3, &held_by_purged).await?;
    f.version(a, 4, &old_only).await?;
    let _b = f.document(f.kb, "b.md", &shared_doc).await?;
    let _far = f.document(other_kb, "far.md", &shared_far).await?;
    let p = f.document(f.kb, "p.md", &held_by_purged).await?;
    assert!(
        f.purge(f.kb, p).await?.is_empty(),
        "a still holds it in a version"
    );

    let shas: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT sha256 FROM document_versions WHERE document_id = $1
         UNION SELECT sha256 FROM documents WHERE id = $1",
    )
    .bind(a)
    .fetch_all(&f.pool)
    .await?;
    let mut batched = f.purge(f.kb, a).await?;
    // The store state after commit is the same as when judged inside the transaction: version rows deleted, document marked purged
    let mut one_by_one = f.judged_one_by_one(&shas).await?;
    batched.sort();
    one_by_one.sort();
    assert_eq!(batched, one_by_one);
    let mut expected = vec![own, held_by_purged, old_only];
    expected.sort();
    assert_eq!(batched, expected);
    f.cleanup().await
}
