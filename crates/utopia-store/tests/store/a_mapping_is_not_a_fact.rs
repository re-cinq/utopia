//! Two hard properties of the semantic-layer mapping — run against a real store (see `docs/decisions/0011`).
//!
//! Both are things bought by moving out of the ledger, and they are exactly what could not be done before:
//!
//! 1. **There is only one row per (concept, source).** Previously this uniqueness hid inside `object_value`,
//!    a JSONB blob the database could not see, so it could only be closed explicitly by the confirmation flow — that is, by process
//!    rather than by constraint. Now it is the primary key.
//! 2. **A judged row is not flipped back to pending by the next exploration.** `ontology_proposals` hit the same
//!    pitfall (on that side): a rerun will inevitably compute again the one that was rejected, and unless blocked it is the same as wiping
//!    a person's rejection on every run.

use sqlx::PgPool;
use uuid::Uuid;

async fn fixture(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let (org, ws, kb, ent) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'map-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'map-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'map-test')")
        .bind(kb)
        .bind(ws)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, '营收')")
        .bind(ent)
        .bind(kb)
        .execute(pool)
        .await?;
    Ok((org, kb, ent))
}

#[tokio::test]
async fn one_concept_one_source_one_mapping() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (_org, kb, ent) = fixture(&pool).await?;

    let run = async {
        let p = |t: &'static str| {
            utopia_store::mappings::propose(
                &pool,
                kb,
                ent,
                "warehouse",
                Some(t),
                None,
                None,
                None,
                None,
                false,
            )
        };
        let (a, wrote_a) = p("orders").await?;
        let (b, wrote_b) = p("orders_v2").await?;
        assert_eq!(
            a, b,
            "the same (concept, source) should be one row, not two"
        );
        assert!(
            wrote_a && wrote_b,
            "first insert and second refresh both count as writes"
        );

        let got = utopia_store::mappings::proposed(&pool, kb, 100, 0).await?;
        assert_eq!(got.len(), 1, "there should be only one");
        assert_eq!(
            got[0].table_name.as_deref(),
            Some("orders_v2"),
            "重跑探索该刷新定义"
        );

        // A different source is a different row: the same concept having different definitions on different sources is supported on purpose
        utopia_store::mappings::propose(
            &pool,
            kb,
            ent,
            "lakehouse",
            Some("f_orders"),
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        assert_eq!(
            utopia_store::mappings::proposed(&pool, kb, 100, 0)
                .await?
                .len(),
            2,
            "不同源该各有一条"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // **Delete only the knowledge base, not org/user.** Users are soft-deleted, and production code has no
    // `DELETE FROM users` — the test should not invent an action the product does not have, otherwise
    // a foreign-key constraint it runs into would be the fixture's own problem and get mistaken for a product defect (this actually happened)
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}

#[tokio::test]
async fn a_rejected_mapping_does_not_come_back() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, kb, ent) = fixture(&pool).await?;

    let run = async {
        let user = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users (id, org_id, email, display_name, password_hash)
             VALUES ($1, $2, $1 || '@m.test', 'm', 'x')",
        )
        .bind(user)
        .bind(org)
        .execute(&pool)
        .await?;

        let (id, _) = utopia_store::mappings::propose(
            &pool,
            kb,
            ent,
            "warehouse",
            Some("orders"),
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        utopia_store::mappings::decide(&pool, kb, id, "rejected", user).await?;

        // The next exploration will compute the same one again — it should not be flipped back to pending, and it does not count as a write
        let (_, written) = utopia_store::mappings::propose(
            &pool,
            kb,
            ent,
            "warehouse",
            Some("orders"),
            None,
            None,
            None,
            None,
            false,
        )
        .await?;
        assert!(!written, "a proposal that collides with a decision is not a write, the exploration ledger records it as decided");
        assert!(
            utopia_store::mappings::proposed(&pool, kb, 100, 0)
                .await?
                .is_empty(),
            "拒绝过的不该重新排队"
        );
        // Same for a confirmed one: exploration must not overwrite it
        assert!(
            utopia_store::mappings::confirmed(&pool, kb, 100)
                .await?
                .is_empty(),
            "拒绝的也不该出现在确认列表里"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // **Delete only the knowledge base, not org/user.** Users are soft-deleted, and production code has no
    // `DELETE FROM users` — the test should not invent an action the product does not have, otherwise
    // a foreign-key constraint it runs into would be the fixture's own problem and get mistaken for a product defect (this actually happened)
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}
