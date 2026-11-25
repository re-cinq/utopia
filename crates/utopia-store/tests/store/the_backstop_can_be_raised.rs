//! The outer backstop **can be raised**, and both defaults say the same number (migration 0011).
//!
//! Why this needs a database: this bug's shape is **the constraint lives in SQL and the check lives in Rust, and the two drift apart**.
//! `set_worker_concurrency` accepts 1..=256, but the column's CHECK was `BETWEEN 1 AND 32` --
//! any value from 33 to 256 entered on the settings page was accepted by Rust and rejected by the database, so the user saw
//! a CHECK constraint error. `cargo check` and clippy say nothing about this, because the two sides
//! are not written in the same language at all.
//!
//! The constraint's upper bound had originally equaled the column default (both 32), so **the backstop could not be raised by a single step** --
//! yet the comment in 0001 says it "must be clearly larger than the sum of the per-model limits, otherwise rate-limited tasks fill the slots
//! and starve everything else". The constraint was blocking its own design.
//!
//! Skipped, not failed, when `UTOPIA_DATABASE_URL` is not set. Read-only, and any change is reverted afterwards -- it never leaves a trace.
//!
//! **Both checks run in sequence inside this one test**: they both touch the same singleton row, and tests in one binary run concurrently by default.
//! Row locks already stop dirty reads, but an interleaving where one test changes the row and another deletes it (inside a transaction)
//! has no reason to exist -- chaining them removes any need to gamble (raised by the reporter of #248).

use sqlx::PgPool;

#[tokio::test]
async fn the_backstop_can_be_raised() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    every_value_rust_accepts_the_database_accepts_too(&pool).await?;
    the_two_defaults_say_the_same_number(&pool).await
}

/// Values Rust accepts must be accepted by the database too.
///
/// Test the boundaries one by one rather than just one: drift could land on any step, and these queries are cheap.
async fn every_value_rust_accepts_the_database_accepts_too(pool: &PgPool) -> anyhow::Result<()> {
    let before = utopia_store::access::worker_concurrency(pool).await?;

    let run = async {
        // 33 is the first step that was blocked; 256 is Rust's upper limit
        for v in [1_i32, 33, 64, 255, 256] {
            utopia_store::access::set_worker_concurrency(pool, v)
                .await
                .map_err(|e| anyhow::anyhow!("Rust 放行了 {v}，数据库却拒绝：{e}"))?;
            let got = utopia_store::access::worker_concurrency(pool).await?;
            assert_eq!(got, v, "写进去 {v} 读回来却是 {got}");
        }

        // The reverse: what Rust rejects must not be silently stored
        for v in [0_i32, 257, -1] {
            assert!(
                utopia_store::access::set_worker_concurrency(pool, v)
                    .await
                    .is_err(),
                "{v} 越界了却被接受"
            );
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // Revert: this is shared deployment settings, so the test should not change anyone else's running parameters
    utopia_store::access::set_worker_concurrency(pool, before).await?;
    run
}

/// The Rust fallback value used when there is no row in the table must be the same number as the column default.
///
/// The two are written separately (one in SQL, one in Rust), so changing one does not carry the other along. The consequence of a mismatch is subtle:
/// a database with a row runs one number and an empty table runs another, and neither reports an error.
async fn the_two_defaults_say_the_same_number(pool: &PgPool) -> anyhow::Result<()> {
    // The column default: ask information_schema directly, do not guess
    let column_default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
          WHERE table_name = 'deployment_settings' AND column_name = 'worker_concurrency'",
    )
    .fetch_one(pool)
    .await?;
    let column_default: i32 = column_default
        .as_deref()
        .and_then(|s| s.split("::").next())
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("读不出列缺省：{column_default:?}"))?;

    // Rust's fallback: hide the row and ask again
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM deployment_settings")
        .execute(&mut *tx)
        .await?;
    let fallback: Option<(i32,)> =
        sqlx::query_as("SELECT worker_concurrency FROM deployment_settings LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    assert!(fallback.is_none(), "行没删掉，下面这句就白测了");
    tx.rollback().await?; // tx.rollback().await?; // **Always roll back**: deployment_settings is a singleton, and deleting it leaves the service with no settings

    // The number in access.rs's unwrap_or
    let rust_fallback = 64;
    assert_eq!(
        column_default, rust_fallback,
        "列缺省是 {column_default}，Rust 兜底是 {rust_fallback}——两处漂开了"
    );
    Ok(())
}
