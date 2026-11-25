//! Attributes on an edge do not enter the edge's identity (0037).
//!
//! The same edge heard again with an amount is the same edge gaining an amount; the same edge with two conflicting amounts
//! keeps the first writer's value and reports the conflict, without overwriting. On the declaring side: only this database's attributes can serve as qualifiers, not the relation itself.

use sqlx::PgPool;
use utopia_store::graph::QualifierWrite;
use uuid::Uuid;

#[tokio::test]
async fn a_qualifier_is_added_to_the_same_edge_and_never_overwritten() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let tag = Uuid::now_v7();
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, $2)")
        .bind(org)
        .bind(format!("qual-{tag}"))
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(org)
        .bind(format!("qual-{tag}"))
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
        .bind(kb)
        .bind(ws)
        .bind(format!("qual-{tag}"))
        .execute(&pool)
        .await?;
    let class = Uuid::now_v7();
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'org', 'Org')")
        .bind(class)
        .bind(kb)
        .execute(&pool)
        .await?;
    // One attribute definition (amount), one relation (investment)
    let amount = utopia_store::ontology::create_relation_type(
        &pool,
        kb,
        "amount",
        "amount",
        "state",
        Default::default(),
        "",
        "attribute",
        &[class],
        &[],
        Some("number"),
        Some("$"),
    )
    .await?;
    let invested = utopia_store::ontology::create_relation_type(
        &pool,
        kb,
        "invested_in",
        "invested in",
        "event",
        Default::default(),
        "",
        "relation",
        &[],
        &[],
        None,
        None,
    )
    .await?;

    // Declaration: attributes can be qualifiers; relations cannot; itself cannot
    utopia_store::ontology::set_relation_qualifiers(&pool, kb, invested, &[amount]).await?;
    let declared = utopia_store::graph::relation_types(&pool, kb)
        .await?
        .into_iter()
        .find(|r| r.id == invested)
        .map(|r| r.qualifiers)
        .unwrap_or_default();
    assert_eq!(declared, vec![amount], "声明要能从关系上读回来");
    assert!(
        utopia_store::ontology::set_relation_qualifiers(&pool, kb, invested, &[invested])
            .await
            .is_err(),
        "关系不能把自己声明成自己的属性"
    );
    let other_rel = utopia_store::ontology::create_relation_type(
        &pool,
        kb,
        "owns",
        "owns",
        "state",
        Default::default(),
        "",
        "relation",
        &[],
        &[],
        None,
        None,
    )
    .await?;
    assert!(
        utopia_store::ontology::set_relation_qualifiers(&pool, kb, invested, &[other_rel])
            .await
            .is_err(),
        "只有属性能当限定项"
    );

    // Append-style declaration: only adds, never deletes; hitting an existing one does not error; the same validation
    utopia_store::ontology::add_relation_qualifier(&pool, kb, invested, amount).await?;
    utopia_store::ontology::add_relation_qualifier(&pool, kb, invested, amount).await?;
    let again = utopia_store::graph::relation_types(&pool, kb)
        .await?
        .into_iter()
        .find(|r| r.id == invested)
        .map(|r| r.qualifiers)
        .unwrap_or_default();
    assert_eq!(again, vec![amount], "追加同一个不重复、不覆盖");
    assert!(
        utopia_store::ontology::add_relation_qualifier(&pool, kb, invested, other_rel)
            .await
            .is_err(),
        "追加式同样只认属性"
    );

    // One edge
    let (a, b, fact) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(a, "Vega"), (b, "Northwind")] {
        sqlx::query(
            "INSERT INTO entities (id, kb_id, type_id, canonical_name) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(kb)
        .bind(class)
        .bind(format!("{name}-{tag}"))
        .execute(&pool)
        .await?;
    }
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $5, 0.9)",
    )
    .bind(fact)
    .bind(kb)
    .bind(a)
    .bind(invested)
    .bind(b)
    .execute(&pool)
    .await?;

    // First time: written; second with the same value: nothing; third with a different value: report the conflict, do not overwrite
    let five = serde_json::json!({ "value": 5000000000.0, "unit": "$" });
    let twenty = serde_json::json!({ "value": 20000000000.0, "unit": "$" });
    assert_eq!(
        utopia_store::graph::upsert_fact_qualifier(&pool, fact, amount, &five).await?,
        QualifierWrite::Set
    );
    assert_eq!(
        utopia_store::graph::upsert_fact_qualifier(&pool, fact, amount, &five).await?,
        QualifierWrite::Same
    );
    assert_eq!(
        utopia_store::graph::upsert_fact_qualifier(&pool, fact, amount, &twenty).await?,
        QualifierWrite::Conflict
    );
    let read = utopia_store::graph::fact_qualifiers_for(&pool, &[fact]).await?;
    let q = &read[&fact];
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].key, "amount");
    assert_eq!(q[0].value, Some(five), "打架时先写者留着，不覆盖");
    assert!(q[0].entity_id.is_none());

    // Cleanup: deleting the database cascades and takes everything along
    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(kb)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}
