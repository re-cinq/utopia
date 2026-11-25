//! The vector index is built by a task, not by a numbered migration (0035).
//!
//! The column is dimensionless (`embedding vector`, no `(N)`): the dimension follows the embedding model the workspace picked,
//! which is not known yet when the migration runs, and pgvector cannot build an HNSW index for an unknown dimension. On the other side,
//! `CREATE INDEX CONCURRENTLY` cannot run inside the transaction sqlx wraps around migrations, and without CONCURRENTLY
//! it would hold an ACCESS EXCLUSIVE lock on `chunks` until the build finishes. So: the first time a vector of some dimension is written,
//! queue a task, and the task builds a **per-dimension partial expression index** outside the transaction,
//! with `IF NOT EXISTS` making re-queuing harmless.
//!
//! The read path has three rules; missing any one makes the index quietly stop applying, or quietly return fewer rows:
//! 1. **Write the dimension as a literal.** With `vector_dims(col) = $2` and a bound parameter, the custom plan can use the index,
//!    but the generic plan falls back to a sequential scan; sqlx's prepared statements switch to generic after five runs, and from the sixth on the index
//!    quietly stops working. Use [`same_dims`] / [`distance`] to put the integer into the SQL.
//! 2. **`ORDER BY` the index's expression verbatim.** Use [`index_order`]: the index is built on
//!    `col::vector(N)` (above [`VECTOR_MAX_DIMS`] it is `col::halfvec(N)`),
//!    and if written any other way the index will not be used. The distance selected is [`distance`], always full precision.
//! 3. **`hnsw.iterative_scan = relaxed_order`.** HNSW first fetches `ef_search` candidates and only then applies
//!    the WHERE; on a table partitioned per KB, a small KB's rows make up only a few of those candidates, and `LIMIT 10` returns
//!    3 of 24 (measured: a 10k-row KB on a 60k-row table returned 3 of 24 with it off, and the full set with it on).
//!    iterative_scan keeps going until enough rows are found. It has two stopping conditions, both relaxed here:
//!    `hnsw.scan_mem_multiplier` raised from 1 to 4 (16 MB when the default work_mem is 4 MB) — measured:
//!    this one is what actually binds: a real KB takes 1% of the index, next to a 50k-row synthetic tenant, with the index forced;
//!    at multiplier 1, 154 of 522 queries did not fill, recall 0.705, 32 ms per query; at multiplier 4, all filled,
//!    recall 1.0, 74 ms per query, and 16 vs 4 makes no difference. `hnsw.max_scan_tuples` raised from 20,000 to
//!    100,000; measured, it did not bind, and the reason for raising it is to make "hitting the cap" a visible slowdown rather than quietly fewer rows.
//!
//! Whether to use the index is decided by the planner: with `chunks_kb_idx`, small and mid-size KBs take the exact path on their own, and only
//! KBs that make up most of the table use HNSW (measured on 60k rows: KBs of 20 and 10k rows take the exact path, and the 50k one uses the index).
//! The app sets no threshold — a threshold is a guess about the planner, and a wrong guess is slow on both sides.

use sqlx::{Executor, PgConnection, PgPool, Postgres, Transaction};
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use utopia_core::{AppError, AppResult};

/// The task kind; the dispatch in `main.rs` matches on this name
pub const JOB_KIND: &str = "build_vector_index";

/// The session-level advisory lock for building an index (see [`build`]). The key is a string hash, using the same approach as the timeline lock in temporal.
/// Uses only the **try** version: a blocking `pg_advisory_lock` waiting for the lock is itself a statement holding a snapshot transaction,
/// and CONCURRENTLY, at its last step, waits for every snapshot older than itself to end — the builder waits for the queued, the queued waits for the builder —
/// deadlocked in another form (reproduced locally every round). It probes and returns at once without holding a snapshot; the waiting is done on the client side
const BUILD_TRY_LOCK: &str =
    "SELECT pg_try_advisory_lock(hashtextextended('vector_index:build', 0))";
const BUILD_UNLOCK: &str = "SELECT pg_advisory_unlock(hashtextextended('vector_index:build', 0))";
/// How long to wait before probing again after failing to get the lock. Building an index takes tens of seconds to a few minutes, so quarter-second granularity is enough
const BUILD_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(250);

async fn lock_build(conn: &mut PgConnection) -> AppResult<()> {
    loop {
        let got: bool = sqlx::query_scalar(BUILD_TRY_LOCK)
            .fetch_one(&mut *conn)
            .await?;
        if got {
            return Ok(());
        }
        tokio::time::sleep(BUILD_LOCK_POLL).await;
    }
}

async fn unlock_build(mut conn: sqlx::pool::PoolConnection<Postgres>) {
    let unlocked: Result<bool, _> = sqlx::query_scalar(BUILD_UNLOCK).fetch_one(&mut *conn).await;
    if !matches!(unlocked, Ok(true)) {
        let _ = conn.close().await;
    }
}

/// The upper limit of pgvector's HNSW for the `vector` type
pub const VECTOR_MAX_DIMS: usize = 2000;

/// The upper limit for building an index: that of `halfvec`. Dimensions above [`VECTOR_MAX_DIMS`] (text-embedding-3-large
/// is 3072) store the index at half precision, which is only responsible for picking out candidates; the candidates' distances and final ordering are still computed at full `vector` precision
/// ([`distance`] plus the outer [`RESORT`]), so half precision can only affect who makes it into the candidates, never the score.
/// Without an index, such a dimension scans the whole database on every query (3072 dimensions, two hundred-odd thousand name vectors, tens of seconds per query).
/// Anything wider gets no index, and queries take the exact path as usual
pub const MAX_DIMS: usize = 4000;

/// The type stored in the index: use `vector` when possible, drop to `halfvec` only when it does not fit
fn index_type(dims: usize) -> &'static str {
    if dims > VECTOR_MAX_DIMS {
        "halfvec"
    } else {
        "vector"
    }
}

/// Which column. The two tables use the same mechanism; only the names differ
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// `chunks.embedding`: document chunks, the table that grows without bound
    Chunks,
    /// `entities.profile_embedding`: entity profiles, which type resolution scans one by one per subject (#514)
    EntityProfiles,
    /// `name_vectors.embedding`: vectors of name strings, the second recall channel (0041 part 2)
    NameVectors,
}

impl Target {
    pub fn table(self) -> &'static str {
        match self {
            Target::Chunks => "chunks",
            Target::EntityProfiles => "entities",
            Target::NameVectors => "name_vectors",
        }
    }

    pub fn column(self) -> &'static str {
        match self {
            Target::Chunks => "embedding",
            Target::EntityProfiles => "profile_embedding",
            Target::NameVectors => "embedding",
        }
    }

    /// The name in the task payload
    pub fn key(self) -> &'static str {
        self.table()
    }

    pub fn parse(key: &str) -> Option<Self> {
        match key {
            "chunks" => Some(Target::Chunks),
            "entities" => Some(Target::EntityProfiles),
            "name_vectors" => Some(Target::NameVectors),
            _ => None,
        }
    }
}

/// Index name: `chunks_embedding_hnsw_1024`
pub fn index_name(target: Target, dims: usize) -> String {
    format!("{}_{}_hnsw_{dims}", target.table(), target.column())
}

/// The predicate of the partial index; it must appear verbatim in the query (rule 1)
pub fn same_dims(column: &str, dims: usize) -> String {
    format!("vector_dims({column}) = {dims}")
}

/// Full-precision distance: the column selected for the outer ordering. `param` is the parameter number of the query vector
pub fn distance(column: &str, param: usize, dims: usize) -> String {
    format!("{column}::vector({dims}) <=> ${param}::vector({dims})")
}

/// The expression in `ORDER BY`, identical to the index (rules 1 and 2). When not above [`VECTOR_MAX_DIMS`]
/// it is [`distance`]; when above, it is half precision, used only to let the index pick candidates
pub fn index_order(column: &str, param: usize, dims: usize) -> String {
    let ty = index_type(dims);
    format!("{column}::{ty}({dims}) <=> ${param}::{ty}({dims})")
}

/// The outer ordering of a nearest-neighbour query (#652). The inner layer writes `ORDER BY {distance} LIMIT n` per rule 2 so the index
/// can take it, wraps it in a `MATERIALIZED` CTE (columns `distance`, `id`), and the outer layer sorts by this:
/// - Under `relaxed_order` the index returns rows only roughly by distance, so the outer layer does one real sort
/// - When distances tie, the two plans each sort their own way (two identical vectors: the exact path and HNSW put them in different first positions), so id settles it
/// - The `+ 0` must not be dropped: without it, the planner takes the CTE's order as already sorted (`Presorted Key: distance`),
///   so it sorts ids only within the tied groups and the out-of-order part leaks through as-is
pub const RESORT: &str = "distance + 0, id";

/// Current state of the index: `None` means none; `Some(valid)` means one exists, and `false` is the invalid index left by a build that broke off halfway
pub async fn status(pool: &PgPool, target: Target, dims: usize) -> AppResult<Option<bool>> {
    status_on(pool, target, dims).await
}

async fn status_on<'a>(
    executor: impl Executor<'a, Database = Postgres>,
    target: Target,
    dims: usize,
) -> AppResult<Option<bool>> {
    let row: Option<(bool,)> = sqlx::query_as(
        "SELECT i.indisvalid FROM pg_class c
           JOIN pg_index i ON i.indexrelid = c.oid
          WHERE c.relname = $1 AND c.relkind = 'i'",
    )
    .bind(index_name(target, dims))
    .fetch_optional(executor)
    .await?;
    Ok(row.map(|(v,)| v))
}

/// An index known in-process to already exist: after this, each write is just one lookup
fn known() -> &'static Mutex<HashSet<String>> {
    static KNOWN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    KNOWN.get_or_init(|| Mutex::new(HashSet::new()))
}

fn remember(name: &str) {
    known().lock().unwrap().insert(name.to_string());
}

fn forget(name: &str) {
    known().lock().unwrap().remove(name);
}

fn is_known(name: &str) -> bool {
    known().lock().unwrap().contains(name)
}

/// Called when a vector of some dimension is written: if the index is absent, queue an index-build task.
/// Returns `Some(job id)` = queued this time.
///
/// When the index exists it is recorded in-process, so later each call is just a HashSet lookup; when it does not exist, every write costs one catalog
/// query plus one insert of "queue only if not already queued and not being built" — writes during the build proceed as usual, and the cost is negligible.
/// Dimensions above [`MAX_DIMS`] queue nothing: no index can be built, so queries take the exact path
pub async fn request(pool: &PgPool, target: Target, dims: usize) -> AppResult<Option<i64>> {
    if dims == 0 || dims > MAX_DIMS {
        return Ok(None);
    }
    let name = index_name(target, dims);
    if is_known(&name) {
        return Ok(None);
    }
    if status(pool, target, dims).await? == Some(true) {
        remember(&name);
        return Ok(None);
    }
    crate::jobs::enqueue_unless_pending(
        pool,
        JOB_KIND,
        serde_json::json!({ "table": target.key(), "dims": dims }),
        std::time::Duration::ZERO,
    )
    .await
}

/// The result of one [`build`]
#[derive(Debug)]
pub struct Built {
    pub name: String,
    /// `false` = it was already there
    pub created: bool,
    pub seconds: f64,
}

/// Built as the table's owner. `owner_url` is the connection string for the migration identity, present only when it differs from the runtime identity.
///
/// **Building an index requires table ownership; a grant cannot substitute for it.** When a deployment splits the two identities as `.env.example` does,
/// the app's database connection uses the restricted role `utopia_app`, and here it gets `must be owner of table`:
/// the task fails three times, the index is never built, and every nearest-neighbour query falls back to a full exact table scan. On a database with 41,000
/// name vectors, the lookup entity resolution does for each mention takes a minute and a half to three minutes (#1120).
///
/// So when the identities are split, open one connection with the migration identity and close it once the build finishes — the same approach as running migrations at startup:
/// the high-privilege connection does not stay resident at runtime. When not split, `owner_url` is `None` and the app's pool is used as before.
pub async fn build_as_owner(
    app: &PgPool,
    owner_url: Option<&str>,
    target: Target,
    dims: usize,
) -> AppResult<Built> {
    let Some(url) = owner_url else {
        return build(app, target, dims).await;
    };
    let owner = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await?;
    let built = build(&owner, target, dims).await;
    owner.close().await;
    built
}

/// The task body. **Outside a transaction** (CONCURRENTLY requires it), **serial** (parallel index builds need shared memory,
/// and Docker's default 64 MB `/dev/shm` produces "could not resize shared memory segment";
/// serially, 60k rows at 1024 dimensions takes about 90 seconds, which runs everywhere). An invalid index left by a half-finished earlier build is dropped first —
/// `IF NOT EXISTS` would see it and think the index was already built.
pub async fn build(pool: &PgPool, target: Target, dims: usize) -> AppResult<Built> {
    if dims == 0 || dims > MAX_DIMS {
        return Err(AppError::Validation(format!(
            "{dims} dims: HNSW on halfvec holds up to {MAX_DIMS}"
        )));
    }
    let name = index_name(target, dims);
    let started = std::time::Instant::now();
    let mut conn = pool.acquire().await?;
    // Only one build at a time. Two CONCURRENTLY builds on the same table each wait for other transactions on the table to finish, and each
    // one also waits on the transaction the other is waiting on; add ingestion writing to chunks alongside and they deadlock (40P01; reproduced in
    // a_vector_index_is_built_by_a_job, about half the time per round). The default worker concurrency is 64, and two dimensions' build tasks claimed at once produce exactly this. The lock is session-level:
    // CONCURRENTLY cannot run inside a transaction, so a transaction-level advisory lock has nowhere to live;
    // tied to this connection, it queues across workers and instances alike. A build takes tens of seconds to a few minutes,
    // so queueing is cheaper than retrying after a deadlock
    lock_build(&mut conn).await?;
    let outcome = async {
        conn.execute("SET max_parallel_maintenance_workers = 0")
            .await?;
        // The default 64 MB maintenance_work_mem cannot hold the graph at 14,000 rows of 1024 dimensions; after that
        // every row must go to disk and be read back, and 50k rows took 4 min 20 s to build; 512 MB is held only for the duration of the build
        conn.execute("SET maintenance_work_mem = '512MB'").await?;
        // Reuse the held connection: acquiring another from a busy pool can
        // make builds wait for connections they are themselves holding.
        let before = status_on(&mut *conn, target, dims).await?;
        if before == Some(false) {
            conn.execute(format!("DROP INDEX CONCURRENTLY IF EXISTS {name}").as_str())
                .await?;
        }
        let existed = before == Some(true);
        conn.execute(
            format!(
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON {table} \
                 USING hnsw (({col}::{ty}({dims})) {ty}_cosine_ops) WHERE {pred}",
                table = target.table(),
                col = target.column(),
                pred = same_dims(target.column(), dims),
                ty = index_type(dims),
            )
            .as_str(),
        )
        .await?;
        Ok::<bool, AppError>(!existed)
    }
    .await;
    // Session-level SET follows the connection back to the pool and is reset whether the build succeeds or fails. The lock likewise — if it cannot be released, close this connection rather than
    // returning it to the pool: returning it still holding the lock would leave every later build stuck behind it
    let _ = conn.execute("RESET max_parallel_maintenance_workers").await;
    let _ = conn.execute("RESET maintenance_work_mem").await;
    if outcome.is_ok() {
        remember(&name);
    }
    unlock_build(conn).await;
    let created = outcome?;
    Ok(Built {
        name,
        created,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// Drops it (for tests and manual maintenance; the write path never reaches here)
pub async fn drop(pool: &PgPool, target: Target, dims: usize) -> AppResult<()> {
    let name = index_name(target, dims);
    let mut conn = pool.acquire().await?;
    // Dropping an index must take the same lock as building one: separate
    // concurrent index operations on the table can deadlock each other.
    lock_build(&mut conn).await?;
    let outcome = conn
        .execute(format!("DROP INDEX CONCURRENTLY IF EXISTS {name}").as_str())
        .await;
    if outcome.is_ok() {
        forget(&name);
    }
    unlock_build(conn).await;
    outcome?;
    Ok(())
}

/// `hnsw.iterative_scan` is a parameter that only pgvector 0.8 has. On older versions it is probed once and remembered in-process:
/// if the probe fails, it is not set — queries are still correct, only small KBs on a shared table may return a few fewer rows,
/// which is exactly what 0.8 fixed
async fn iterative_scan_available(pool: &PgPool) -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    if let Some(v) = AVAILABLE.get() {
        return *v;
    }
    let seen: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT current_setting('hnsw.iterative_scan', true)")
            .fetch_one(pool)
            .await;
    let ok = matches!(seen, Ok(Some(_)));
    let _ = AVAILABLE.set(ok);
    ok
}

/// Session settings for the read path (rule 3). `SET LOCAL` takes effect only inside a transaction, so nearest-neighbour queries each wrap one transaction
pub async fn relaxed_order(pool: &PgPool, tx: &mut Transaction<'_, Postgres>) -> AppResult<()> {
    if iterative_scan_available(pool).await {
        (&mut **tx)
            .execute("SET LOCAL hnsw.iterative_scan = relaxed_order")
            .await?;
        // All three parameters came in the same version (0.8). The two stopping conditions are described in the module comment: the memory one is the one measured to bind
        // the scan; the tuple cap one did not bind, and relaxing it is so that "hitting the cap" stays a visible slowdown
        (&mut **tx)
            .execute("SET LOCAL hnsw.scan_mem_multiplier = 4")
            .await?;
        (&mut **tx)
            .execute("SET LOCAL hnsw.max_scan_tuples = 100000")
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_name_carries_table_column_and_dims() {
        assert_eq!(
            index_name(Target::Chunks, 1024),
            "chunks_embedding_hnsw_1024"
        );
        assert_eq!(
            index_name(Target::EntityProfiles, 768),
            "entities_profile_embedding_hnsw_768"
        );
    }

    #[test]
    fn the_sql_writes_the_dimension_as_a_literal() {
        assert_eq!(
            same_dims("c.embedding", 1024),
            "vector_dims(c.embedding) = 1024"
        );
        assert_eq!(
            distance("c.embedding", 2, 1024),
            "c.embedding::vector(1024) <=> $2::vector(1024)"
        );
    }

    #[test]
    fn a_dimension_wider_than_vector_orders_by_halfvec_and_scores_by_vector() {
        assert_eq!(
            index_order("c.embedding", 2, 2000),
            "c.embedding::vector(2000) <=> $2::vector(2000)"
        );
        assert_eq!(
            index_order("c.embedding", 2, 3072),
            "c.embedding::halfvec(3072) <=> $2::halfvec(3072)"
        );
        assert_eq!(
            distance("c.embedding", 2, 3072),
            "c.embedding::vector(3072) <=> $2::vector(3072)"
        );
    }

    #[test]
    fn a_target_round_trips_through_the_payload() {
        for t in [Target::Chunks, Target::EntityProfiles] {
            assert_eq!(Target::parse(t.key()), Some(t));
        }
        assert_eq!(Target::parse("documents"), None);
    }
}
