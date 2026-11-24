//! The recompute's whole-base statements are planned for the base they are given (#1298): a cached
//! statement switches to a generic plan after a few runs, and the generic plan of the source
//! cleanup ran for hours on a production base where the plan for the base's own id takes seconds.
//!
//! The due statements are planned against temp copies of the tables (statistics come only from
//! the seed), so the shared database's concurrent writers cannot flip the plan.
use super::{due_sql, materialize};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[tokio::test]
async fn a_recompute_leaves_none_of_its_whole_base_statements_cached_on_its_connection(
) -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let base = Uuid::now_v7();

    materialize(&pool, base).await?;
    materialize(&pool, base).await?;
    let cached: Vec<String> = sqlx::query_scalar(
        "SELECT statement FROM pg_prepared_statements
          WHERE statement LIKE '%kb_id = $1%' AND statement NOT LIKE '%pg_prepared_statements%'",
    )
    .fetch_all(&pool)
    .await?;

    assert_eq!(cached, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn the_source_cleanup_plans_the_sources_that_hold_once_as_a_cte_the_delete_scans(
) -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;

    let plan_lines: Vec<String> =
        sqlx::query_scalar(&format!("EXPLAIN {}", super::source_cleanup_sql()))
            .persistent(false)
            .bind(Uuid::now_v7())
            .fetch_all(&pool)
            .await?;
    let plan = plan_lines.join("\n");

    assert!(
        plan.contains("CTE holding") && plan.contains("CTE Scan on holding"),
        "the delete must anti-join a CTE built once, not rescan the binding match per source, got:\n{plan}"
    );
    Ok(())
}

/// More entities than the bound phrases' statements need, so a read of all of them per binding
/// is far above a read of the statements' own.
const ENTITIES: u32 = 300_000;
const BOUND_PHRASES: u32 = 30;

#[tokio::test]
async fn the_due_statements_are_read_once_from_the_bound_phrases() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let (base, property) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = pool.begin().await?;
    for table in [
        "relation_types",
        "entities",
        "facts",
        "phrase_bindings",
        "statement_qualifiers",
        "errata_actions",
        "typed_fact_sources",
    ] {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL) ON COMMIT DROP"
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::raw_sql(&format!(
        "INSERT INTO relation_types (id, kb_id, key, label) VALUES ('{property}', '{base}', 'p', 'p');
         INSERT INTO entities (id, kb_id, canonical_name)
              SELECT gen_random_uuid(), '{base}', 'entity ' || n FROM generate_series(1, {ENTITIES}) n;
         -- one statement per entity, spread over the bound phrases and twenty times as many others
         INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase)
              SELECT gen_random_uuid(), '{base}', e.id, '1'::jsonb, 'open',
                     CASE WHEN n % 3 = 0 THEN 'bound phrase ' || (n % {BOUND_PHRASES})
                          ELSE 'other phrase ' || (n % 600) END
                FROM (SELECT id, row_number() OVER () n FROM entities) e;
         INSERT INTO phrase_bindings (id, kb_id, phrase, object_is_value, relation_type_id, direction, status)
              SELECT gen_random_uuid(), '{base}', 'bound phrase ' || n, true, '{property}', 'forward', 'bound'
                FROM generate_series(0, {BOUND_PHRASES} - 1) n;
         INSERT INTO phrase_bindings (id, kb_id, phrase, object_is_value, status)
              SELECT gen_random_uuid(), '{base}', 'other phrase ' || n, true,
                     CASE WHEN n % 2 = 0 THEN 'none' ELSE 'undecided' END
                FROM generate_series(0, 599) n;
         ANALYZE pg_temp.relation_types; ANALYZE pg_temp.entities; ANALYZE pg_temp.facts;
         ANALYZE pg_temp.phrase_bindings; ANALYZE pg_temp.statement_qualifiers;
         ANALYZE pg_temp.errata_actions; ANALYZE pg_temp.typed_fact_sources"
    ))
    .execute(&mut *tx)
    .await?;

    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
        due_sql()
    ))
    .persistent(false)
    .bind(base)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let read = entity_rows_read(&plan[0]["Plan"]);
    assert!(
        read <= u64::from(ENTITIES),
        "{read} entity rows read for {ENTITIES} entities: {plan:#}"
    );
    Ok(())
}

/// Rows the plan read from `entities`, counting what filters threw away, over all loops.
fn entity_rows_read(node: &serde_json::Value) -> u64 {
    let own_rows = if node["Relation Name"].as_str() == Some("entities") {
        ["Actual Rows", "Rows Removed by Filter"]
            .iter()
            .map(|field| node[*field].as_u64().unwrap_or(0))
            .sum::<u64>()
            * node["Actual Loops"].as_u64().unwrap_or(0)
    } else {
        0
    };
    own_rows
        + node["Plans"]
            .as_array()
            .map_or(0, |children| children.iter().map(entity_rows_read).sum())
}
