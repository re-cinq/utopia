//! Entry point for integration tests that connect to a database (#248).
//!
//! Every DB-connected test opens with the same line: without `UTOPIA_DATABASE_URL` it skips rather than fails,
//! so a plain local `cargo test` needs no database running first. On CI it also skips that way, so green becomes fake: the backend job
//! once had no database, and all 24 store integration tests silently returned, while the migrations job, which did have one, ran only one.
//!
//! So skipping depends on the context: where `UTOPIA_TEST_REQUIRE_DB` is set (CI's backend job,
//! which now brings its own Postgres), having no database is a failure: what was meant to run but did not must stay visible.
//!
//! Here we only supply the address and do not migrate: most callers assume the database is already migrated. For an empty database, first run
//! `cargo run -p utopia-store --example migrate`, as CI does too.

/// The database address for DB-connected tests. `None` = skip this run.
///
/// Panics when `UTOPIA_TEST_REQUIRE_DB` is set without an address: this is for CI. There, skipping
/// is the same as the test not running at all, so it must not show green.
pub fn url() -> Option<String> {
    match std::env::var("UTOPIA_DATABASE_URL") {
        Ok(u) if !u.trim().is_empty() => Some(u),
        _ => {
            if std::env::var_os("UTOPIA_TEST_REQUIRE_DB").is_some() {
                panic!(
                    "UTOPIA_TEST_REQUIRE_DB is set but UTOPIA_DATABASE_URL is not:                      this run must not skip database-backed tests"
                );
            }
            eprintln!(
                "跳过：未设 UTOPIA_DATABASE_URL（设 UTOPIA_TEST_REQUIRE_DB=1 让跳过变成失败）"
            );
            None
        }
    }
}
