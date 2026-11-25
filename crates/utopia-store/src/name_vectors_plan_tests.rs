//! The vector backfill runs `pending` once per document (#1296): it reaches the index through the predicate as its index condition on `facts_live_predicate_idx`,
//! reads only this library's name facts, in id order, with no sort. If it goes back to a JOIN, the planner reads the whole library along `facts_kb_id_key`.
//!
//! The planner picks indexes by statistics: when the library is empty, several indexes cost the same and which one is chosen is luck (other tests write to facts at the same time,
//! so the choice would flip). So here, inside a rolled-back transaction, we build a library of 2000 facts, one tenth of them names, and ANALYZE it,
//! and check which path it takes with data present.
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

    // 200 names and 1800 other facts: using the predicate as the index condition reads only name facts; a sequential scan or the library index would read all 2000.
    // No check on Sort: on the empty evidence side the planner likes to sort or not sort, which is unrelated to what this guards.
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

/// 0071's clear-vectors step runs once for each identity document, and it must not read the evidence of other documents before it picks this one (#1408).
/// The temporary table copies the migrated columns and indexes; statistics come only from this case, so other tests' writes do not change the query plan.
/// This pins down a small, highly selective source; when one run covers many documents, a sequential scan may be cheaper, and the planner should choose.
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
        // It really runs the delete, but each time it goes back to the same vector set. It compares rows accessed, not machine speed, so the test is not an assertion about the hardware.
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

/// The vector backfill runs `pending` once per document: it lists one library's names, and when checking identity evidence it should read only that library's own
/// evidence. When another library has a very large identity source, it must not read through that library's evidence just to pick a few dozen candidates of this one.
/// The temporary table copies the migrated columns and indexes; statistics come only from this case. Compare rows accessed, not elapsed time.
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
    // The limit is larger than the candidate count: the planner does not count on reading the first few rows and stopping, but computes the cost of the whole anti-join.
    .bind(10_000_i64)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;

    let visited = evidence_rows_visited(&plan[0]["Plan"]);
    // The target library has 20,000 identity evidence rows of its own, and reading those is expected; another library's 300,000 should not be read.
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

/// When deleting a document, the source is found per block (`lock_cited_timelines`), linking from `chunks` to `fact_evidence.chunk_id`;
/// without an index on that column, every call does a sequential scan of the whole evidence table. Turn off sequential scans to see whether the planner has an index it can use.
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
