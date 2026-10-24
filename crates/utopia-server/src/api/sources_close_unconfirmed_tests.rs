//! `close-unconfirmed` 收写令牌（RC-004）：读令牌 403，别的库的来源 404。
//! 关哪些、怎么关由 store 的 `a_source_closes_its_unconfirmed_facts` 钉住。
use crate::state::AppState;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn a_write_token_closes_through_its_own_bases_source_only() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    let [org, ws, editor, kb, other_kb, source, foreign] = [(); 7].map(|_| Uuid::now_v7());
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations(id,name) VALUES ('{org}','close-unconfirmed-route');
         INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','close-unconfirmed-route');
         INSERT INTO users(id,org_id,email,display_name,password_hash) VALUES
             ('{editor}','{org}','{editor}@close-unconfirmed.test','Editor','unused');
         INSERT INTO memberships(user_id,workspace_id,role) VALUES ('{editor}','{ws}','editor');
         INSERT INTO knowledge_bases(id,workspace_id,name) VALUES
             ('{kb}','{ws}','mirror'), ('{other_kb}','{ws}','other');
         INSERT INTO kb_members(kb_id,user_id,role) VALUES
             ('{kb}','{editor}','editor'), ('{other_kb}','{editor}','editor');
         INSERT INTO sources(id,kb_id,kind,name) VALUES
             ('{source}','{kb}','statements','mirror'), ('{foreign}','{other_kb}','statements','other');"
    ))
    .execute(&pool)
    .await?;
    let dir = tempfile::tempdir()?;
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let search = Arc::new(utopia_search::SearchIndex::open(
        &dir.path().join("search"),
    )?);
    let app = super::super::router(
        AppState::new(pool.clone(), &cfg, search, "test-only".into()),
        &cfg,
    );
    let post = |source: Uuid, token: String| {
        let app = app.clone();
        async move {
            let request = Request::post(format!(
                "/api/v1/kbs/{kb}/sources/{source}/close-unconfirmed"
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(json!({ "limit": 10 }).to_string()))?;
            let response = app.oneshot(request).await?;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            anyhow::Ok((
                status,
                serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
            ))
        }
    };
    let result = async {
        let issue = |scope: &'static str| {
            let pool = pool.clone();
            async move {
                anyhow::Ok(
                    utopia_store::tokens::issue(&pool, editor, "ci", scope, None, None)
                        .await?
                        .1,
                )
            }
        };
        let (write, read) = (issue("write").await?, issue("read").await?);
        assert_eq!(
            post(source, write.clone()).await?,
            (
                StatusCode::OK,
                json!({ "closed": 0, "skipped": 0, "remaining": 0 })
            )
        );
        assert_eq!(post(source, read).await?.0, StatusCode::FORBIDDEN);
        assert_eq!(post(foreign, write).await?.0, StatusCode::NOT_FOUND);
        anyhow::Ok(())
    }
    .await;
    sqlx::raw_sql(&format!(
        "DELETE FROM knowledge_bases WHERE id IN ('{kb}','{other_kb}');
         DELETE FROM organizations WHERE id = '{org}';"
    ))
    .execute(&pool)
    .await?;
    result
}
