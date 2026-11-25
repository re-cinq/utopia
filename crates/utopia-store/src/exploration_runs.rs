//! The ledger of one mapping exploration run: how big a thing it scanned, what it dropped, and how much it covered (#503).
//!
//! Before, an exploration run left only two things: several rows in `concept_mappings`, and a warning for "not a single one was proposed".
//! **Coverage was not reported anywhere** — eleven proposals against an eighty-column wide table,
//! and eleven that exactly cover a small database, looked identical on the page.
//!
//! Dropping was even quieter. `explore_mappings` has four `continue`s for each proposal, and none of the four is counted.
//! Hit once in practice: the source was named `tpch-2026-09-08-12-30`, the model returned `tpch` looking at the schema,
//! and all twelve were swallowed without a trace while the task still went `done`.
//!
//! Its split from `extraction_drops` follows that table's own words: that one answers "these facts did not land", and its reader is
//! the person who uploads documents; this one answers "how big a thing this run saw and how much it covered", and its reader is
//! the person who manages the data source, whose action is to add column comments or run another round.

use sqlx::PgPool;
use utopia_core::models::ExplorationRun;
use utopia_core::AppResult;
use uuid::Uuid;

/// Drop reason codes. The frontend looks up the copy by these, so they are a stable contract: do not change the literals
/// (same convention as `extraction_drops::reason`).
pub mod drop_reason {
    /// The model's returned `source` matches no mounted source's name. **The most common one**: the source name carries an
    /// environment suffix or timestamp, and the model, reading the schema, returned a short name
    pub const SOURCE: &str = "source";
    /// `kind` is not metric / dimension
    pub const KIND: &str = "kind";
    /// Concept lookup finds nothing: the ontology has no Metric / Dimension, so
    /// after `ensure_concept_types` this should not happen
    pub const TYPE: &str = "type";
    /// `definition` is not an object, or no executable definition can be extracted from it
    pub const DEFINITION: &str = "definition";
    /// The model returned more entries than this run's cap, and the excess was not looked at. The cap is set by the number of tables; returning more than that
    /// usually means the model treated columns as definitions — common on wide tables
    pub const CAP: &str = "cap";
    /// This concept already has a human verdict on this source (confirmed or rejected). Proposals do not override decisions,
    /// so when the next run computes the same concept, it neither goes back to pending review nor counts as written by this run
    pub const DECIDED: &str = "decided";
}

/// Opens a run. **Open the row before doing the work**, so a run that crashed still has a record of it.
/// Failure used to look the same as "ran but proposed nothing": both showed as "no new proposals" on the page.
pub async fn start(pool: &PgPool, kb_id: Uuid) -> AppResult<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO mapping_exploration_runs (id, kb_id) VALUES ($1, $2)")
        .bind(id)
        .bind(kb_id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Right after reading the schema and before asking the model, record the scale first. **Asking the model can take a minute**,
/// and during that time the page should show how big a thing is being scanned.
pub async fn scanned(
    pool: &PgPool,
    id: Uuid,
    sources: &[String],
    tables: i32,
    columns: i32,
    truncated: bool,
    cap: i32,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE mapping_exploration_runs
            SET sources = $2, tables_scanned = $3, columns_scanned = $4,
                schema_truncated = $5, cap = $6
          WHERE id = $1",
    )
    .bind(id)
    .bind(sources)
    .bind(tables)
    .bind(columns)
    .bind(truncated)
    .bind(cap)
    .execute(pool)
    .await?;
    Ok(())
}

/// Closes a run. The shape of `dropped` is described at [`drop_reason`]: each reason gets one
/// `{"n": count, "example": "one example"}` — **a count alone diagnoses nothing**;
/// "twelve source names did not match" needs "the model said tpch, but the mount is tpch-2026-09-08"
/// before you know what to change.
pub async fn finish(
    pool: &PgPool,
    id: Uuid,
    returned: i32,
    accepted: i32,
    dropped: serde_json::Value,
    tables_covered: &[String],
) -> AppResult<()> {
    sqlx::query(
        "UPDATE mapping_exploration_runs
            SET finished_at = now(), returned = $2, accepted = $3,
                dropped = $4, tables_covered = $5
          WHERE id = $1",
    )
    .bind(id)
    .bind(returned)
    .bind(accepted)
    .bind(dropped)
    .bind(tables_covered)
    .execute(pool)
    .await?;
    Ok(())
}

/// This run failed. Record it and stop — **a failure on the error path must not drown out the thing it is reporting**;
/// callers always use `let _ =` (same convention as `extraction_drops`).
pub async fn fail(pool: &PgPool, id: Uuid, error: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE mapping_exploration_runs
            SET finished_at = now(), error = left($2, 500) WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// The most recent runs, newest first. The data mapping page reads this.
pub async fn recent(pool: &PgPool, kb_id: Uuid, limit: i64) -> AppResult<Vec<ExplorationRun>> {
    Ok(sqlx::query_as(
        "SELECT id, started_at, finished_at, sources, tables_scanned, columns_scanned,
                schema_truncated, cap, returned, accepted, dropped, tables_covered, error
           FROM mapping_exploration_runs
          WHERE kb_id = $1
          ORDER BY started_at DESC
          LIMIT $2",
    )
    .bind(kb_id)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}
