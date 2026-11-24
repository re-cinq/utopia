//! 按名字找身份持有者（`resolve_identity_name`）每个实体都跑一次：本名分支和别名分支都得
//! 按名字走索引读，否则生产库里一个名字要读完这个库所有无类实体和所有 known_as 事实。
//!
//! 规划器按统计选路，而共享库里别的测试在同时写、ANALYZE 同样的表，统计会翻。所以这里用
//! 临时表复制迁移后的列和索引（统计只来自本例），造两万个无类实体和两万条名字事实，
//! 比的是真正执行时读了多少行，不是规划器挑了哪个索引；关掉顺扫和哈希、归并连接，
//! 剩下的路只有按索引找，估算偏了也不会把整个库读一遍。
use super::identity_holders_sql;
use uuid::Uuid;

/// 两个名字各只对上一行；顺扫两万个实体或名字事实会远超这个数。
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

/// 生产库的形状：一个库里上百万条现行事实、几百种关系，known_as 只占百分之一，别名各不相同。
/// 部分索引 `facts_value_text_idx` 的统计规划器不用，值的行数估成库行数的 0.5%（这里五千行），
/// 而关系索引按“库行数 / 关系种类数”估成几行，于是规划器宁可把整个 known_as 读一遍再按值过滤。
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
