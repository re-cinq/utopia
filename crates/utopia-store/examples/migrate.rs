//! Migrates the database that `UTOPIA_DATABASE_URL` points to up to the latest version, then exits.
//!
//! DB tests assume the database is already migrated: the store's hundred-plus integration tests and most server fixtures create their data straight away
//! without running migrations themselves; only a handful call `db::migrate`. Running `cargo test --workspace` against a brand-new empty database
//! and the first test to run hits relation "organizations" does not exist
//! (that is how 32 tests went red when #869 was verified). CI used to run `migrate run` via sqlx-cli first, but installing it
//! takes over a minute, and the DB tests now run in the backend job on the critical path; this example reuses the utopia-store build the tests need anyway,
//! so it adds almost no time. Locally, likewise: `docker compose up -d db`
//! then `cargo run -p utopia-store --example migrate`, and the DB tests can run.
//!
//! It only rolls forward. It does not do the other thing sqlx-cli does: re-running on an already-migrated database to check it is safe,
//! that is still the migrations job's work.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("UTOPIA_DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("UTOPIA_DATABASE_URL is not set"))?;
    // Creating tables and triggers needs more privilege than runtime, so like utopia-server's startup, open one small pool and close it when done
    let pool = utopia_store::db::connect(&url, Some(2)).await?;
    utopia_store::db::migrate(&pool).await?;
    pool.close().await;
    // Do not echo the address: it carries a password, and this line goes into CI logs
    println!("数据库迁移完成");
    Ok(())
}
