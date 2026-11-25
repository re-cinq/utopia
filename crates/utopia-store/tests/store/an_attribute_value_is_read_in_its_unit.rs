//! When an open statement bound to an attribute becomes a typed row, its value is read in the attribute's datatype and unit (`materialize::attribute_value`).
//!
//! Three statements about one company: "registered capital 2.3亿元" bound to a number attribute in units of 万元, "founded 2008年11月"
//! bound to a date attribute, and "registered capital 5000万美元", whose currency does not match. The first two compute 23000 and 2008-11; the third
//! is not written — the statement is still in the open graph, the characters of the source are not lost. Before, all three copied the source characters into the typed graph.
//!
//! Skipped rather than failed when there is no `UTOPIA_DATABASE_URL`. Sets up and tears down its own data, never touching an existing store.

use serde_json::json;
use sqlx::PgPool;
use utopia_store::materialize::materialize;
use uuid::Uuid;

#[tokio::test]
async fn an_attribute_value_is_read_in_its_unit() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'attribute-unit-test')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'attribute-unit-test')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'attribute-unit-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(&pool)
    .await?;

    let run = async {
        let id = Uuid::now_v7;
        let (company, capital, founded, lanshan) = (id(), id(), id(), id());
        let (in_yuan, in_dollars, month) = (id(), id(), id());
        // Only locally generated UUIDs and fixed characters are inserted
        sqlx::raw_sql(&format!(
            r#"
            INSERT INTO entity_types (id, kb_id, key, label, color, shape) VALUES
                ('{company}','{kb}','company','公司','#7fd0ff','circle');
            INSERT INTO relation_types (id, kb_id, key, label, temporal, kind, datatype, unit) VALUES
                ('{capital}','{kb}','registered_capital','注册资本','eternal','attribute','number','万元'),
                ('{founded}','{kb}','founded_on','成立日期','eternal','attribute','date',NULL);
            INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES
                ('{lanshan}','{kb}','{company}','蓝山生物');
            INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, confidence) VALUES
                ('{in_yuan}','{kb}','{lanshan}','{{"value": "2.3亿元"}}','open','注册资本',0.9),
                ('{in_dollars}','{kb}','{lanshan}','{{"value": "5000万美元"}}','open','注册资本',0.9),
                ('{month}','{kb}','{lanshan}','{{"value": "2008年11月"}}','open','成立',0.9);
            INSERT INTO phrase_bindings (id, kb_id, phrase, subject_type_id, object_type_id,
                                         object_is_value, relation_type_id, direction, status) VALUES
                ('{b1}','{kb}','注册资本','{company}',NULL,true,'{capital}','forward','bound'),
                ('{b2}','{kb}','成立','{company}',NULL,true,'{founded}','forward','bound');
            "#,
            b1 = id(),
            b2 = id(),
        ))
        .execute(&pool)
        .await?;

        materialize(&pool, kb).await?;
        let typed: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
            "SELECT predicate_id, object_value FROM facts
              WHERE kb_id = $1 AND layer = 'typed' AND invalidated_at IS NULL
              ORDER BY object_value::text",
        )
        .bind(kb)
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            typed,
            vec![
                (founded, json!({"value": "2008-11"})),
                (capital, json!({"value": 23000.0})),
            ]
        );

        anyhow::Ok(())
    }
    .await;

    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    run
}
