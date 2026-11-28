//! The relation listing a pushed document's extraction reads must not count the base's facts (RC-011).
//!
//! On the internal cluster on 2026-11-27 the counted listing ran after every pushed document: a parallel
//! sequential scan of all 15 million facts, 2.3 GB read from disk, 42.7 s under load, and 38% of all active
//! database sessions. The caller only reads each relation's name, domain and range.
//!
//! Both tests run on temp copies of the tables in one session, so the shared database is untouched.
use super::{relation_type_views, relation_type_views_sql, relation_type_views_uncounted};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

/// A one-connection pool whose session holds temp copies of the tables the listing reads.
async fn session() -> anyhow::Result<Option<PgPool>> {
    let Some(url) = crate::test_db::url() else {
        return Ok(None);
    };
    let pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect(&url)
        .await?;
    for table in [
        "relation_types",
        "relation_type_domains",
        "relation_type_ranges",
        "relation_type_qualifiers",
        "facts",
    ] {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL)"
        ))
        .execute(&pool)
        .await?;
    }
    Ok(Some(pool))
}

/// Whether the plan of `sql` reads the `facts` table.
async fn reads_facts(pool: &PgPool, sql: &str) -> anyhow::Result<bool> {
    let plan: serde_json::Value = sqlx::query_scalar(&format!("EXPLAIN (FORMAT JSON) {sql}"))
        .persistent(false)
        .bind(Uuid::now_v7())
        .fetch_one(pool)
        .await?;
    Ok(mentions_facts(&plan[0]["Plan"]))
}

fn mentions_facts(node: &serde_json::Value) -> bool {
    node["Relation Name"].as_str() == Some("facts")
        || node["Plans"]
            .as_array()
            .is_some_and(|children| children.iter().any(mentions_facts))
}

#[tokio::test]
async fn the_uncounted_listing_reads_no_fact() -> anyhow::Result<()> {
    let Some(pool) = session().await? else {
        return Ok(());
    };
    assert!(
        reads_facts(&pool, &relation_type_views_sql(true)).await?,
        "the counted listing is the one that reads facts"
    );
    assert!(
        !reads_facts(&pool, &relation_type_views_sql(false)).await?,
        "the uncounted listing must not read facts"
    );
    Ok(())
}

#[tokio::test]
async fn the_uncounted_listing_names_the_same_relations_with_usage_zero() -> anyhow::Result<()> {
    let Some(pool) = session().await? else {
        return Ok(());
    };
    let (kb, other_kb, works_with, owns, class) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES
           ($1, $3, 'works_with', 'worksWith'), ($2, $3, 'owns', 'owns'),
           (gen_random_uuid(), $4, 'elsewhere', 'elsewhere')",
    )
    .bind(works_with)
    .bind(owns)
    .bind(kb)
    .bind(other_kb)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO relation_type_domains (relation_type_id, entity_type_id) VALUES ($1, $2)",
    )
    .bind(works_with)
    .bind(class)
    .execute(&pool)
    .await?;
    // Three live typed facts on worksWith, none on owns
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
         SELECT gen_random_uuid(), $1, gen_random_uuid(), $2, gen_random_uuid() FROM generate_series(1, 3)",
    )
    .bind(kb)
    .bind(works_with)
    .execute(&pool)
    .await?;

    let counted = relation_type_views(&pool, kb).await?;
    let uncounted = relation_type_views_uncounted(&pool, kb).await?;

    let usage = |views: &[utopia_core::models::RelationTypeView]| -> Vec<(String, i64)> {
        views.iter().map(|v| (v.key.clone(), v.usage)).collect()
    };
    assert_eq!(
        usage(&counted),
        [("owns".to_string(), 0), ("works_with".to_string(), 3)]
    );
    assert_eq!(
        usage(&uncounted),
        [("owns".to_string(), 0), ("works_with".to_string(), 0)]
    );
    // Everything but the count is the same, in the same order
    let without_usage = |views: &[utopia_core::models::RelationTypeView]| -> anyhow::Result<Vec<serde_json::Value>> {
        views
            .iter()
            .map(|v| {
                let mut json = serde_json::to_value(v)?;
                json["usage"] = serde_json::Value::Null;
                Ok(json)
            })
            .collect()
    };
    assert_eq!(without_usage(&counted)?, without_usage(&uncounted)?);
    assert_eq!(uncounted[1].domains, vec![class]);
    Ok(())
}
