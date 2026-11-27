//! Pools are told apart by `max_connections`: lazy pools need no database, and `PgPool` exposes no other identity.
use super::*;
use sqlx::postgres::PgPoolOptions;

const MAIN_SIZE: u32 = 3;
const INTERACTIVE_SIZE: u32 = 5;

fn lazy_pool(max_connections: u32) -> PgPool {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_lazy("postgres://localhost/never_connected")
        .unwrap()
}

fn size_of(pool: &PgPool) -> u32 {
    pool.options().get_max_connections()
}

fn state_over(pool: PgPool) -> anyhow::Result<AppState> {
    let dir = tempfile::tempdir()?;
    let config = AppConfig {
        data_dir: dir.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(SearchIndex::open(&dir.path().join("search"))?);
    Ok(AppState::new(pool, &config, search, "test-only".into()))
}

#[tokio::test]
async fn interactive_reads_use_the_main_pool_without_an_interactive_pool() -> anyhow::Result<()> {
    let state = state_over(lazy_pool(MAIN_SIZE))?;
    assert_eq!(size_of(&state.for_interactive_reads().pool), MAIN_SIZE);
    Ok(())
}

#[tokio::test]
async fn interactive_reads_use_the_interactive_pool_and_leave_the_main_pool() -> anyhow::Result<()>
{
    let state =
        state_over(lazy_pool(MAIN_SIZE))?.with_interactive_pool(lazy_pool(INTERACTIVE_SIZE));
    let reads = state.for_interactive_reads();
    let sizes = [
        size_of(&reads.pool),
        size_of(&reads.interactive_pool),
        size_of(&state.pool),
    ];
    assert_eq!(sizes, [INTERACTIVE_SIZE, INTERACTIVE_SIZE, MAIN_SIZE]);
    Ok(())
}
