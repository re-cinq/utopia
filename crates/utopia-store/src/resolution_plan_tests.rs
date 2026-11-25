//! Finding the identity holder by name (`resolve_identity_name`) runs once per entity: both the primary-name branch and the alias branch must
//! read through the name index, otherwise in a production store one name would read every untyped entity and every known_as fact in the store.
//!
//! The planner chooses its path from statistics, while other tests in the shared store write to the same tables and ANALYZE them, so the statistics shift. So here we
//! copy the migrated columns and indexes into a temporary table (so the statistics come only from this case), create twenty thousand untyped entities and twenty thousand name facts,
//! and compare how many rows are actually read during execution, not which index the planner picked; with sequential scan, hash and merge joins turned off,
//! the only path left is lookup by index, and even if the estimate is off, the whole store is not read.
use super::identity_holders_sql;
use uuid::Uuid;

/// Each name matches exactly one row; a sequential scan of twenty thousand entities or name facts would far exceed this number.
const MOST_NAME_ROWS_READ: u64 = 50;

/// A connection that sees only temp copies of `tables` (their columns and indexes, none of the
/// shared database's rows or statistics), inside a transaction the test rolls back.
async fn temp_copies(
    tables: &[&str],
) -> anyhow::Result<sqlx::Transaction<'static, sqlx::Postgres>> {
    let url = crate::test_db::url().expect("callers return early without a database");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let mut tx = pool.begin().await?;
    for table in tables {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL) ON COMMIT DROP"
        ))
        .execute(&mut *tx)
        .await?;
    }
    Ok(tx)
}

#[tokio::test]
async fn an_identity_name_is_found_by_its_name_and_its_aliases_indexes() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let (kb, known_as) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = temp_copies(&["relation_types", "entities", "facts"]).await?;
    for (sql, ids) in [
        (
            "INSERT INTO relation_types (id, kb_id, key, label, builtin)
             VALUES ($2, $1, 'known_as', 'known as', true)",
            vec![kb, known_as],
        ),
        (
            "INSERT INTO entities (id, kb_id, canonical_name)
             SELECT gen_random_uuid(), $1, 'entity ' || n FROM generate_series(1, 20000) n",
            vec![kb],
        ),
        (
            "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
             SELECT gen_random_uuid(), $1, e.id, $2, jsonb_build_object('value', 'alias ' || e.canonical_name)
               FROM entities e WHERE e.kb_id = $1",
            vec![kb, known_as],
        ),
    ] {
        let mut query = sqlx::query(sql);
        for id in ids {
            query = query.bind(id);
        }
        query.execute(&mut *tx).await?;
    }
    sqlx::raw_sql(
        "ANALYZE pg_temp.relation_types; ANALYZE pg_temp.entities; ANALYZE pg_temp.facts;
         SET LOCAL enable_seqscan = off; SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off",
    )
    .execute(&mut *tx)
    .await?;
    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, FORMAT JSON) {}",
        identity_holders_sql()
    ))
    .bind(kb)
    .bind(None::<Uuid>)
    .bind(vec!["entity 7".to_string(), "alias entity 8".to_string()])
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let visited = rows_visited(&plan[0]["Plan"]);
    assert!(
        visited < MOST_NAME_ROWS_READ,
        "{visited} rows visited: {plan}"
    );
    Ok(())
}

/// Rows the plan read from `entities` and `facts`, counting what filters and rechecks threw away.
fn rows_visited(node: &serde_json::Value) -> u64 {
    let reads_a_table = matches!(node["Relation Name"].as_str(), Some("entities" | "facts"))
        && node["Node Type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("Scan"));
    let own_rows = if reads_a_table {
        [
            "Actual Rows",
            "Rows Removed by Filter",
            "Rows Removed by Index Recheck",
        ]
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
            .map(|children| children.iter().map(rows_visited).sum::<u64>())
            .unwrap_or(0)
}

/// Shape of the production store: a store with millions of current facts and hundreds of relation kinds, known_as only one percent, and every alias different.
/// The planner does not use the statistics of the partial index `facts_value_text_idx`, so it estimates the rows for a value at 0.5% of the store's rows (here, five thousand rows),
/// while the relation index estimates by "store rows / relation kinds" as a few rows, so the planner would rather read the whole known_as and filter by value.
const RELATION_KINDS: i32 = 300;
const MOST_BUFFERS_TOUCHED: u64 = 200;

#[tokio::test]
async fn an_alias_is_found_by_its_value_not_by_walking_every_name() -> anyhow::Result<()> {
    if crate::test_db::url().is_none() {
        return Ok(());
    }
    let kb = Uuid::now_v7();
    let mut tx = temp_copies(&["relation_types", "facts"]).await?;
    for sql in [
        "INSERT INTO relation_types (id, kb_id, key, label, builtin)
         SELECT gen_random_uuid(), $1, CASE WHEN kind = 0 THEN 'known_as' ELSE 'relation ' || kind END,
                'relation', kind = 0
           FROM generate_series(0, $2) kind",
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
         SELECT gen_random_uuid(), $1, gen_random_uuid(), nr.id,
                jsonb_build_object('value', CASE WHEN f.kind = 0 THEN 'alias ' || f.n ELSE 'other ' || f.n END)
           FROM (SELECT n, CASE WHEN n % 100 = 0 THEN 0 ELSE 1 + n % $2 END AS kind
                   FROM generate_series(1, 1000000) n) f
           JOIN relation_types nr ON nr.key = CASE WHEN f.kind = 0 THEN 'known_as' ELSE 'relation ' || f.kind END",
    ] {
        sqlx::query(sql)
            .bind(kb)
            .bind(RELATION_KINDS)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::raw_sql("ANALYZE pg_temp.relation_types; ANALYZE pg_temp.facts")
        .execute(&mut *tx)
        .await?;
    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
        crate::names::named_ids(1, 2)
    ))
    .bind(kb)
    .bind(vec!["alias 500000".to_string()])
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let root = &plan[0]["Plan"];
    let touched = [
        "Shared Hit Blocks",
        "Local Hit Blocks",
        "Shared Read Blocks",
        "Local Read Blocks",
    ]
    .iter()
    .map(|field| root[*field].as_u64().unwrap_or(0))
    .sum::<u64>();
    assert!(
        touched < MOST_BUFFERS_TOUCHED,
        "{touched} buffers touched: {plan}"
    );
    Ok(())
}
