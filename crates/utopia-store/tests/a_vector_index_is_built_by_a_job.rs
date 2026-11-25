//! The index is built by a job (0035): the write side only queues "one", and queuing once is enough.
//!
//! - The first time a dimension is written: queue one `build_vector_index`
//! - Writes after that: queue nothing (one is already in the queue)
//! - Writes after it has been built: queue nothing (remembered in the process)
//! - A dimension beyond the HNSW limit: queue nothing and cannot build it -- this kind of failure should not be retried
//!
//! The consistency of the index build against queries is covered in `the_nearest_chunk_is_found_however_it_is_reached`.

use sqlx::PgPool;
use utopia_store::vector_index::{self, Target, JOB_KIND, MAX_DIMS};

/// The dimension this test owns exclusively
const DIMS: usize = 9;

async fn queued(pool: &PgPool, dims: usize) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = $1 AND payload = $2 AND status = 'queued'",
    )
    .bind(JOB_KIND)
    .bind(serde_json::json!({ "table": "chunks", "dims": dims }))
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn dropping_an_index_waits_for_a_build() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let mut holder = pool.acquire().await?;
    let lock = "SELECT pg_try_advisory_lock(hashtextextended('vector_index:build', 0))";
    loop {
        if sqlx::query_scalar::<_, bool>(lock)
            .fetch_one(&mut *holder)
            .await?
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // An absent index makes DROP CONCURRENTLY quick unless it takes the build lock.
    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        vector_index::drop(&pool, Target::Chunks, 17),
    )
    .await
    .is_err();
    let unlocked: bool =
        sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended('vector_index:build', 0))")
            .fetch_one(&mut *holder)
            .await?;
    assert!(unlocked);
    assert!(blocked, "DROP must wait for an in-progress index build");
    vector_index::drop(&pool, Target::Chunks, 17).await?;
    Ok(())
}

#[tokio::test]
async fn the_first_write_of_a_dimension_queues_one_build() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // Clear out whatever an earlier run left, starting from "nothing at all"
    vector_index::drop(&pool, Target::Chunks, DIMS).await?;
    sqlx::query("DELETE FROM jobs WHERE kind = $1 AND payload->>'dims' = $2")
        .bind(JOB_KIND)
        .bind(DIMS.to_string())
        .execute(&pool)
        .await?;

    let run = async {
        let first = vector_index::request(&pool, Target::Chunks, DIMS).await?;
        assert!(
            first.is_some(),
            "first write of 9 dimensions enqueues one job"
        );
        assert_eq!(queued(&pool, DIMS).await?, 1);
        let second = vector_index::request(&pool, Target::Chunks, DIMS).await?;
        assert_eq!(
            second, None,
            "a job is already queued, do not enqueue another"
        );
        assert_eq!(queued(&pool, DIMS).await?, 1);
        // A worker has claimed it and is building: writes do not queue again. Each queued one takes a worker slot,
        // and would sit idle on the index build's advisory lock until the earlier one finishes (on a 3072-dimension database, one build takes over an hour)
        sqlx::query("UPDATE jobs SET status = 'running' WHERE kind = $1 AND payload->>'dims' = $2")
            .bind(JOB_KIND)
            .bind(DIMS.to_string())
            .execute(&pool)
            .await?;
        let while_building = vector_index::request(&pool, Target::Chunks, DIMS).await?;
        assert_eq!(
            while_building, None,
            "do not enqueue while it is being built"
        );
        assert_eq!(queued(&pool, DIMS).await?, 0);

        let built = vector_index::build(&pool, Target::Chunks, DIMS).await?;
        assert!(built.created);
        assert_eq!(
            vector_index::status(&pool, Target::Chunks, DIMS).await?,
            Some(true)
        );
        // When the job finishes, that one is marked done; simulate it here so the next question relies only on "remembered in the process"
        sqlx::query("UPDATE jobs SET status = 'done' WHERE kind = $1 AND payload->>'dims' = $2")
            .bind(JOB_KIND)
            .bind(DIMS.to_string())
            .execute(&pool)
            .await?;
        let third = vector_index::request(&pool, Target::Chunks, DIMS).await?;
        assert_eq!(third, None, "built: enqueue nothing");
        assert_eq!(queued(&pool, DIMS).await?, 0);

        // A dimension beyond the limit
        let too_wide = MAX_DIMS + 1;
        assert_eq!(
            vector_index::request(&pool, Target::Chunks, too_wide).await?,
            None,
            "建不了的不排"
        );
        assert_eq!(queued(&pool, too_wide).await?, 0);
        let err = vector_index::build(&pool, Target::Chunks, too_wide)
            .await
            .expect_err("HNSW on halfvec holds up to 4000 dims");
        assert!(
            matches!(err, utopia_core::AppError::Validation(_)),
            "是 Validation，分发层据此判 Terminal：{err:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;

    let _ = vector_index::drop(&pool, Target::Chunks, DIMS).await;
    sqlx::query("DELETE FROM jobs WHERE kind = $1 AND payload->>'dims' = $2")
        .bind(JOB_KIND)
        .bind(DIMS.to_string())
        .execute(&pool)
        .await?;
    run
}

/// Two dimensions built at once, with a transaction also touching `chunks` alongside. Each of two `CREATE INDEX CONCURRENTLY` must wait
/// for the other transactions on the table to finish, and each also counts as a transaction the other is waiting on, so Postgres reports deadlock detected: reproduced locally
/// about half the time in one round, and never when the two are staggered by more than 10ms (#886 hit it 3 out of 3 times when the tests were merged into one process; the server's
/// workers claiming two build_vector_index jobs concurrently is the same thing, with concurrency defaulting to 64).
/// The session-level advisory lock inside `build` makes the later one wait for the earlier one to finish. Run eight rounds: without the lock it almost always fails.
#[tokio::test]
async fn two_dimensions_built_at_once_take_turns() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    // The two dimensions this test owns exclusively
    const A: usize = 11;
    const B: usize = 13;
    let mut outcome = Ok(());
    for round in 0..8 {
        for dims in [A, B] {
            vector_index::drop(&pool, Target::Chunks, dims).await?;
        }
        // Ingestion is still writing to chunks: CIC has to wait on those transactions, and the deadlock is only assembled with them present
        let writer = async {
            for _ in 0..20 {
                let mut tx = pool.begin().await?;
                sqlx::query("SELECT count(*) FROM chunks")
                    .execute(&mut *tx)
                    .await?;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                tx.commit().await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let (a, b, w) = tokio::join!(
            vector_index::build(&pool, Target::Chunks, A),
            vector_index::build(&pool, Target::Chunks, B),
            writer
        );
        w?;
        match (a, b) {
            (Ok(a), Ok(b)) => assert!(
                a.created && b.created,
                "第 {round} 轮两条都建成：{a:?} {b:?}"
            ),
            (a, b) => {
                outcome = Err(anyhow::anyhow!("round {round}: {a:?} / {b:?}"));
                break;
            }
        }
    }
    for dims in [A, B] {
        let _ = vector_index::drop(&pool, Target::Chunks, dims).await;
    }
    outcome
}
