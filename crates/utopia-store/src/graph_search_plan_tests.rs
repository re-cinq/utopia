//! A substring search by name (`search_entities` with `exact = false`) must not read the whole `facts` table: in a production store the alias half of the
//! filter walked every name fact of every knowledge base for each search.
//!
//! The planner chooses its path from statistics, and other tests write to the shared tables, so the columns and indexes of the migrated tables are copied into
//! temporary tables whose statistics come only from this case: one small knowledge base searched, among two hundred others holding two hundred thousand name facts.
use super::search_count_sql;
use uuid::Uuid;

const OTHER_BASES: i32 = 200;
const NAMES_PER_BASE: i32 = 1000;

#[tokio::test]
async fn a_substring_search_does_not_scan_every_fact() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let mut tx = pool.begin().await?;
    for table in ["relation_types", "entities", "facts"] {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL) ON COMMIT DROP"
        ))
        .execute(&mut *tx)
        .await?;
    }
    let searched_base = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, builtin)
         SELECT gen_random_uuid(), CASE WHEN n = 0 THEN $1 ELSE gen_random_uuid() END, 'known_as', 'known as', true
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
    .bind(NAMES_PER_BASE)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
         SELECT gen_random_uuid(), nr.kb_id, gen_random_uuid(), nr.id, jsonb_build_object('value', 'alias ' || n)
           FROM relation_types nr, generate_series(1, $1) n",
    )
    .bind(NAMES_PER_BASE)
    .execute(&mut *tx)
    .await?;
    sqlx::raw_sql(
        "ANALYZE pg_temp.relation_types; ANALYZE pg_temp.entities; ANALYZE pg_temp.facts",
    )
    .execute(&mut *tx)
    .await?;
    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (FORMAT JSON) {}",
        search_count_sql(false, false)
    ))
    .bind(searched_base)
    .bind("%alias 55%")
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    assert!(!scans_every_fact(&plan[0]["Plan"]), "{plan}");
    Ok(())
}

fn scans_every_fact(node: &serde_json::Value) -> bool {
    let is_fact_scan = node["Node Type"] == "Seq Scan" && node["Relation Name"] == "facts";
    is_fact_scan
        || node["Plans"]
            .as_array()
            .is_some_and(|children| children.iter().any(scans_every_fact))
}
