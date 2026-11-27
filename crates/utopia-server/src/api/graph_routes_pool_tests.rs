//! The graph read routes query through the interactive pool. Skipped when `UTOPIA_DATABASE_URL` is not set.
//! Authentication reads through the main pool, so the pool a route's query uses is observed by holding the interactive pool instead.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::postgres::PgPoolOptions;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn entity_search_answers_500_when_every_interactive_connection_is_held() -> anyhow::Result<()>
{
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let [org, ws, kb, reader] = std::array::from_fn(|_| Uuid::now_v7());
    // Only locally generated UUIDs are interpolated into fixture SQL.
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','graph-pool');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','graph-pool');
         INSERT INTO users(id,org_id,email,display_name,password_hash)
              VALUES ('{reader}','{org}','{reader}@pool.test','reader','unused');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','graph-pool');
         INSERT INTO kb_members(kb_id,user_id,role) VALUES ('{kb}','{reader}','viewer');"
    ))
    .execute(&pool)
    .await?;

    let result = async {
        let dir = tempfile::tempdir()?;
        let config = utopia_core::config::AppConfig {
            data_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let search = Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?);
        let interactive = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(400))
            .connect(&url)
            .await?;
        let state = crate::state::AppState::new(pool.clone(), &config, search, "test-only".into())
            .with_interactive_pool(interactive.clone());
        let token = crate::auth::issue_token(&state, reader)?;
        let app = crate::api::router(state, &config);
        let held = (interactive.acquire().await?, interactive.acquire().await?);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/kbs/{kb}/entities?q=Alice"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())?,
            )
            .await?;
        drop(held);
        Ok::<StatusCode, anyhow::Error>(response.status())
    }
    .await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    assert_eq!(result?, StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}
