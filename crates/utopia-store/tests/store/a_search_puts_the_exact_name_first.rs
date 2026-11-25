//! Find entities by name: exact-name matches come first, then by degree.
//!
//! Previously it ordered by degree only and cut off at `limit`. Asking for "Apple" while the KB holds an "Apple Store …" with nine more facts pushed the entity
//! called Apple to tenth place — the chat and MCP tools only read the first eight, so it was not found, and reading facts by name
//! read another entity's facts instead. Aliases likewise: "IBM" is a name of International Business Machines (0041), and
//! anyone asking for IBM wants that one.
//!
//! The replay branch binds one more moment parameter and shifts the order by one place, so it is walked through as well.
//!
//! Skipped rather than failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use uuid::Uuid;

async fn entity(pool: &PgPool, kb: Uuid, ty: Uuid, name: &str) -> anyhow::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(kb)
    .bind(ty)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(id)
}

/// One fact, adding one to the degree of `subject`
async fn busy(
    pool: &PgPool,
    kb: Uuid,
    subject: Uuid,
    rel: Uuid,
    object: Uuid,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, recorded_at)
         VALUES ($1, $2, $3, $4, $5, now())",
    )
    .bind(Uuid::now_v7())
    .bind(kb)
    .bind(subject)
    .bind(rel)
    .bind(object)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_search_puts_the_exact_name_first() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb, ty, rel) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'exact-name-search-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'exact-name-search-test')",
    )
    .bind(ws)
    .bind(org)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'exact-name-search-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        sqlx::query(
            "INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'organization', 'Organization')",
        )
        .bind(ty)
        .bind(kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, kind)
             VALUES ($1, $2, 'located_in', 'located in', 'relation')",
        )
        .bind(rel)
        .bind(kb)
        .execute(&pool)
        .await?;
        let city = entity(&pool, kb, ty, "Cupertino").await?;

        // The one called Apple has no facts; each of the nine with Apple in their name has one, so their degrees are all higher
        let apple = entity(&pool, kb, ty, "Apple").await?;
        for i in 1..=9 {
            let store = entity(&pool, kb, ty, &format!("Apple Store {i}")).await?;
            busy(&pool, kb, store, rel, city).await?;
        }
        let (rows, total) =
            utopia_store::graph::search_entities(&pool, kb, "Apple", 8, 0, None, false).await?;
        assert_eq!(total, 10);
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].id, apple, "the entity named exactly Apple comes first");

        // The replay branch: the moment parameter has shifted by one place, and the same name is still first
        let (rows, _) = utopia_store::graph::search_entities(
            &pool,
            kb,
            "Apple",
            8,
            0,
            Some(chrono::Utc::now()),
            false,
        )
        .await?;
        assert_eq!(rows[0].id, apple, "the same holds when replaying a moment");

        // Alias: IBM is a name of International Business Machines, case-insensitive
        let ibm = entity(&pool, kb, ty, "International Business Machines").await?;
        utopia_store::names::record(&pool, kb, ibm, "IBM", None, None).await?;
        for i in 1..=9 {
            let lab = entity(&pool, kb, ty, &format!("IBM Lab {i}")).await?;
            busy(&pool, kb, lab, rel, city).await?;
        }
        let (rows, total) =
            utopia_store::graph::search_entities(&pool, kb, "ibm", 8, 0, None, false).await?;
        assert_eq!(total, 10);
        assert_eq!(rows[0].id, ibm, "an entity whose name is exactly IBM comes first");
        anyhow::Ok(())
    }
    .await;
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
