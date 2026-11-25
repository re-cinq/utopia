//! Paging through to the end, each row appears exactly once (#646), run against a real DB.
//!
//! The Review tiers and the ledger all page with `LIMIT/OFFSET`. If the sort key is not unique, tied rows' order can differ from query to query —
//! by the time one reaches the second page, a row seen on page one comes back once more, and another row never appears on any page. When a timestamp
//! is the sort key, rows written one at a time rarely tie; **rows written within one transaction all tie** (`now()` is the same value within a transaction),
//! and Review's queue is exactly what gets written in batches. Measured: one check inserted 572 cycles, paged 200 rows at a time
//! yielded 557.
//!
//! Three batch-written sets of data, each paged through:
//!
//! 1. **Axiom violations**: one `reasoning::run` inserts hundreds of cycles, and `open_violations` pages through
//! 2. **Ontology defects**: one statement inserts hundreds of rows, and `open_defects` pages through
//! 3. **Review ledger**: one statement writes hundreds of audit events, and `review_history` pages through
//!
//! What it asserts is not merely "each row once", but that **the sequence paged out is exactly all the rows, ordered by timestamp descending and then id descending**.
//! Asserting only once-per-row is luck: the order of tied rows depends on the execution plan, and the old sort went wrong one time in three on this data.
//! Row ids are random and unrelated to physical write order, so whenever the sort is not pinned down at ties
//! the sequence will not line up — it fails every time.
//!
//! The page size is prime (37), so page boundaries land nowhere tidy. Skipped rather than
//! failed when `UTOPIA_DATABASE_URL` is not set. It builds and tears down its own data and never touches an existing DB.

use sqlx::PgPool;
use std::future::Future;
use utopia_store::reasoning;
use uuid::Uuid;

const PAGE: i64 = 37;

struct Fixture {
    org: Uuid,
    kb: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, kb) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'paging-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'paging-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'paging-test')",
    )
    .bind(kb)
    .bind(ws)
    .execute(pool)
    .await?;
    Ok(Fixture { org, kb })
}

/// Page through to the end, joining the pages in the order they were fetched
async fn page_through<F, Fut>(mut page: F) -> anyhow::Result<Vec<Uuid>>
where
    F: FnMut(i64) -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<Uuid>>>,
{
    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let ids = page(offset).await?;
        let n = ids.len() as i64;
        seen.extend(ids);
        if n < PAGE {
            return Ok(seen);
        }
        offset += PAGE;
    }
}

/// What comes back is all the rows, one each, and with timestamps tied the order is by id descending — the sort is a total order
async fn assert_total_order(
    pool: &PgPool,
    what: &str,
    seen: &[Uuid],
    all_ids: &str,
    expected: usize,
) -> anyhow::Result<()> {
    let mut want: Vec<Uuid> = sqlx::query_scalar(all_ids).fetch_all(pool).await?;
    assert_eq!(want.len(), expected, "{what}: 写下的行数");
    want.sort_by(|a, b| b.cmp(a));
    let mut distinct = seen.to_vec();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        seen.len(),
        "{what}: {} 行翻到了不止一次",
        seen.len() - distinct.len()
    );
    assert_eq!(seen.len(), expected, "{what}: 翻完只见到 {} 行", seen.len());
    assert!(
        seen == want,
        "{what}: 并列的行没有按 id 定下先后，翻页之间会漂"
    );
    Ok(())
}

#[tokio::test]
async fn paging_through_a_batch_sees_every_row_exactly_once() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let result = async {
        // ---- 1. Axiom violations: one central node mutually referencing 300 nodes, which on a constant predicate makes 300 cycles,
        // inserted in one transaction by one check
        const CYCLES: usize = 300;
        let (etype, part_of) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 't', 'T')")
            .bind(etype)
            .bind(f.kb)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, temporal, is_transitive)
             VALUES ($1, $2, 'part_of', 'part_of', 'eternal', TRUE)",
        )
        .bind(part_of)
        .bind(f.kb)
        .execute(&pool)
        .await?;
        sqlx::query(
            "WITH n AS (
                 INSERT INTO entities (id, kb_id, type_id, canonical_name)
                 SELECT gen_random_uuid(), $1, $2, 'N' || i FROM generate_series(0, $4) AS i
                 RETURNING id, canonical_name
             ), hub AS (SELECT id FROM n WHERE canonical_name = 'N0'),
                spokes AS (SELECT id FROM n WHERE canonical_name <> 'N0')
             INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id)
             SELECT gen_random_uuid(), $1, hub.id, $3, spokes.id FROM hub, spokes
             UNION ALL
             SELECT gen_random_uuid(), $1, spokes.id, $3, hub.id FROM hub, spokes",
        )
        .bind(f.kb)
        .bind(etype)
        .bind(part_of)
        .bind(CYCLES as i32)
        .execute(&pool)
        .await?;
        let r = reasoning::run(&pool, f.kb).await?;
        assert_eq!(r.inserted, CYCLES, "一次检查插下这些环");
        let seen = page_through(|offset| {
            let pool = pool.clone();
            async move {
                Ok(reasoning::open_violations(&pool, f.kb, PAGE, offset)
                    .await?
                    .into_iter()
                    .map(|v| v.id)
                    .collect())
            }
        })
        .await?;
        let ids = format!("SELECT id FROM axiom_violations WHERE kb_id = '{}'", f.kb);
        assert_total_order(&pool, "公理违规", &seen, &ids, CYCLES).await?;

        // ---- 2. Ontology defects: one statement inserts 250 rows, all with the same detected_at
        const DEFECTS: usize = 250;
        sqlx::query(
            "INSERT INTO ontology_defects (id, kb_id, kind, subject, other)
             SELECT gen_random_uuid(), $1, 'rules_disagree', gen_random_uuid(), gen_random_uuid()
               FROM generate_series(1, $2)",
        )
        .bind(f.kb)
        .bind(DEFECTS as i32)
        .execute(&pool)
        .await?;
        let seen = page_through(|offset| {
            let pool = pool.clone();
            async move {
                Ok(reasoning::open_defects(&pool, f.kb, PAGE, offset)
                    .await?
                    .into_iter()
                    .map(|d| d.id)
                    .collect())
            }
        })
        .await?;
        let ids = format!("SELECT id FROM ontology_defects WHERE kb_id = '{}'", f.kb);
        assert_total_order(&pool, "本体缺陷", &seen, &ids, DEFECTS).await?;

        // ---- 3. Review ledger: one statement writes 250 entries, all with the same created_at
        const EVENTS: usize = 250;
        sqlx::query(
            "INSERT INTO audit_events (id, kb_id, action, target_kind)
             SELECT gen_random_uuid(), $1, 'review.decided', 'violation'
               FROM generate_series(1, $2)",
        )
        .bind(f.kb)
        .bind(EVENTS as i32)
        .execute(&pool)
        .await?;
        let seen = page_through(|offset| {
            let pool = pool.clone();
            async move {
                Ok(
                    utopia_store::audit::review_history(&pool, f.kb, PAGE, offset)
                        .await?
                        .0
                        .into_iter()
                        .map(|e| e.id)
                        .collect(),
                )
            }
        })
        .await?;
        let ids = format!("SELECT id FROM audit_events WHERE kb_id = '{}'", f.kb);
        assert_total_order(&pool, "审核台账", &seen, &ids, EVENTS).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // Audit events are append-only (cannot be deleted) and stay in the test DB; they hang under this self-built DB name and do not leak elsewhere
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    result
}
