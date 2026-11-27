//! The present-path overview node query must not look facts up once per entity: on a production base of 296 thousand entities and 3.5 million live facts
//! the per-entity count took over six minutes, and 27 overviews at once used up the connection pool.
//!
//! The planner chooses its path from statistics, and other tests write to the shared tables, so the columns and indexes of the migrated tables are copied into
//! temporary tables whose statistics come only from this case: one base of a few hundred entities and a few thousand facts, among twenty others.
use super::overview_nodes_sql;
use uuid::Uuid;

const SEARCHED_ENTITIES: i32 = 300;
const FACTS_PER_BASE: i32 = 3000;
const OTHER_BASES: i32 = 20;

#[tokio::test]
async fn the_present_overview_does_not_look_facts_up_per_entity() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let mut tx = pool.begin().await?;
    for table in [
        "relation_types",
        "entity_types",
        "entities",
        "entity_merges",
        "facts",
    ] {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL) ON COMMIT DROP"
        ))
        .execute(&mut *tx)
        .await?;
    }
    let searched_base = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label)
         SELECT gen_random_uuid(), CASE WHEN n = 0 THEN $1 ELSE gen_random_uuid() END, 'works_with', 'works with'
           FROM generate_series(0, $2) n",
    )
    .bind(searched_base)
    .bind(OTHER_BASES)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO entities (id, kb_id, canonical_name)
         SELECT gen_random_uuid(), $1, 'entity ' || n FROM generate_series(1, $2) n",
    )
    .bind(searched_base)
    .bind(SEARCHED_ENTITIES)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH numbered AS (SELECT id, row_number() OVER () AS position FROM entities)
         INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
         SELECT gen_random_uuid(), searched.kb_id, subject.id, searched.id, object.id
           FROM relation_types searched, generate_series(1, $2) n
           JOIN numbered subject ON subject.position = n % 300 + 1
           JOIN numbered object ON object.position = (n * 7 + 3) % 300 + 1
          WHERE searched.kb_id = $1",
    )
    .bind(searched_base)
    .bind(FACTS_PER_BASE)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
         SELECT gen_random_uuid(), other.kb_id, gen_random_uuid(), other.id, gen_random_uuid()
           FROM relation_types other, generate_series(1, $2) n WHERE other.kb_id <> $1",
    )
    .bind(searched_base)
    .bind(FACTS_PER_BASE)
    .execute(&mut *tx)
    .await?;
    sqlx::raw_sql(
        "ANALYZE pg_temp.relation_types; ANALYZE pg_temp.entity_types; ANALYZE pg_temp.entities;
         ANALYZE pg_temp.entity_merges; ANALYZE pg_temp.facts",
    )
    .execute(&mut *tx)
    .await?;
    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (FORMAT JSON) {}",
        overview_nodes_sql(false)
    ))
    .bind(searched_base)
    .bind(150_i64)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    assert!(!looks_facts_up_per_row(&plan[0]["Plan"], false), "{plan}");
    Ok(())
}

fn looks_facts_up_per_row(node: &serde_json::Value, inside_sub_plan: bool) -> bool {
    let inside_sub_plan = inside_sub_plan || node["Parent Relationship"] == "SubPlan";
    let is_fact_scan = inside_sub_plan && node["Relation Name"] == "facts";
    is_fact_scan
        || node["Plans"].as_array().is_some_and(|children| {
            children
                .iter()
                .any(|child| looks_facts_up_per_row(child, inside_sub_plan))
        })
}
