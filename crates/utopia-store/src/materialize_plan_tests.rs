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

const OTHER_BASES_FACTS: u32 = 600_000;
const BASE_STATEMENTS: u32 = 300;
const ENTITY_SOURCED_FACTS: u32 = 30_000;

#[tokio::test]
async fn attestation_sync_reads_only_the_facts_of_its_base() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let (base, other_base, rule) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let mut tx = pool.begin().await?;
    for table in [
        "facts",
        "typed_fact_sources",
        "implied_fact_sources",
        "implication_rules",
    ] {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL) ON COMMIT DROP"
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::raw_sql(&format!(
        "INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, attested_from)
              SELECT gen_random_uuid(), '{other_base}', gen_random_uuid(), '1'::jsonb, 'open',
                     'phrase ' || (n % 50), now()
                FROM generate_series(1, {OTHER_BASES_FACTS}) n;
         INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, attested_from)
              SELECT gen_random_uuid(), '{base}', gen_random_uuid(), '1'::jsonb, 'open',
                     'statement ' || n, now() - make_interval(days => n)
                FROM generate_series(1, {BASE_STATEMENTS}) n;
         -- one typed fact per statement, attested at a time that is not its statement's
         INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, attested_from)
              SELECT gen_random_uuid(), '{base}', gen_random_uuid(), '1'::jsonb, 'typed',
                     'typed ' || n, now()
                FROM generate_series(1, {BASE_STATEMENTS}) n;
         INSERT INTO typed_fact_sources (fact_id, statement_id)
              SELECT t.id, s.id
                FROM (SELECT id, row_number() OVER () n FROM facts WHERE kb_id = '{base}' AND layer = 'typed') t
                JOIN (SELECT id, row_number() OVER () n FROM facts WHERE kb_id = '{base}' AND layer = 'open') s
                  USING (n);
         -- facts a kind-word rule implies: their sources are entities, no statement to attest
         INSERT INTO implication_rules (id, kb_id, trigger, phrase, conclude_property_id, status)
              VALUES ('{rule}', '{base}', 'kind_word', 'kind', gen_random_uuid(), 'approved');
         INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, implied)
              SELECT gen_random_uuid(), '{base}', gen_random_uuid(), '1'::jsonb, 'typed', 'kind ' || n, true
                FROM generate_series(1, {ENTITY_SOURCED_FACTS}) n;
         INSERT INTO implied_fact_sources (fact_id, rule_id, entity_id)
              SELECT id, '{rule}', gen_random_uuid() FROM facts WHERE kb_id = '{base}' AND implied;
         ANALYZE pg_temp.facts; ANALYZE pg_temp.typed_fact_sources;
         ANALYZE pg_temp.implied_fact_sources; ANALYZE pg_temp.implication_rules"
    ))
    .execute(&mut *tx)
    .await?;

    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
        super::typed_attestation_sql(super::SyncScope::Base)
    ))
    .persistent(false)
    .bind(base)
    .bind(None::<&[Uuid]>)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let read = rows_read_from(&plan[0]["Plan"], "facts");
    let base_facts = u64::from(BASE_STATEMENTS * 2 + ENTITY_SOURCED_FACTS);
    assert!(
        read < base_facts * 10,
        "{read} facts rows read for a base of {base_facts} facts among {OTHER_BASES_FACTS} others: {plan:#}"
    );
    Ok(())
}
const GIVEN_BASE_STATEMENTS: u32 = 120_000;
const GIVEN_ROWS: i64 = 7;

/// A recompute for a few rows (a deleted document's, or those following a dated document's statements)
/// reads those rows' sources, not the base: on the internal cluster a recompute for seven facts read the
/// base's 7.3 M facts and all 4.6 M sources and took three minutes (RC-012).
#[tokio::test]
async fn attestation_sync_of_given_rows_reads_only_their_sources() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    let (base, rule) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = pool.begin().await?;
    for table in [
        "facts",
        "typed_fact_sources",
        "implied_fact_sources",
        "implication_rules",
    ] {
        sqlx::raw_sql(&format!(
            "CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL) ON COMMIT DROP"
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::raw_sql(&format!(
        "INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, attested_from)
              SELECT gen_random_uuid(), '{base}', gen_random_uuid(), '1'::jsonb, 'open',
                     'statement ' || n, now() - make_interval(days => n % 900)
                FROM generate_series(1, {GIVEN_BASE_STATEMENTS}) n;
         INSERT INTO facts (id, kb_id, subject_id, object_value, layer, phrase, attested_from)
              SELECT gen_random_uuid(), '{base}', gen_random_uuid(), '1'::jsonb, 'typed',
                     'typed ' || n, now()
                FROM generate_series(1, {GIVEN_BASE_STATEMENTS}) n;
         INSERT INTO typed_fact_sources (fact_id, statement_id)
              SELECT t.id, s.id
                FROM (SELECT id, row_number() OVER () n FROM facts WHERE layer = 'typed') t
                JOIN (SELECT id, row_number() OVER () n FROM facts WHERE layer = 'open') s
                  USING (n);
         INSERT INTO implication_rules (id, kb_id, trigger, phrase, conclude_property_id, status)
              VALUES ('{rule}', '{base}', 'phrase', 'statement', gen_random_uuid(), 'approved');
         -- a tenth of the typed rows are also read by a rule from their statement
         INSERT INTO implied_fact_sources (fact_id, rule_id, statement_id)
              SELECT ts.fact_id, '{rule}', ts.statement_id
                FROM (SELECT fact_id, statement_id, row_number() OVER () n FROM typed_fact_sources) ts
               WHERE ts.n % 10 = 0;
         ANALYZE pg_temp.facts; ANALYZE pg_temp.typed_fact_sources;
         ANALYZE pg_temp.implied_fact_sources; ANALYZE pg_temp.implication_rules"
    ))
    .execute(&mut *tx)
    .await?;
    let given: Vec<Uuid> = sqlx::query_scalar(
        "SELECT fact_id FROM (SELECT fact_id, row_number() OVER () n FROM typed_fact_sources) ts
          WHERE ts.n % 10 = 0 LIMIT $1",
    )
    .bind(GIVEN_ROWS)
    .fetch_all(&mut *tx)
    .await?;

    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
        super::typed_attestation_sql(super::SyncScope::Rows)
    ))
    .persistent(false)
    .bind(base)
    .bind(&given)
    .fetch_one(&mut *tx)
    .await?;
    // the same rows reached through their statements (a dated document's pass)
    let statements: Vec<Uuid> =
        sqlx::query_scalar("SELECT statement_id FROM typed_fact_sources WHERE fact_id = ANY($1)")
            .bind(&given)
            .fetch_all(&mut *tx)
            .await?;
    sqlx::query("UPDATE facts SET attested_from = now() WHERE id = ANY($1)")
        .bind(&given)
        .execute(&mut *tx)
        .await?;
    let following: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
        super::typed_attestation_sql(super::SyncScope::FollowingStatements)
    ))
    .persistent(false)
    .bind(base)
    .bind(&statements)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;
    let following_top = &following[0]["Plan"];
    let (following_facts, following_sources) = (
        rows_read_from(following_top, "facts"),
        rows_read_from(following_top, "typed_fact_sources"),
    );
    assert_eq!(
        following_top["Actual Rows"].as_u64(),
        Some(7),
        "all rows following the 7 statements move: {following:#}"
    );
    assert!(
        following_facts < 200 && following_sources < 200,
        "{following_facts} facts rows and {following_sources} source rows read for the rows following 7 statements: {following:#}"
    );

    let top = &plan[0]["Plan"];
    let (facts, sources) = (
        rows_read_from(top, "facts"),
        rows_read_from(top, "typed_fact_sources"),
    );
    assert_eq!(given.len(), 7);
    assert_eq!(
        top["Actual Rows"].as_u64(),
        Some(7),
        "all given rows move: {plan:#}"
    );
    assert!(
        facts < 200 && sources < 200,
        "{facts} facts rows and {sources} source rows read for 7 given rows in a base of {GIVEN_BASE_STATEMENTS} statements: {plan:#}"
    );
    Ok(())
}

/// Rows the plan read from `relation`, counting what filters threw away, over all loops.
fn rows_read_from(node: &serde_json::Value, relation: &str) -> u64 {
    let own_rows = if node["Relation Name"].as_str() == Some(relation) {
        let loops = node["Actual Loops"].as_u64().unwrap_or(1);
        ["Actual Rows", "Rows Removed by Filter"]
            .iter()
            .map(|field| node[*field].as_u64().unwrap_or(0))
            .sum::<u64>()
            * loops
    } else {
        0
    };
    own_rows
        + node["Plans"].as_array().map_or(0, |children| {
            children
                .iter()
                .map(|child| rows_read_from(child, relation))
                .sum()
        })
}
