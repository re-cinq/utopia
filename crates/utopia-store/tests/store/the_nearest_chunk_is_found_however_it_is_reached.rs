//! The vector recall answer does not change with the plan (0035 / #512), run against a real database.
//!
//! The index is invisible to its users: the contract is recall and the record axis. So every question here is asked twice —
//! once without an index, and again after `vector_index::build` — the two must agree. Whether the index is used
//! is decided by the planner; this test does not pretend it can choose for it. It pins the **answer**: change the answer and it goes red; change only the plan and it stays silent.
//!
//! Six questions each break in their own way:
//! - The nearest one must come back (if the expression index and `ORDER BY` are written differently, the index quietly stops applying)
//! - In a small KB next to a big one, `LIMIT 10` must come back full (HNSW post-filtering turns a small KB into zero rows; `relaxed_order` is what rescues it)
//! - No cross-KB leakage (tenancy, not performance)
//! - A chunk of another dimension is neither a candidate nor an error (in a KB that switched embedding models, both dimensions coexist)
//! - A superseded chunk is not a hit
//! - Retrieval with a moment sees what was alive at that time (0019, restated against the index)

use pgvector::Vector;
use sqlx::PgPool;
use utopia_store::vector_index::{self, Target};
use uuid::Uuid;

/// The dimension this test owns alone: other database tests use 3 dimensions; indexes are split by dimension, so they do not interfere
const DIMS: usize = 7;
const CROWD: usize = 2000;

fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

/// Reproducible pseudo-random vectors: the same seed gives the same batch of vectors, so a failure can be replayed
fn lcg(seed: &mut u64) -> f32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
}

struct Fixture {
    org: Uuid,
    big: Uuid,
    small: Uuid,
    /// In the big KB, the one that coincides with the query vector
    planted: Uuid,
    /// Coincides with the query but was superseded in May
    superseded: Uuid,
    /// The only one in the small KB
    only: Uuid,
    /// One 5-dimensional one in the big KB (from a model that was swapped)
    other_dims: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, big, small) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (doc_big, doc_small) = (Uuid::now_v7(), Uuid::now_v7());
    let (planted, superseded, only, other_dims) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let tag = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'plan-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'plan-test')")
        .bind(ws)
        .bind(org)
        .execute(pool)
        .await?;
    for (kb, name, doc) in [(big, "big", doc_big), (small, "small", doc_small)] {
        sqlx::query("INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, $3)")
            .bind(kb)
            .bind(ws)
            .bind(name)
            .execute(pool)
            .await?;
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256, status, created_at)
             VALUES ($1, $2, $3, $4, 'ready', $5)",
        )
        .bind(doc)
        .bind(kb)
        .bind(format!("{name}.md"))
        .bind(format!("sha-{tag}-{name}"))
        .bind(t("2026-02-01T00:00:00Z"))
        .execute(pool)
        .await?;
    }

    let mut tx = pool.begin().await?;
    let mut seed = 0x5eed_u64;
    for i in 0..CROWD {
        let v: Vec<f32> = (0..DIMS).map(|_| lcg(&mut seed)).collect();
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text, embedding, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(big)
        .bind(doc_big)
        .bind(i as i32)
        .bind(format!("crowd {i}"))
        .bind(Vector::from(v))
        .bind(t("2026-02-01T00:00:00Z"))
        .execute(&mut *tx)
        .await?;
    }
    // One that coincides with the query: one alive, one superseded in May
    for (id, seq, text, superseded_at) in [
        (planted, CROWD as i32, "the planted one", None),
        (
            superseded,
            CROWD as i32 + 1,
            "the one a reparse displaced",
            Some("2026-05-01T00:00:00Z"),
        ),
    ] {
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text, embedding, created_at,
                                 superseded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(big)
        .bind(doc_big)
        .bind(seq)
        .bind(text)
        .bind(Vector::from(query()))
        .bind(t("2026-02-01T00:00:00Z"))
        .bind(superseded_at.map(t))
        .execute(&mut *tx)
        .await?;
    }
    // A 5-dimensional one left over from a model swap
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text, embedding, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(other_dims)
    .bind(big)
    .bind(doc_big)
    .bind(CROWD as i32 + 2)
    .bind("five dims")
    .bind(Vector::from(vec![1.0, 0.0, 0.0, 0.0, 0.0]))
    .bind(t("2026-02-01T00:00:00Z"))
    .execute(&mut *tx)
    .await?;
    // Small KB: one, neither near nor far from the query
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text, embedding, created_at)
         VALUES ($1, $2, $3, 0, 'the only one', $4, $5)",
    )
    .bind(only)
    .bind(small)
    .bind(doc_small)
    .bind(Vector::from(vec![0.3, -0.2, 0.1, 0.4, -0.1, 0.2, 0.0]))
    .bind(t("2026-02-01T00:00:00Z"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Fixture {
        org,
        big,
        small,
        planted,
        superseded,
        only,
        other_dims,
    })
}

fn query() -> Vec<f32> {
    vec![0.11, -0.42, 0.37, 0.05, -0.28, 0.19, 0.33]
}

/// The answers to six questions, asked in one go; one set is taken under each of the two plans to compare
#[derive(Debug, PartialEq)]
struct Answers {
    nearest_in_big: Option<Uuid>,
    small_limit_10: Vec<Uuid>,
    other_dims_query: Vec<Uuid>,
    big_top_10_has_superseded: bool,
    big_top_10_has_other_dims: bool,
    as_of_march_first: Option<Uuid>,
}

async fn ask(pool: &PgPool, f: &Fixture) -> anyhow::Result<Answers> {
    use utopia_store::documents::vector_search;
    let big_now = vector_search(pool, f.big, &query(), 10, None, None).await?;
    let small_now = vector_search(pool, f.small, &query(), 10, None, None).await?;
    let five = vector_search(pool, f.big, &[1.0, 0.0, 0.0, 0.0, 0.0], 10, None, None).await?;
    let big_then = vector_search(
        pool,
        f.big,
        &query(),
        10,
        Some(t("2026-03-01T00:00:00Z")),
        None,
    )
    .await?;
    Ok(Answers {
        nearest_in_big: big_now.first().copied(),
        small_limit_10: small_now,
        other_dims_query: five,
        big_top_10_has_superseded: big_now.contains(&f.superseded),
        big_top_10_has_other_dims: big_now.contains(&f.other_dims),
        as_of_march_first: big_then.first().copied(),
    })
}

#[tokio::test]
async fn the_answers_agree_with_and_without_the_index() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let f = seed(&pool).await?;

    let run = async {
        // ---- 1. No index: the answer the exact path gives
        vector_index::drop(&pool, Target::Chunks, DIMS).await?;
        assert_eq!(
            vector_index::status(&pool, Target::Chunks, DIMS).await?,
            None
        );
        let exact = ask(&pool, &f).await?;
        assert_eq!(exact.nearest_in_big, Some(f.planted), "重合的那一条最近");
        assert_eq!(
            exact.small_limit_10,
            vec![f.only],
            "小库只有一条，LIMIT 10 就回这一条——不多不少，也不串大库"
        );
        assert_eq!(
            exact.other_dims_query,
            vec![f.other_dims],
            "5 维的查询只找 5 维的"
        );
        assert!(!exact.big_top_10_has_superseded, "顶掉的不是命中");
        assert!(!exact.big_top_10_has_other_dims, "7 维的查询看不见 5 维的");
        // March 1: the chunk that was superseded is still alive, and it coincides with the query — two identical vectors, distances tie, settled by id.
        // If undetermined, the exact path and HNSW each sort their own way, and the "index changed the answer" check below would flip between red and green (#652)
        assert_eq!(
            exact.as_of_march_first,
            Some(f.planted.min(f.superseded)),
            "距离并列由 id 定"
        );
        let then = utopia_store::documents::vector_search(
            &pool,
            f.big,
            &query(),
            2,
            Some(t("2026-03-01T00:00:00Z")),
            None,
        )
        .await?;
        assert!(
            then.contains(&f.superseded),
            "三月一日它还活着，带时刻的检索该看见它"
        );

        // ---- 2. Build the index: the task body, outside a transaction, idempotent twice
        let built = vector_index::build(&pool, Target::Chunks, DIMS).await?;
        assert!(built.created, "第一次建");
        assert_eq!(built.name, "chunks_embedding_hnsw_7");
        assert_eq!(
            vector_index::status(&pool, Target::Chunks, DIMS).await?,
            Some(true)
        );
        let again = vector_index::build(&pool, Target::Chunks, DIMS).await?;
        assert!(!again.created, "第二次是空操作");
        let def: (String,) = sqlx::query_as(
            "SELECT pg_get_indexdef(indexrelid) FROM pg_index
              WHERE indexrelid = 'chunks_embedding_hnsw_7'::regclass",
        )
        .fetch_one(&pool)
        .await?;
        assert!(def.0.contains("USING hnsw"), "{}", def.0);
        assert!(def.0.contains("vector_cosine_ops"), "{}", def.0);
        assert!(
            def.0.contains("WHERE (vector_dims(embedding) = 7)"),
            "{}",
            def.0
        );

        // ---- 3. With the index: the same six questions, the answers must be identical
        let indexed = ask(&pool, &f).await?;
        assert_eq!(indexed, exact, "索引改了答案");
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = vector_index::drop(&pool, Target::Chunks, DIMS).await;
    sqlx::query("DELETE FROM knowledge_bases WHERE id = ANY($1)")
        .bind(vec![f.big, f.small])
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await?;
    run
}
