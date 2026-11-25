//! Alert center (0005). **Cross-library**: a single panel in the top bar, not attached under any one KB.
//!
//! Visibility is not decided here: it is decided once, in the SQL of `utopia_store::alerts`.
//! The route layer only fetches the current user.

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::state::{AppEvent, AppState};

/// How many groups fit in one page. What fits in the popup; more should page, not make people scroll a whole screen.
const PAGE: i64 = 8;
/// The page-size cap: it stops someone from setting limit to 100000 and making the server count the whole table.
const MAX_PAGE: i64 = 50;

#[derive(Deserialize)]
pub struct ListQuery {
    /// Searches library names, object details and kind codes. **It does not find the heading text on screen**:
    /// the wording lives on the client and the server does not have it (see the SEARCH comment in the store)
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

/// A run of connected failures of the same kind. Grouping is **read** out; storage still holds one failure per row.
#[derive(Serialize)]
pub struct GroupView {
    pub kb_id: Option<Uuid>,
    pub kb_name: Option<String>,
    pub kind: String,
    pub severity: String,
    pub count: i64,
    pub unread: i64,
    pub latest_at: DateTime<Utc>,
    /// Together with `latest_at` this delimits the group; it is sent back as is when marking it read.
    pub earliest_at: DateTime<Utc>,
    /// Details, at most a few entries, newest first
    pub lines: Vec<serde_json::Value>,
}

#[derive(Serialize)]
pub struct ListResponse {
    pub items: Vec<GroupView>,
    /// Total number of **groups**: the pager counts groups, not rows
    pub total: i64,
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<ListResponse>> {
    let limit = q.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE);
    let offset = q.offset.unwrap_or(0).max(0);
    let page = utopia_store::alerts::list_groups(&state.pool, &user, q.q.as_deref(), limit, offset)
        .await?;
    Ok(Json(ListResponse {
        items: page
            .items
            .into_iter()
            .map(|g| GroupView {
                kb_id: g.kb_id,
                kb_name: g.kb_name,
                kind: g.kind,
                severity: g.severity,
                count: g.count,
                unread: g.unread,
                latest_at: g.latest_at,
                earliest_at: g.earliest_at,
                lines: g.lines,
            })
            .collect(),
        total: page.total,
    }))
}

/// Badge count. It is its own route rather than counted from the list, because it is needed on every page load.
pub async fn unread(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let n = utopia_store::alerts::unread_count(&state.pool, &user).await?;
    Ok(Json(json!({ "unread": n })))
}

#[derive(Deserialize)]
pub struct ReadGroupBody {
    pub kb_id: Option<Uuid>,
    pub kind: String,
    /// The group's time range, taken as is from the `earliest_at` / `latest_at` the list returns
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// Marks a whole group as read. **Per person**: reading it does not mean the problem is gone, and other people's unread items are unaffected.
///
/// Delimited by time range rather than an id list: a group can have several hundred rows. Visibility is still checked in the store,
/// so guessing a kind cannot clear things you cannot see.
pub async fn mark_group_read(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(b): Json<ReadGroupBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let n =
        utopia_store::alerts::mark_group_read(&state.pool, &user, b.kb_id, &b.kind, b.from, b.to)
            .await?;
    Ok(Json(json!({ "marked": n })))
}

/// Mark everything as read.
pub async fn mark_all_read(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let n = utopia_store::alerts::mark_all_read(&state.pool, &user).await?;
    Ok(Json(json!({ "marked": n })))
}

/// Global event stream. The KB one is `/kbs/{id}/events`, filtered by library; the badge is cross-library,
/// so it cannot be attached there.
///
/// **No permission filtering happens here**: events carry no data, so every recipient refetches the list,
/// and the list query blocks what they should not see. The cost is that people without permission get woken once,
/// in exchange for the push path having no permission logic at all, so "push judges looser than the list" cannot happen.
pub async fn stream(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    Ok(alert_event_stream(state.events.subscribe()))
}

fn alert_event_stream(
    mut rx: broadcast::Receiver<AppEvent>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.kind == "alert" => {
                    yield Ok(Event::default().event("alert").data("{}"));
                }
                Ok(_) => continue,
                // The only alert may already have been pushed out by another kind of notification. End the response so EventSource
                // reconnects and refreshes via onRecover, even if no new alert arrives afterwards.
                Err(broadcast::error::RecvError::Lagged(_)) => return,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::{routing::get, Router};
    use futures_util::{FutureExt, StreamExt};
    use std::time::Duration;

    #[tokio::test]
    async fn lagged_alert_stream_ends_when_non_alert_events_displace_its_only_alert() {
        let (sender, _) = broadcast::channel(256);
        let route_sender = sender.clone();
        let app = Router::new().route(
            "/events",
            get(move || {
                let sender = route_sender.clone();
                async move {
                    let receiver = sender.subscribe();
                    sender
                        .send(AppEvent {
                            kb_id: None,
                            kind: "alert",
                            document_id: None,
                        })
                        .unwrap();
                    for _ in 0..257 {
                        sender
                            .send(AppEvent {
                                kb_id: Some(Uuid::now_v7()),
                                kind: "document",
                                document_id: None,
                            })
                            .unwrap();
                    }
                    alert_event_stream(receiver)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = async {
            let response = client
                .get(format!("http://{address}/events"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.headers()["content-type"], "text/event-stream");
            response.text().await.unwrap()
        };
        // Keep the sender: remaining filtered events must not leave a connection that missed an alert open forever.
        let result = tokio::time::timeout(Duration::from_secs(2), request).await;
        server.abort();
        assert!(result
            .expect("a lagged alert SSE response must end")
            .is_empty());
    }

    #[tokio::test]
    async fn alert_stream_filters_non_alerts_and_never_exposes_alert_details() {
        let (sender, receiver) = broadcast::channel(256);
        let response = alert_event_stream(receiver).into_response();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut body = response.into_body().into_data_stream();
        assert!(body.next().now_or_never().is_none());
        sender
            .send(AppEvent {
                kb_id: Some(Uuid::now_v7()),
                kind: "document",
                document_id: None,
            })
            .unwrap();
        assert!(body.next().now_or_never().is_none());
        sender
            .send(AppEvent {
                kb_id: Some(Uuid::now_v7()),
                kind: "alert",
                document_id: Some(Uuid::now_v7()),
            })
            .unwrap();
        let frame = body.next().await.unwrap().unwrap();
        assert_eq!(frame.as_ref(), b"event: alert\ndata: {}\n\n");
        assert!(body.next().now_or_never().is_none());
        drop(sender);
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_an_alert_response_releases_its_subscription() {
        let (sender, receiver) = broadcast::channel(256);
        let response = alert_event_stream(receiver).into_response();
        assert_eq!(sender.receiver_count(), 1);
        drop(response);
        assert_eq!(sender.receiver_count(), 0);
    }
}
