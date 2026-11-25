//! Retrieval candidates must **carry their ancestors**, or the prompt contains no generalized base class.
//!
//! Why this needs a database: `ancestors_of` lives entirely inside one recursive SQL query, and whether multiple inheritance and diamonds work out,
//! and whether the same ancestor gets expanded twice, is something `cargo check` says nothing about.
//!
//! What is guarded is this causal chain, found by measurement: vector retrieval naturally favors leaf classes that literally appear in the body text
//!(for a chunk about Sutskever, among 976 classes `researcher` ranks 4th and `person` ranks 359th),
//! and the top 40 contain no generalized base class at all. Two symptoms then appear together: an entity gets classified as
//! `researcher` (in schema.org it is a subclass of `Audience`), and
//! the signature `employee (organization → person)` degrades to `(* → *)`, so the model never sees the direction constraint.
//!
//! Until now this floor was held up by "built-in classes are always present"; once the seed was retired, the criterion hung in the air.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. It creates and tears down its own data.

use sqlx::PgPool;
use uuid::Uuid;

/// Builds a diamond: `researcher → audience → thing`, `corporation → organization → thing`,
/// plus a multi-inherited `agent` (attached under both thing and organization).
///
/// The diamond is the key: a recursion without de-duplication would expand `thing` twice.
async fn seed(pool: &PgPool) -> anyhow::Result<(Uuid, Vec<(&'static str, Uuid)>)> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'floor-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'floor-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'floor-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;

    let keys = [
        "thing",
        "audience",
        "researcher",
        "organization",
        "corporation",
        "agent",
    ];
    let mut ids: Vec<(&'static str, Uuid)> = Vec::new();
    for k in keys {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(kb)
            .bind(k)
            .execute(pool)
            .await?;
        ids.push((k, id));
    }
    let get = |k: &str| ids.iter().find(|(n, _)| *n == k).unwrap().1;
    for (child, parent) in [
        ("audience", "thing"),
        ("researcher", "audience"),
        ("organization", "thing"),
        ("corporation", "organization"),
        // Multiple inheritance + diamond: agent reaches thing by both paths
        ("agent", "thing"),
        ("agent", "organization"),
    ] {
        sqlx::query(
            "INSERT INTO entity_type_parents (child_id, parent_id) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(get(child))
        .bind(get(parent))
        .execute(pool)
        .await?;
    }
    Ok((kb, ids))
}

#[tokio::test]
async fn a_retrieved_leaf_brings_its_ancestors() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // Clean up before starting: an assertion panic would skip teardown
    sqlx::query("DELETE FROM organizations WHERE name = 'floor-test'")
        .execute(&pool)
        .await?;
    let (kb, ids) = seed(&pool).await?;
    let id = |k: &str| ids.iter().find(|(n, _)| *n == k).unwrap().1;

    let run = async {
        // Retrieval only picked up the leaf -- the body text reads "a researcher at the corporation"
        let leaves = vec![id("researcher"), id("corporation")];
        let anc = utopia_store::ontology::ancestors_of(&pool, &leaves).await?;

        for k in ["audience", "thing", "organization"] {
            assert!(anc.contains(&id(k)), "祖先里少了 {k}——地板没补上");
        }
        // An entity is not its own ancestor: the caller merges the two, and a duplicate would be just noise
        for k in ["researcher", "corporation"] {
            assert!(!anc.contains(&id(k)), "{k} 是它自己，不该出现在祖先里");
        }

        // **Diamond de-duplication**: agent reaches thing by two paths, but thing should appear only once
        let anc2 = utopia_store::ontology::ancestors_of(&pool, &[id("agent")]).await?;
        let things = anc2.iter().filter(|x| **x == id("thing")).count();
        assert_eq!(things, 1, "菱形继承把 thing 展开了 {things} 次");
        assert!(anc2.contains(&id("organization")), "多继承的另一条腿丢了");

        // Empty input should not blow up, nor scan the whole table
        assert!(utopia_store::ontology::ancestors_of(&pool, &[])
            .await?
            .is_empty());
        Ok::<(), anyhow::Error>(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE name = 'floor-test'")
        .execute(&pool)
        .await?;
    let _ = kb;
    run
}
