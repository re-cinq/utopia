//! A relation's signature (domain / range) must hold on write paths other than extraction (#190 / #196).
//!
//! Extraction writes correct the direction per #138 or leave the predicate empty, but **merging** swaps out the subject's class, and a guard installed only on extraction
//! gets bypassed by it. Two things are guarded here:
//!
//! 1. After a merge, a fact whose swapped subject now violates the signature → one extra `signature` entry in `axiom_violations`.
//! 2. The consistency check (R0) can also measure signature violations, and the fact is cleared once it is retracted.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data and never touches an existing database.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    company: Uuid,
    person: Uuid,
    /// schema.org's `employee (organization → person)`
    employee: Uuid,
    acme: Uuid,
    alice: Uuid,
    bob: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (company, person, employee) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (acme, alice, bob) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (src, doc, chunk) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'signature-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'signature-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'signature-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (company, "company", "Company"),
        (person, "person", "Person"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ($1, $2, 'employee', 'employee')",
    )
    .bind(employee)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_type_domains (relation_type_id, entity_type_id) VALUES ($1, $2)",
    )
    .bind(employee)
    .bind(company)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_type_ranges (relation_type_id, entity_type_id) VALUES ($1, $2)",
    )
    .bind(employee)
    .bind(person)
    .execute(pool)
    .await?;
    for (id, ty, name) in [
        (acme, company, "Acme"),
        (alice, person, "Alice"),
        (bob, person, "Bob"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(ty)
        .bind(name)
        .execute(pool)
        .await?;
    }
    sqlx::query("INSERT INTO sources (id, kb_id, name) VALUES ($1, $2, 'signature-test')")
        .bind(src)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, source_id, filename, sha256, status)
         VALUES ($1, $2, $3, 'note.md', 'signature', 'ready')",
    )
    .bind(doc)
    .bind(kb)
    .bind(src)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text)
         VALUES ($1, $2, $3, 0, 'Alice is an employee of Acme. Bob is an employee of Alice.')",
    )
    .bind(chunk)
    .bind(kb)
    .bind(doc)
    .execute(pool)
    .await?;
    Ok(Fixture {
        org,
        kb,
        company,
        person,
        employee,
        acme,
        alice,
        bob,
    })
}

async fn live_employee_edges(pool: &PgPool, f: &Fixture) -> anyhow::Result<Vec<(Uuid, Uuid)>> {
    Ok(sqlx::query_as(
        "SELECT subject_id, object_id FROM facts
          WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NULL
          ORDER BY recorded_at",
    )
    .bind(f.kb)
    .bind(f.employee)
    .fetch_all(pool)
    .await?)
}

async fn open_signature_breaks(pool: &PgPool, kb: Uuid) -> anyhow::Result<Vec<Uuid>> {
    Ok(sqlx::query_as::<_, (Uuid,)>(
        "SELECT left_fact FROM axiom_violations
          WHERE kb_id = $1 AND kind = 'signature' AND status = 'open'",
    )
    .bind(kb)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(id,)| id)
    .collect())
}

#[tokio::test]
async fn a_merge_that_breaks_the_signature_is_recorded() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // Acme employee Alice: subject company, object person, matches the signature
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .bind(f.acme)
        .bind(f.employee)
        .bind(f.alice)
        .execute(&pool)
        .await?;
        assert!(open_signature_breaks(&pool, f.kb).await?.is_empty());

        // 1. Merge: merge Acme into Bob (person), so the subject of Acme employee Alice becomes person
        utopia_store::resolution::merge_entities(&pool, f.kb, f.acme, f.bob, None, "test")
            .await?;
        let edges = live_employee_edges(&pool, &f).await?;
        assert_eq!(edges, vec![(f.bob, f.alice)], "the merge moved the subject onto Bob");
        let (moved_fact,): (Uuid,) = sqlx::query_as(
            "SELECT id FROM facts WHERE kb_id = $1 AND predicate_id = $2 AND invalidated_at IS NULL",
        )
        .bind(f.kb)
        .bind(f.employee)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            open_signature_breaks(&pool, f.kb).await?,
            vec![moved_fact],
            "a merge that breaks the signature must show up as an open violation"
        );

        // 2. The consistency check measures it, and retracting the fact clears it
        let report = utopia_store::reasoning::run(&pool, f.kb).await?;
        assert_eq!(report.found, 1);
        assert_eq!(report.inserted, 0, "already recorded by the merge; the check must not duplicate it");
        sqlx::query("UPDATE facts SET invalidated_at = now() WHERE id = $1")
            .bind(moved_fact)
            .execute(&pool)
            .await?;
        let report = utopia_store::reasoning::run(&pool, f.kb).await?;
        assert_eq!(report.found, 0);
        assert_eq!(report.cleared, 1, "a retracted fact takes its violation with it");
        assert!(open_signature_breaks(&pool, f.kb).await?.is_empty());

        // An unclassified entity is not a violation: a subject whose type is NULL has nothing to compare against
        let untyped = Uuid::now_v7();
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'Nobody')")
            .bind(untyped)
            .bind(f.kb)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
             VALUES ($1, $2, $3, $4, $5, 0.9)",
        )
        .bind(Uuid::now_v7())
        .bind(f.kb)
        .bind(untyped)
        .bind(f.employee)
        .bind(f.alice)
        .execute(&pool)
        .await?;
        let mut conn = pool.acquire().await?;
        assert!(
            utopia_store::reasoning::signature_breaks(&mut conn, f.kb, None)
                .await?
                .is_empty(),
            "an untyped subject is unknown, not wrong"
        );
        let _ = (f.company, f.person);
        anyhow::Ok(())
    }
    .await;

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await;
    run
}
