//! A definition written by a person from scratch -- tested against a real database (#562).
//!
//! Three properties:
//! 1. **Written means confirmed**, and it records who wrote it (`written_by`); those that exploration proposed have it empty.
//! 2. **The same (concept, source) is not silently overwritten**: if one already exists, a conflict is reported; changing it goes through `revise`.
//! 3. **Exploration cannot overwrite what a person wrote**: `propose` only refreshes rows that are `proposed` -- the same rule as its refusal to reset a
//!    rejection back to pending.

use sqlx::PgPool;
use uuid::Uuid;

async fn fixture(pool: &PgPool) -> anyhow::Result<(Uuid, Uuid, Uuid)> {
    let (org, ws, kb, ent, user) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'write-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'write-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'write-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'GMV')")
        .bind(ent)
        .bind(kb)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, org_id, email, display_name, password_hash)
         VALUES ($1, $2, $1 || '@w.test', 'w', 'x')",
    )
    .bind(user)
    .bind(org)
    .execute(pool)
    .await?;
    Ok((kb, ent, user))
}

#[tokio::test]
async fn a_written_definition_is_confirmed_and_signed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (kb, ent, user) = fixture(&pool).await?;

    let run = async {
        let id = utopia_store::mappings::create(
            &pool,
            kb,
            ent,
            "warehouse",
            Some("dw.dwd_ord_dtl"),
            Some("sum(amt_pay) FILTER (WHERE pay_st IN (1,2) AND is_test = 0) / 100.0"),
            None,
            Some("CNY"),
            Some("paid, non-test, in yuan"),
            false,
            user,
        )
        .await?;

        // Written this way means confirmed: the next question can read it without going through review again
        let confirmed = utopia_store::mappings::confirmed(&pool, kb, 100).await?;
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].id, id);
        assert_eq!(confirmed[0].written_by, Some(user), "人写的记谁写的");
        assert!(
            utopia_store::mappings::proposed(&pool, kb, 100, 0)
                .await?
                .is_empty(),
            "不该出现在待审队列里"
        );

        // Write another one for the same (concept, source): a conflict, rather than silently overwriting the one that exists
        let again = utopia_store::mappings::create(
            &pool,
            kb,
            ent,
            "warehouse",
            Some("dw.dwd_ord_dtl"),
            Some("sum(amt_total)"),
            None,
            None,
            None,
            false,
            user,
        )
        .await;
        assert!(
            matches!(again, Err(utopia_core::AppError::Conflict(_))),
            "同键第二条该是 Conflict，得到 {again:?}"
        );

        // Exploration cannot overwrite what a person wrote: propose with the same key → the row is left as it is, still confirmed
        utopia_store::mappings::propose(
            &pool,
            kb,
            ent,
            "warehouse",
            Some("orders_v2"),
            Some("sum(amt_total)"),
            None,
            None,
            None,
            false,
        )
        .await?;
        let confirmed = utopia_store::mappings::confirmed(&pool, kb, 100).await?;
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].table_name.as_deref(), Some("dw.dwd_ord_dtl"));
        assert_eq!(confirmed[0].written_by, Some(user));
        // Those proposed by exploration itself have an empty written_by -- the page uses this to tell "written by a person" from "proposed by exploration"
        let ent2 = Uuid::now_v7();
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'Orders')")
            .bind(ent2)
            .bind(kb)
            .execute(&pool)
            .await?;
        utopia_store::mappings::propose(
            &pool,
            kb,
            ent2,
            "warehouse",
            Some("dw.dwd_ord_dtl"),
            Some("count(distinct ord_id)"),
            None,
            None,
            None,
            false,
        )
        .await?;
        let proposed = utopia_store::mappings::proposed(&pool, kb, 100, 0).await?;
        assert_eq!(proposed.len(), 1);
        assert_eq!(proposed[0].written_by, None);
        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Only the knowledge base is deleted, not the org or user -- users are soft-deleted, and the test should not invent an action that does not exist in the product
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    run
}
