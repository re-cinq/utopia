//! 补向量每篇文档都跑一次 `pending`（#1296）：它得以谓词为索引条件走 `facts_live_predicate_idx`，
//! 只读这个库的名字事实、按 id 顺序读，不用排序。改回 JOIN 时规划器会沿 `facts_kb_id_key` 读整个库。
//!
//! 规划器按统计选索引：库是空的时候几个索引代价一样，选哪个看运气（别的测试同时在写 facts，
//! 选择就会翻）。所以这里在回滚的事务里造一个两千条事实、十分之一是名字的库并 ANALYZE，
//! 比的是有数据时它走哪条路。
use super::{DROP_FOR_DOCUMENT_SQL, DROP_FOR_SOURCE_SQL, PENDING_SQL};
use sqlx::PgPool;
use uuid::Uuid;

#[tokio::test]
async fn pending_reads_the_name_facts_through_the_predicate_index() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE facts (LIKE public.facts INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE relation_types (LIKE public.relation_types INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE name_vectors (LIKE public.name_vectors INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE fact_evidence (LIKE public.fact_evidence INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE documents (LIKE public.documents INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE sources (LIKE public.sources INCLUDING ALL) ON COMMIT DROP;
         INSERT INTO relation_types (id, kb_id, key, label, builtin)
         VALUES (md5('known-as')::uuid, md5('kb')::uuid, 'known_as', 'known as', true),
                (md5('mentions')::uuid, md5('kb')::uuid, 'mentions', 'mentions', false);
         INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
         SELECT gen_random_uuid(), md5('kb')::uuid, md5('thing')::uuid,
                CASE WHEN n % 10 = 0 THEN md5('known-as')::uuid ELSE md5('mentions')::uuid END,
                jsonb_build_object('value', 'v' || n)
           FROM generate_series(1, 2000) n;
         ANALYZE pg_temp.facts;
         ANALYZE pg_temp.relation_types;
         ANALYZE pg_temp.name_vectors;
         ANALYZE pg_temp.fact_evidence;
         ANALYZE pg_temp.documents;
         ANALYZE pg_temp.sources;",
    )
    .execute(&mut *tx)
    .await?;
    let kb: Uuid = sqlx::query_scalar("SELECT md5('kb')::uuid")
        .fetch_one(&mut *tx)
        .await?;
    let plan: serde_json::Value =
        sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {PENDING_SQL}"))
            .bind(kb)
            .bind(crate::names::KNOWN_AS)
            .bind(64_i64)
            .fetch_one(&mut *tx)
            .await?;
    tx.rollback().await?;

    // 两百条名字、一千八百条别的：以谓词为索引条件只读名字事实，顺扫或沿库索引会读满两千。
    // 不查 Sort：空的证据一侧规划器爱排不排，与这里要守的无关
    let visited = rows_visited(&plan[0]["Plan"], "facts");
    assert!(
        visited > 0 && visited <= 200,
        "read {visited} facts: {plan}"
    );
    assert!(
        plan.to_string().contains("(kb_id = ") && plan.to_string().contains("predicate_id = $0"),
        "{plan}"
    );
    Ok(())
}

/// 0071 的清向量每篇身份文档都走一次，不能读完别的文档的证据才挑中这一篇（#1408）。
/// 临时表复制迁移后的列和索引，统计只来自本例，避免别的测试的写入改变查询计划。
/// 这里约束选择性高的小来源；一次覆盖很多文档时，顺扫可能更便宜，应由规划器选择。
#[tokio::test]
async fn dropping_vectors_reads_only_the_selected_documents_evidence() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE documents (LIKE public.documents INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE fact_evidence (LIKE public.fact_evidence INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE name_vectors (LIKE public.name_vectors INCLUDING ALL) ON COMMIT DROP;
         INSERT INTO documents (id, kb_id, source_id, filename, sha256, external_key)
         SELECT gen_random_uuid(), md5('kb')::uuid,
                md5(CASE WHEN n <= 2 THEN 'selected' ELSE 'other' END)::uuid,
                'document-' || n, md5(n::text), n::text
           FROM generate_series(1, 1000) n;
         INSERT INTO fact_evidence (fact_id, chunk_id, document_id)
         SELECT gen_random_uuid(), gen_random_uuid(), d.id
           FROM documents d CROSS JOIN generate_series(1, 20) n;
         INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
         SELECT fact_id, md5('kb')::uuid, md5('entity')::uuid, '[1,0,0]'::vector
           FROM fact_evidence;
         ANALYZE pg_temp.documents;
         ANALYZE pg_temp.fact_evidence;
         ANALYZE pg_temp.name_vectors;",
    )
    .execute(&mut *tx)
    .await?;
    let (document, source): (Uuid, Uuid) =
        sqlx::query_as("SELECT id, source_id FROM documents WHERE external_key = '1'")
            .fetch_one(&mut *tx)
            .await?;

    for (query, id, expected_rows) in [
        (DROP_FOR_DOCUMENT_SQL, document, 20),
        (DROP_FOR_SOURCE_SQL, source, 40),
    ] {
        // 真正执行删除，但每次都回到相同的向量集合。比访问行数，不把机器快慢变成断言。
        sqlx::query("SAVEPOINT plan_case").execute(&mut *tx).await?;
        let plan: serde_json::Value =
            sqlx::query_scalar(&format!("EXPLAIN (ANALYZE, FORMAT JSON) {query}"))
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query("ROLLBACK TO SAVEPOINT plan_case")
            .execute(&mut *tx)
            .await?;
        let visited = evidence_rows_visited(&plan[0]["Plan"]);
        assert!(visited > 0 && visited <= expected_rows, "{plan}");
    }
    tx.rollback().await?;
    Ok(())
}

/// 补向量每篇文档都跑一次 `pending`：它列的是一个库的名字，核对身份证据时只该读这个库自己的
/// 证据。别的库有一个很大的身份来源时，不能为了这个库的几十个候选把它的证据读一遍。
/// 临时表复制迁移后的列和索引，统计只来自本例；比访问行数，不比耗时。
#[tokio::test]
async fn pending_names_read_only_their_own_bases_identity_evidence() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(
        "CREATE TEMP TABLE facts (LIKE public.facts INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE relation_types (LIKE public.relation_types INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE name_vectors (LIKE public.name_vectors INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE fact_evidence (LIKE public.fact_evidence INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE documents (LIKE public.documents INCLUDING ALL) ON COMMIT DROP;
         CREATE TEMP TABLE sources (LIKE public.sources INCLUDING ALL) ON COMMIT DROP;
         INSERT INTO relation_types (id, kb_id, key, label, builtin)
         VALUES (md5('target-known-as')::uuid, md5('target')::uuid, 'known_as', 'known as', true),
                (md5('other-known-as')::uuid, md5('other')::uuid, 'known_as', 'known as', true);
         -- 目标库：三万条名字；前十条还没有向量，接着两万条有身份证据所以不留向量，其余有向量
         INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
         SELECT md5('target-fact-' || n)::uuid, md5('target')::uuid, md5('thing')::uuid,
                md5('target-known-as')::uuid, jsonb_build_object('value', 'name-' || n)
           FROM generate_series(1, 30000) n;
         INSERT INTO name_vectors (fact_id, kb_id, entity_id, embedding)
         SELECT md5('target-fact-' || n)::uuid, md5('target')::uuid, md5('thing')::uuid, '[1,0,0]'::vector
           FROM generate_series(20011, 30000) n;
         INSERT INTO sources (id, kb_id, kind, name, config)
         VALUES (md5('target-source')::uuid, md5('target')::uuid, 'statements', 'identity',
                 '{\"names\": \"identity\"}');
         INSERT INTO documents (id, kb_id, source_id, filename, sha256, external_key)
         VALUES (md5('target-document')::uuid, md5('target')::uuid, md5('target-source')::uuid,
                 'target-document', md5('target-document'), 'target');
         INSERT INTO fact_evidence (fact_id, chunk_id, document_id)
         SELECT md5('target-fact-' || n)::uuid, gen_random_uuid(), md5('target-document')::uuid
           FROM generate_series(11, 20010) n;
         -- 别的库：一个身份来源，五千篇文档，六万条证据落在它自己的名字上
         INSERT INTO sources (id, kb_id, kind, name, config)
         VALUES (md5('other-source')::uuid, md5('other')::uuid, 'statements', 'identity',
                 '{\"names\": \"identity\"}');
         INSERT INTO documents (id, kb_id, source_id, filename, sha256, external_key)
         SELECT md5('other-document-' || n)::uuid, md5('other')::uuid, md5('other-source')::uuid,
                'document-' || n, md5(n::text), n::text
           FROM generate_series(1, 30000) n;
         INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_value)
         SELECT md5('other-fact-' || n)::uuid, md5('other')::uuid, md5('thing')::uuid,
                md5('other-known-as')::uuid, jsonb_build_object('value', 'other-' || n)
           FROM generate_series(1, 60000) n;
         INSERT INTO fact_evidence (fact_id, chunk_id, document_id)
         SELECT md5('other-fact-' || (n % 60000 + 1))::uuid, gen_random_uuid(),
                md5('other-document-' || (n % 30000 + 1))::uuid
           FROM generate_series(1, 300000) n;
         ANALYZE pg_temp.facts;
         ANALYZE pg_temp.relation_types;
         ANALYZE pg_temp.fact_evidence;
         ANALYZE pg_temp.documents;
         ANALYZE pg_temp.sources;",
    )
    .execute(&mut *tx)
    .await?;

    let target: Uuid = sqlx::query_scalar("SELECT md5('target')::uuid")
        .fetch_one(&mut *tx)
        .await?;
    let plan: serde_json::Value = sqlx::query_scalar(&format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {PENDING_SQL}"
    ))
    .bind(target)
    .bind(crate::names::KNOWN_AS)
    // 上限比候选多：规划器不会指望读到前几行就停，而要算整个反连接的代价
    .bind(10_000_i64)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let visited = evidence_rows_visited(&plan[0]["Plan"]);
    // 目标库自己有两万条身份证据，读一遍它们是应当的；别的库的三十万条不该被读
    let own_evidence = 20_010;
    assert!(
        visited < 2 * own_evidence,
        "read {visited} evidence rows, the base has {own_evidence}: {plan}"
    );
    Ok(())
}

fn evidence_rows_visited(node: &serde_json::Value) -> u64 {
    rows_visited(node, "fact_evidence")
}

fn rows_visited(node: &serde_json::Value, relation: &str) -> u64 {
    let own_rows = if node["Relation Name"] == relation
        && node["Node Type"]
            .as_str()
            .is_some_and(|kind| kind.ends_with("Scan"))
    {
        (node["Actual Rows"].as_u64().unwrap_or(0)
            + node["Rows Removed by Filter"].as_u64().unwrap_or(0)
            + node["Rows Removed by Index Recheck"].as_u64().unwrap_or(0))
            * node["Actual Loops"].as_u64().unwrap_or(0)
    } else {
        0
    };
    own_rows
        + node["Plans"]
            .as_array()
            .map(|children| {
                children
                    .iter()
                    .map(|child| rows_visited(child, relation))
                    .sum::<u64>()
            })
            .unwrap_or(0)
}

/// 删文档时按块找出处（`lock_cited_timelines`）从 `chunks` 连到 `fact_evidence.chunk_id`；
/// 没有这一列的索引时，每次调用都顺扫整张证据表。关掉顺扫，看规划器有没有索引可走。
#[tokio::test]
async fn the_evidence_of_a_chunk_is_found_by_an_index() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await?;
    let plan: Vec<String> =
        sqlx::query_scalar("EXPLAIN SELECT fact_id FROM fact_evidence WHERE chunk_id = $1")
            .bind(Uuid::now_v7())
            .fetch_all(&mut *tx)
            .await?;
    tx.rollback().await?;

    let plan = plan.join("\n");
    assert!(plan.contains("fact_evidence_chunk_idx"), "{plan}");
    Ok(())
}
