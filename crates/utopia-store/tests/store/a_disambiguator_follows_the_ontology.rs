//! Same-name suffixes come from the ontology, not from a hand-written word list (#299).
//!
//! Why it must hit a database: choosing who gets the suffix is one whole `ORDER BY`. Reverse one of the four sort keys and
//! `cargo check` says nothing; the symptom is two same-named people on the graph displayed as identical
//! "John Smith · Person": it looks like no disambiguation was done, when in fact the word list does not match.
//!
//! The vocabulary uses schema.org's set (`works_for`), **which is exactly what the old word list lacked**:
//! this test must be red on the old implementation, or it proves nothing.

use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    org: Uuid,
    kb: Uuid,
    /// Two John Smiths: one at Acme Robotics, one at St Mary's Hospital
    smith_a: Uuid,
    smith_b: Uuid,
    /// Two Wang Nings: both at Acme, nothing tells them apart
    wang_a: Uuid,
    wang_b: Uuid,
    /// Only one Lin Zhao: unique within the group, so it should not get a suffix
    lin: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (person, organization, document) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    // works_for declares a range; mentioned_in grew out of extraction in passing, and no one said what it points to
    let (works_for, mentioned_in) = (Uuid::now_v7(), Uuid::now_v7());
    let (acme, st_marys) = (Uuid::now_v7(), Uuid::now_v7());
    let (report_a, report_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (smith_a, smith_b) = (Uuid::now_v7(), Uuid::now_v7());
    let (wang_a, wang_b, lin) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'disambig-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'disambig-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'disambig-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    for (id, key, label) in [
        (person, "person", "Person"),
        (organization, "organization", "Organization"),
        (document, "document", "Document"),
    ] {
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(kb)
            .bind(key)
            .bind(label)
            .execute(pool)
            .await?;
    }
    // works_for: a state relation, range declared → identity-bearing
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal)
         VALUES ($1, $2, 'works_for', 'works for', 'state')",
    )
    .bind(works_for)
    .bind(kb)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_type_ranges (relation_type_id, entity_type_id) VALUES ($1, $2)",
    )
    .bind(works_for)
    .bind(organization)
    .execute(pool)
    .await?;
    // mentioned_in: no range declared, and it is an event: **confidence and time both favour it**,
    // the old ordering (confidence DESC, recorded_at DESC) would pick it
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, temporal)
         VALUES ($1, $2, 'mentioned_in', 'mentioned in', 'event')",
    )
    .bind(mentioned_in)
    .bind(kb)
    .execute(pool)
    .await?;

    for (id, type_id, name) in [
        (acme, organization, "Acme Robotics"),
        (st_marys, organization, "St Mary's Hospital"),
        (report_a, document, "2026 Field Report"),
        (report_b, document, "2026 Ward Roster"),
        (smith_a, person, "John Smith"),
        (smith_b, person, "John Smith"),
        (wang_a, person, "Wang Ning"),
        (wang_b, person, "Wang Ning"),
        (lin, person, "Lin Zhao"),
    ] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(type_id)
        .bind(name)
        .execute(pool)
        .await?;
    }

    let fact =
        |subject: Uuid, predicate: Uuid, object: Uuid, confidence: f32, rec: &'static str| {
            sqlx::query(
                "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id,
                                confidence, recorded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(Uuid::now_v7())
            .bind(kb)
            .bind(subject)
            .bind(predicate)
            .bind(object)
            .bind(confidence)
            .bind(rec.parse::<chrono::DateTime<chrono::Utc>>().unwrap())
        };
    // The two John Smiths are told apart: different employers
    fact(smith_a, works_for, acme, 0.8, "2026-01-01T00:00:00Z")
        .execute(pool)
        .await?;
    fact(smith_b, works_for, st_marys, 0.8, "2026-01-01T00:00:00Z")
        .execute(pool)
        .await?;
    // Each also has a later, higher-confidence "was mentioned" row: the old ordering would pick it
    fact(
        smith_a,
        mentioned_in,
        report_a,
        0.99,
        "2026-05-01T00:00:00Z",
    )
    .execute(pool)
    .await?;
    fact(
        smith_b,
        mentioned_in,
        report_b,
        0.99,
        "2026-05-01T00:00:00Z",
    )
    .execute(pool)
    .await?;
    // Both Wang Nings are at Acme: this fact cannot tell them apart
    fact(wang_a, works_for, acme, 0.8, "2026-02-01T00:00:00Z")
        .execute(pool)
        .await?;
    fact(wang_b, works_for, acme, 0.8, "2026-02-01T00:00:00Z")
        .execute(pool)
        .await?;
    fact(lin, works_for, acme, 0.8, "2026-02-01T00:00:00Z")
        .execute(pool)
        .await?;

    Ok(Fixture {
        org,
        kb,
        smith_a,
        smith_b,
        wang_a,
        wang_b,
        lin,
    })
}

async fn suffix(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<String>> {
    let row: (Option<String>,) = sqlx::query_as("SELECT disambiguator FROM entities WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

#[tokio::test]
async fn a_suffix_comes_from_the_ontology_not_a_word_list() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    for name in ["John Smith", "Wang Ning", "Lin Zhao"] {
        utopia_store::resolution::refresh_disambiguators(&pool, f.kb, name).await?;
    }

    // 1. schema.org's vocabulary also recognizes it. The old implementation's word list had no works_for, so both fell back
    //    to "Person": two clearly distinct people looked identical on screen
    assert_eq!(
        suffix(&pool, f.smith_a).await?.as_deref(),
        Some("Acme Robotics")
    );
    assert_eq!(
        suffix(&pool, f.smith_b).await?.as_deref(),
        Some("St Mary's Hospital")
    );

    // 2. The later, higher-confidence mentioned_in did not win: it declares no range, and it is an event.
    //    "Which report this person was written into" is not who they are
    assert_ne!(
        suffix(&pool, f.smith_a).await?.as_deref(),
        Some("2026 Field Report")
    );

    // 3. Two Wang Nings cannot be told apart by the same employer: then both just show the employer, rather than inventing a fake distinction.
    //    The shared employer is still information; it just does not carry the job of distinguishing
    assert_eq!(
        suffix(&pool, f.wang_a).await?.as_deref(),
        Some("Acme Robotics")
    );
    assert_eq!(
        suffix(&pool, f.wang_b).await?.as_deref(),
        Some("Acme Robotics")
    );

    // 4. Unique within the group: nothing needs to be told apart, so no suffix should be attached
    assert_eq!(suffix(&pool, f.lin).await?, None);

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(f.kb)
        .execute(&pool)
        .await?;
    let gone = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    assert_eq!(gone.rows_affected(), 1, "一次性 org 没删掉");
    Ok(())
}
