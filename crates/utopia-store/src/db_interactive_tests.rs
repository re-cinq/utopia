//! Interactive reads get their own small pool with a Postgres-enforced statement timeout. Skipped when `UTOPIA_DATABASE_URL` is not set.
use super::*;
use std::time::Duration;

const SHORT_TIMEOUT: Duration = Duration::from_millis(200);

async fn show_statement_timeout(pool: &PgPool) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(pool)
        .await?)
}

async fn short_timeout_pool(url: &str, size: u32) -> anyhow::Result<PgPool> {
    connect_interactive(url, size, SHORT_TIMEOUT).await
}

#[test]
fn interactive_pool_size_is_a_quarter_of_the_main_pool() {
    let sizes = [32, 100, 9].map(interactive_pool_size);
    assert_eq!(sizes, [8, 25, 2]);
}

#[test]
fn interactive_pool_size_is_never_fewer_than_two() {
    let sizes = [8, 4, 2, 0].map(interactive_pool_size);
    assert_eq!(sizes, [2, 2, 2, 2]);
}

#[tokio::test]
async fn interactive_pool_reports_200ms_statement_timeout() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = short_timeout_pool(&url, 2).await?;
    assert_eq!(show_statement_timeout(&pool).await?, "200ms");
    Ok(())
}

#[tokio::test]
async fn interactive_pool_answers_a_fast_statement() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = short_timeout_pool(&url, 2).await?;
    let answer: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&pool).await?;
    assert_eq!(answer, 1);
    Ok(())
}

#[tokio::test]
async fn interactive_pool_cancels_a_two_second_sleep_with_sqlstate_57014() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = short_timeout_pool(&url, 2).await?;
    let failure = sqlx::query("SELECT pg_sleep(2)").execute(&pool).await;
    let code = match failure {
        Err(sqlx::Error::Database(db_error)) => db_error.code().map(|code| code.into_owned()),
        other => anyhow::bail!("expected a database error, got {other:?}"),
    };
    assert_eq!(code.as_deref(), Some("57014"));
    Ok(())
}

#[tokio::test]
async fn interactive_pool_keeps_serving_after_a_cancelled_statement() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = short_timeout_pool(&url, 2).await?;
    sqlx::query("SELECT pg_sleep(2)")
        .execute(&pool)
        .await
        .unwrap_err();
    let mut first = pool.acquire().await?;
    let mut second = pool.acquire().await?;
    let answers: [i32; 2] = [
        sqlx::query_scalar("SELECT 1")
            .fetch_one(&mut *first)
            .await?,
        sqlx::query_scalar("SELECT 2")
            .fetch_one(&mut *second)
            .await?,
    ];
    assert_eq!(answers, [1, 2]);
    Ok(())
}

#[tokio::test]
async fn ordinary_pool_has_no_statement_timeout() -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let pool = connect(&url, Some(2)).await?;
    assert_eq!(show_statement_timeout(&pool).await?, "0");
    Ok(())
}

#[tokio::test]
async fn busy_interactive_pool_refuses_a_third_connection_while_the_ordinary_pool_answers(
) -> anyhow::Result<()> {
    let Some(url) = crate::test_db::url() else {
        return Ok(());
    };
    let interactive = short_timeout_pool(&url, 2).await?;
    let ordinary = connect(&url, Some(2)).await?;
    let _held_first = interactive.acquire().await?;
    let _held_second = interactive.acquire().await?;

    let third = tokio::time::timeout(Duration::from_millis(300), interactive.acquire()).await;
    let ordinary_answer: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&ordinary).await?;
    assert_eq!((third.is_err(), ordinary_answer), (true, 1));
    Ok(())
}
