//! A source's incremental cursor is visible through the API (ADR 0072).
//!
//! **It says "how far has been read", not "when this round ran".** Not finishing a round is common: the same update
//! instant may have a thousand tickets hanging on it; the cursor records "the update instant consumed up to, and the whole group at that instant
//! has been read". Without it, the screen shows only "sync succeeded" and not where this round stopped; to work out
//! "how much this round actually covered" you could only check the logs.
//!
//! For sources that do not use it, the cell is NULL, not 0 and not some other instant: having no cursor and "the cursor is at midnight"
//! are different things: the former means "not read up to any point yet", the latter means "read up to 1970".
//!
//! Tests that use the database are skipped when there is no `UTOPIA_DATABASE_URL` (same as documents_routes_tests).

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// The cursor sits at this moment: second precision, exactly the granularity of ticket update times.
///
/// **The response writes the form that ends in `Z`** (how chrono writes `DateTime<Utc>`),
/// not `+00:00`: the assertion is written in the form actually sent, so a change is a change.
const CHECKPOINT: &str = "2026-01-02T00:00:00Z";

fn checkpoint() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(CHECKPOINT)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

struct Fixture {
    app: axum::Router,
    kb: Uuid,
    /// Ticket source: has a cursor
    ticket: Uuid,
    /// Subscription source: no cursor
    feed: Uuid,
    token: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> anyhow::Result<Option<Self>> {
        let Some(url) = utopia_store::test_db::url() else {
            return Ok(None);
        };
        let pool = sqlx::PgPool::connect(&url).await?;
        utopia_store::db::migrate(&pool).await?;
        let (org, ws, kb, user, ticket, feed) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        // Only locally generated UUIDs are interpolated into fixture SQL.
        sqlx::raw_sql(&format!(
            "INSERT INTO organizations(id,name) VALUES ('{org}','checkpoint-test');
             INSERT INTO workspaces(id,org_id,name) VALUES ('{ws}','{org}','checkpoint-test');
             INSERT INTO users(id,org_id,email,display_name,password_hash)
                 VALUES ('{user}','{org}','{user}@checkpoint.test','checkpoint-test','unused');
             INSERT INTO knowledge_bases(id,workspace_id,name) VALUES ('{kb}','{ws}','observations');
             INSERT INTO kb_members(kb_id,user_id,role) VALUES ('{kb}','{user}','editor');
             INSERT INTO sources(id,kb_id,kind,name,config)
                 VALUES ('{ticket}','{kb}','github_issues','tickets','{{\"repo\":\"o/r\"}}');
             INSERT INTO sources(id,kb_id,kind,name,config)
                 VALUES ('{feed}','{kb}','rss','feed','{{\"feed_url\":\"https://example.test/f\"}}');"
        ))
        .execute(&pool)
        .await?;
        // The ticket source has read up to that instant; the subscription source is not grouped by instant, so it has no such cell
        sqlx::query("UPDATE sources SET sync_checkpoint = $2 WHERE id = $1")
            .bind(ticket)
            .bind(checkpoint())
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
        let state = crate::state::AppState::new(pool.clone(), &cfg, search, "test-only".into());
        let token = crate::auth::issue_token(&state, user)?;
        let app = super::super::router(state.clone(), &cfg);
        Ok(Some(Self {
            app,
            kb,
            ticket,
            feed,
            token,
            _dir: dir,
        }))
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> anyhow::Result<(StatusCode, Value)> {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {}", self.token));
        let request = match body {
            Some(body) => request
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))?,
            None => request.body(Body::empty())?,
        };
        let response = self.app.clone().oneshot(request).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }
}

/// **The list has this cell.** A ticket source carries the cursor; a source that does not use a cursor is NULL.
#[tokio::test]
async fn the_source_list_carries_the_checkpoint() -> anyhow::Result<()> {
    let Some(fx) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = fx
        .call("GET", &format!("/api/v1/kbs/{}/sources", fx.kb), None)
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sources = body["sources"].as_array().expect("sources[]");
    let ticket = sources
        .iter()
        .find(|s| s["id"] == fx.ticket.to_string())
        .expect("ticket source is in the list");
    let feed = sources
        .iter()
        .find(|s| s["id"] == fx.feed.to_string())
        .expect("subscription source is in the list");

    // **The instant itself must be given**, not a count: that cell says how far has been read
    assert_eq!(
        ticket["sync_checkpoint"],
        json!(CHECKPOINT),
        "工单来源的游标要原样给出"
    );
    // A source that does not use it is NULL: having no cursor and "the cursor at midnight" are two different things
    assert_eq!(
        feed["sync_checkpoint"],
        Value::Null,
        "用不到游标的来源不该编一个时刻出来"
    );
    // The cursor and "when this round ran" are two separate things, and both are present
    assert!(
        ticket["last_sync_at"].is_null(),
        "还没同步过就没有这一轮的时刻"
    );
    Ok(())
}

/// **Change one source, and the cell that comes back also carries the cursor.** The list and the detail say the same thing,
/// so one place cannot have it while another does not.
#[tokio::test]
async fn a_source_update_carries_the_checkpoint_too() -> anyhow::Result<()> {
    let Some(fx) = Fixture::new().await? else {
        return Ok(());
    };
    let (status, body) = fx
        .call(
            "PATCH",
            &format!("/api/v1/kbs/{}/sources/{}", fx.kb, fx.ticket),
            Some(json!({ "name": "renamed" })),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["source"]["sync_checkpoint"],
        json!(CHECKPOINT),
        "更新回来的那一格也要带游标"
    );
    Ok(())
}
