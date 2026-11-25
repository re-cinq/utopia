//! Embeddings wider than the HNSW limit of `vector` (text-embedding-3-large is 3072 dimensions) are still indexed:
//! the index stores `halfvec` and is only responsible for picking candidates; distance and order are still computed at full `vector` precision.
//!
//! The distance of two chunks differs only where half precision cannot see: at half precision they tie (ties are broken by id, and the smaller id is
//! farther away), while at full precision the nearer one must rank first. If the outer query ranked by the half-precision distance, it would pull the farther one up.

use pgvector::Vector;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use utopia_store::vector_index::{self, Target, JOB_KIND, VECTOR_MAX_DIMS};
use uuid::Uuid;

/// The dimension this test owns exclusively: just too wide to fit the `vector` index
const DIMS: usize = VECTOR_MAX_DIMS + 1;
const CROWD: usize = 300;

fn lcg(seed: &mut u64) -> f32 {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
}

/// A vector with the first two dimensions given and the rest zero
fn head(a: f32, b: f32) -> Vec<f32> {
    let mut v = vec![0.0; DIMS];
    v[0] = a;
    v[1] = b;
    v
}

struct Fixture {
    org: Uuid,
    big: Uuid,
    small: Uuid,
    /// The one nearer the query at full precision
    nearer: Uuid,
    /// As far as `nearer` at half precision, but with the smaller id
    farther: Uuid,
    only: Uuid,
}

async fn seed(pool: &PgPool) -> anyhow::Result<Fixture> {
    let (org, ws, big, small) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (doc_big, doc_small) = (Uuid::now_v7(), Uuid::now_v7());
    // `farther` takes the smaller id first: on a tie, it sorts first
    let (farther, nearer, only) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let tag = Uuid::now_v7();

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'wide-test')")
        .bind(org)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'wide-test')")
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
            "INSERT INTO documents (id, kb_id, filename, sha256, status)
             VALUES ($1, $2, $3, $4, 'ready')",
        )
        .bind(doc)
        .bind(kb)
        .bind(format!("{name}.md"))
        .bind(format!("sha-{tag}-{name}"))
        .execute(pool)
        .await?;
    }

    let mut tx = pool.begin().await?;
    let mut seed = 0x31de_u64;
    let mut rows: Vec<(Uuid, Uuid, Uuid, Vec<f32>)> = (0..CROWD)
        .map(|_| {
            let v = (0..DIMS).map(|_| lcg(&mut seed)).collect();
            (Uuid::now_v7(), big, doc_big, v)
        })
        .collect();
    // 1.0004 and 1.0 are the same number at half precision (the spacing near 1 is 2^-10, and 1.0004 is less than half of that, so it rounds to 1), but at full precision they are not
    rows.push((farther, big, doc_big, head(1.0, 1.0004)));
    rows.push((nearer, big, doc_big, head(1.0, 1.0)));
    rows.push((only, small, doc_small, head(0.2, 1.0)));
    for (seq, (id, kb, doc, v)) in rows.into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO chunks (id, kb_id, document_id, seq, text, embedding)
             VALUES ($1, $2, $3, $4, 'wide', $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(doc)
        .bind(seq as i32)
        .bind(Vector::from(v))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Fixture {
        org,
        big,
        small,
        nearer,
        farther,
        only,
    })
}

async fn index_scans(pool: &PgPool) -> anyhow::Result<i64> {
    pool.execute("SELECT pg_stat_force_next_flush()").await?;
    pool.execute("SELECT pg_stat_clear_snapshot()").await?;
    let name = vector_index::index_name(Target::Chunks, DIMS);
    Ok(sqlx::query_scalar(
        "SELECT coalesce(sum(idx_scan), 0)::int8 FROM pg_stat_user_indexes WHERE indexrelname = $1",
    )
    .bind(name)
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn a_wide_embedding_is_indexed_and_ranked_at_full_precision() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // One connection: no sequential scan and no sorting allowed; the planner is left only with the index path, and statistics are all recorded on this connection
    let indexed = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET enable_seqscan = off; SET enable_sort = off")
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await?;
    let f = seed(&pool).await?;
    let forget_jobs = || {
        sqlx::query("DELETE FROM jobs WHERE kind = $1 AND payload->>'dims' = $2")
            .bind(JOB_KIND)
            .bind(DIMS.to_string())
            .execute(&pool)
    };

    let run = async {
        // Clear away everything left by the previous run, and start from "nothing at all"
        vector_index::drop(&pool, Target::Chunks, DIMS).await?;
        forget_jobs().await?;
        let queued = vector_index::request(&pool, Target::Chunks, DIMS).await?;
        assert!(
            queued.is_some(),
            "{DIMS} dimensions also queue an index build task"
        );

        let built = vector_index::build(&pool, Target::Chunks, DIMS).await?;
        assert!(built.created);
        let def: String = sqlx::query_scalar(
            "SELECT pg_get_indexdef(indexrelid) FROM pg_index WHERE indexrelid = $1::regclass",
        )
        .bind(&built.name)
        .fetch_one(&pool)
        .await?;
        assert!(def.contains("halfvec_cosine_ops"), "{def}");

        let before = index_scans(&indexed).await?;
        let top =
            utopia_store::documents::vector_search(&indexed, f.big, &head(1.0, 0.0), 2, None, None)
                .await?;
        let small = utopia_store::documents::vector_search(
            &indexed,
            f.small,
            &head(1.0, 0.0),
            10,
            None,
            None,
        )
        .await?;
        let after = index_scans(&indexed).await?;

        assert!(
            after > before,
            "retrieval went through the half-precision index"
        );
        assert_eq!(
            top,
            vec![f.nearer, f.farther],
            "ordering uses full-precision distance"
        );
        assert_eq!(
            small,
            vec![f.only],
            "a small store beside a large one also gets full recall"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = vector_index::drop(&pool, Target::Chunks, DIMS).await;
    forget_jobs().await?;
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
