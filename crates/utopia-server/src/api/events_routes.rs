//! KB event stream (SSE): live pushes of document ingest/extraction status and changes to the review queue.
//! On receiving an event, the frontend only does a react-query invalidation and refetch. The event carries no business data, so it is idempotent by nature.
//!
//! **Alerts also travel on this stream** (#1028). A browser gives one origin only six HTTP/1.1 connections, and an event stream holds one
//! for as long as it is open. A page with two notification streams, plus one more for an answer being generated, fills all six in two tabs, and Stop cannot be sent.
//! So a page with a knowledge base open subscribes only to this one; the global `/alerts/events` is kept for pages with no KB.

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use std::convert::Infallible;
use tokio::sync::broadcast;
use utopia_core::models::Role;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::state::{AppEvent, AppState};

pub async fn kb_events(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(kb_id): Path<Uuid>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;

    Ok(kb_event_stream(state.events.subscribe(), kb_id))
}

fn kb_event_stream(
    mut rx: broadcast::Receiver<AppEvent>,
    kb_id: Uuid,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(ev) => match relay(&ev, kb_id) {
                    Some(Relay::Alert) => yield Ok(Event::default().event("alert").data("{}")),
                    Some(Relay::Kb) => yield Ok(Event::default()
                        .event(ev.kind)
                        .data(serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into()))),
                    None => continue,
                },
                // The dropped event may be the only notification for this KB or alert; the rest may all have been filtered out.
                // End the response, so EventSource reconnects and catches up through onRecover.
                Err(broadcast::error::RecvError::Lagged(_)) => return,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, PartialEq)]
enum Relay {
    /// Alerts: not filtered by KB, and carry no data; same convention as `alerts_routes::stream`. Whoever receives it
    /// refetches the list, and the list query decides who can see what
    Alert,
    /// This KB's own events, sent through as they are
    Kb,
}

fn relay(ev: &AppEvent, kb_id: Uuid) -> Option<Relay> {
    if ev.kind == "alert" {
        Some(Relay::Alert)
    } else if ev.kb_id == Some(kb_id) {
        Some(Relay::Kb)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::{routing::get, Router};
    use futures_util::{FutureExt, StreamExt};
    use std::time::Duration;

    #[tokio::test]
    async fn lagged_kb_stream_ends_when_its_only_document_or_global_alert_is_lost() {
        let (here, elsewhere) = (Uuid::now_v7(), Uuid::now_v7());
        for (kb_id, kind) in [(Some(here), "document"), (None, "alert")] {
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
                                kb_id,
                                kind,
                                document_id: None,
                            })
                            .unwrap();
                        // Deterministically evict the only valid notification before the first poll; all the rest will be filtered out.
                        for _ in 0..257 {
                            sender
                                .send(AppEvent {
                                    kb_id: Some(elsewhere),
                                    kind: "document",
                                    document_id: None,
                                })
                                .unwrap();
                        }
                        kb_event_stream(receiver, here)
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
            // Keep the sender, so that the real HTTP response ends because of Lagged and not because the channel closed.
            let result = tokio::time::timeout(Duration::from_secs(2), request).await;
            server.abort();
            assert!(result
                .expect("a lagged KB SSE response must end")
                .is_empty());
        }
    }

    #[tokio::test]
    async fn kb_stream_keeps_filtering_and_alert_payloads_until_the_channel_closes() {
        let (here, elsewhere) = (Uuid::now_v7(), Uuid::now_v7());
        let (sender, receiver) = broadcast::channel(256);
        let response = kb_event_stream(receiver, here).into_response();
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let mut body = response.into_body().into_data_stream();
        // The initial connection does not send a refresh event out of nothing, and does not end the response.
        assert!(body.next().now_or_never().is_none());
        sender
            .send(AppEvent {
                kb_id: Some(elsewhere),
                kind: "document",
                document_id: None,
            })
            .unwrap();
        assert!(body.next().now_or_never().is_none());

        let document = AppEvent {
            kb_id: Some(here),
            kind: "document",
            document_id: Some(Uuid::now_v7()),
        };
        sender.send(document.clone()).unwrap();
        let frame = body.next().await.unwrap().unwrap();
        assert_eq!(
            frame.as_ref(),
            format!(
                "event: document\ndata: {}\n\n",
                serde_json::to_string(&document).unwrap()
            )
            .as_bytes()
        );

        for kb_id in [None, Some(here), Some(elsewhere)] {
            sender
                .send(AppEvent {
                    kb_id,
                    kind: "alert",
                    document_id: Some(Uuid::now_v7()),
                })
                .unwrap();
            let frame = body.next().await.unwrap().unwrap();
            assert_eq!(frame.as_ref(), b"event: alert\ndata: {}\n\n");
        }
        assert!(body.next().now_or_never().is_none());
        drop(sender);
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_a_kb_response_releases_its_subscription() {
        let (sender, receiver) = broadcast::channel(256);
        let response = kb_event_stream(receiver, Uuid::now_v7()).into_response();
        assert_eq!(sender.receiver_count(), 1);
        drop(response);
        assert_eq!(sender.receiver_count(), 0);
    }

    #[test]
    fn a_kb_stream_carries_its_own_events_and_every_alert() {
        let (here, elsewhere) = (Uuid::now_v7(), Uuid::now_v7());
        let event = |kb_id, kind| AppEvent {
            kb_id,
            kind,
            document_id: None,
        };
        assert_eq!(relay(&event(Some(here), "document"), here), Some(Relay::Kb));
        assert_eq!(relay(&event(Some(elsewhere), "document"), here), None);
        // System-level alerts have no KB; alerts from other KBs must also wake the badge: send all of them, none carrying data
        for kb_id in [None, Some(here), Some(elsewhere)] {
            assert_eq!(relay(&event(kb_id, "alert"), here), Some(Relay::Alert));
        }
    }
}
