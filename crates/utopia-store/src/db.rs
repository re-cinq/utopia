use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;

/// Default connection pool cap.
///
/// **It is no longer equal to the worker concurrency** (the worker default is already 64, see migration 0011), and this is deliberate:
/// background tasks spend most of their time waiting on model responses, and during that time they hold no connection and are throttled
/// to around ten by the per-model semaphore. The pool has to cover the ones that are **actually doing work** — per-chunk epoch checks, vector search,
/// unmatched-statistics and other short queries, which arrive in bursts.
///
/// It used to be hardcoded at 10, while the worker default was 32 — a threefold over-subscription. Hitting it first slows requests down, then causes timeouts,
/// instead of any single place reporting "the pool is too small". So the criterion for this number is "how many short queries run at the same time",
/// not "how many task slots there are"; when the worker is raised further, it is the former that must scale.
const DEFAULT_MAX_CONNECTIONS: u32 = 32;

pub async fn connect(database_url: &str, max_connections: Option<u32>) -> anyhow::Result<PgPool> {
    let max = max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS).max(2);
    let pool = PgPoolOptions::new()
        .max_connections(max)
        // Fail loudly and early when no connection can be had, rather than leaving the request hanging on the default 30 seconds —
        // so that when the pool is undersized, it is visible that the pool is the problem
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await?;
    tracing::info!(max_connections = max, "数据库连接池已建立");
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("../../migrations").run(pool).await?;
    Ok(())
}
