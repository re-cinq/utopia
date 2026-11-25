//! An answer still being generated can be reattached.
//!
//! **One SSE stream is bound to one HTTP request, but the answer lives longer than the request.** Generation has already moved into
//! its own task (`api::chat`), so refreshing the page no longer loses the answer; but when that stream breaks, it is broken, and
//! after a refresh you can only wait for it to be saved, with the middle part invisible. The front end lifts the in-progress one out of the component,
//! which solves switching between tabs in the same window; **refresh, a new tab or a new device are not covered by that**.
//!
//! This fills the last gap: while generating, register it, so anyone can reattach.
//!
//! **On reattach, first give a snapshot, not a replay of events.** An event stream grows without bound, and buffering it would keep
//! every increment of one conversation in memory; whereas the size of a snapshot is the size of the answer itself,
//! which has a natural ceiling. The client side is simpler too: overwrite the current state with the snapshot, then take increments as usual,
//! with no need to think "which event did I replay up to".
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, watch, RwLock};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// One SSE event: the event name plus the already-serialised data.
///
/// Not `axum::response::sse::Event`: it has no way to read the content back, and here we need both
/// to broadcast it and to update the snapshot with it.
#[derive(Clone, Debug)]
pub struct Frame {
    pub event: &'static str,
    pub data: String,
}

impl Frame {
    pub fn new(event: &'static str, data: String) -> Self {
        Self { event, data }
    }
}

/// What this answer looks like up to this moment. A reattaching viewer gets it first.
#[derive(Clone, Default, Debug)]
pub struct Snapshot {
    pub generation_id: Uuid,
    pub content: String,
    pub steps: Vec<serde_json::Value>,
    pub sources: Vec<serde_json::Value>,
    terminal: Option<Frame>,
}

impl Snapshot {
    /// **The snapshot is derived from the events themselves; no second write path is set up.** Two write paths would sooner or later disagree:
    /// that is exactly the shape this repo keeps tripping over (one place recognises a new field, another does not)
    fn apply(&mut self, f: &Frame) {
        match f.event {
            "delta" => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&f.data) {
                    if let Some(t) = v["text"].as_str() {
                        self.content.push_str(t);
                    }
                }
            }
            "step" => {
                if let Ok(v) = serde_json::from_str(&f.data) {
                    self.steps.push(v);
                }
            }
            // sources are re-sent in full, not appended
            "sources" => {
                if let Ok(serde_json::Value::Array(a)) = serde_json::from_str(&f.data) {
                    self.sources = a;
                }
            }
            _ => {}
        }
    }

    pub(crate) fn terminal(&self) -> Option<Frame> {
        self.terminal.clone()
    }

    pub fn to_frame(&self) -> Frame {
        Frame::new(
            "snapshot",
            json!({
                "generation_id": self.generation_id,
                "content": self.content,
                "steps": self.steps,
                "sources": self.sources,
            })
            .to_string(),
        )
    }
}

struct Entry {
    generation_id: Uuid,
    tx: broadcast::Sender<Frame>,
    snap: Arc<RwLock<Snapshot>>,
    cancellation: Cancellation,
    ready: watch::Sender<bool>,
}

/// Keep the cancel signal: Stop may arrive before the generator starts waiting.
#[derive(Clone)]
pub struct Cancellation(watch::Sender<bool>);

impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl Cancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

/// Register the in-progress generation per session, taking the slot before the question is written. The registry lock does not span an await;
/// a synchronous lock is used, so that the Drop that releases the slot also works when preparation fails.
#[derive(Default)]
pub struct Registry(Mutex<HashMap<Uuid, Entry>>);

/// A handle held during one generation. Emits events, and deregisters when finished.
pub struct Handle {
    conversation_id: Uuid,
    generation_id: Uuid,
    tx: broadcast::Sender<Frame>,
    snap: Arc<RwLock<Snapshot>>,
    cancellation: Cancellation,
    ready: watch::Sender<bool>,
    registry: Arc<Registry>,
}

impl Handle {
    pub fn generation_id(&self) -> Uuid {
        self.generation_id
    }

    pub fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }

    pub async fn snapshot(&self) -> Snapshot {
        self.snap.read().await.clone()
    }

    /// Subscription opens only after preparation succeeds; a rejected retry only releases the slot and leaves no client waiting for a terminal state.
    pub async fn start(&self) {
        self.ready.send_replace(true);
    }

    /// Emit one event: record it into the snapshot, then broadcast.
    ///
    /// **The snapshot's write lock is still held during the broadcast**, and this is required. "Write first, then send" alone
    /// does not prevent duplication: a reattacher who subscribes between the two steps would see this segment both in the snapshot
    /// and receive it again from the broadcast. Sending while holding the lock, with `attach` holding the read lock while subscribing, makes the two mutually exclusive:
    /// so a reattach moment is either wholly before this emit or wholly after it
    pub async fn emit(&self, frame: Frame) {
        let mut snap = self.snap.write().await;
        // A terminal state must also be recorded into the snapshot and obey the subscription boundary:
        // a subscriber attached after the broadcast still needs to receive the same result.
        if snap.terminal.is_some() {
            return;
        }
        snap.apply(&frame);
        if matches!(frame.event, "done" | "error") {
            snap.terminal = Some(frame.clone());
        }
        // No subscribers is the normal case (the person left), not an error
        let _ = self.tx.send(frame);
    }

    /// Once persistence completes, release the session first, then notify subscribers they may ask again;
    /// the snapshot and the subscription still keep the same boundary.
    pub async fn complete(self, terminal: Frame) {
        let mut snap = self.snap.write().await;
        self.retire();
        if snap.terminal.is_some() {
            return;
        }
        snap.terminal = Some(terminal.clone());
        let _ = self.tx.send(terminal);
    }

    fn retire(&self) {
        let mut entries = self.registry.0.lock().expect("live registry lock");
        if entries
            .get(&self.conversation_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.snap, &self.snap))
        {
            entries.remove(&self.conversation_id);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.retire();
    }
}

impl Registry {
    /// The check and the slot-taking happen under the same lock: a new question and a retry both allow only one generator.
    pub async fn begin(self: &Arc<Self>, conversation_id: Uuid) -> AppResult<Handle> {
        let mut entries = self.0.lock().expect("live registry lock");
        if entries.contains_key(&conversation_id) {
            return Err(AppError::CodedConflict {
                code: "answer_running",
                message: "This conversation is still generating an answer.".into(),
            });
        }
        let generation_id = Uuid::now_v7();
        let (tx, _) = broadcast::channel(256);
        let snap = Arc::new(RwLock::new(Snapshot {
            generation_id,
            ..Snapshot::default()
        }));
        let cancellation = Cancellation::default();
        let (ready, _) = watch::channel(false);
        entries.insert(
            conversation_id,
            Entry {
                generation_id,
                tx: tx.clone(),
                snap: snap.clone(),
                cancellation: cancellation.clone(),
                ready: ready.clone(),
            },
        );
        Ok(Handle {
            conversation_id,
            generation_id,
            tx,
            snap,
            cancellation,
            ready,
            registry: self.clone(),
        })
    }

    /// A delayed or duplicate Stop must not cancel the next generation.
    pub async fn stop(&self, conversation_id: Uuid, generation_id: Uuid) {
        let entries = self.0.lock().expect("live registry lock");
        let Some(entry) = entries.get(&conversation_id) else {
            return;
        };
        if entry.generation_id == generation_id {
            entry.cancellation.cancel();
        }
    }

    /// Reattach to a generation that is running: get the snapshot at this moment, plus the increments after it.
    ///
    /// Returns `None` = no generation running for this session. **That is not an error**, and it is the most common case
    pub async fn attach(
        &self,
        conversation_id: Uuid,
    ) -> Option<(Snapshot, broadcast::Receiver<Frame>)> {
        let (snap, tx, mut ready) = {
            let map = self.0.lock().expect("live registry lock");
            let entry = map.get(&conversation_id)?;
            (
                entry.snap.clone(),
                entry.tx.clone(),
                entry.ready.subscribe(),
            )
        };
        // Do not hold the ready sender: when preparation fails and the slot is released, waiters wake up and return to idle.
        // If preparation succeeds, attach to the same round, so that we do not return idle too early and miss the whole answer.
        ready.wait_for(|started| *started).await.ok()?;
        // **Subscribe while holding the snapshot's read lock.** `emit` broadcasts while holding the write lock, so this section
        // is mutually exclusive with any emit: the snapshot obtained and the subscription start line up exactly,
        // and nothing in between is either missed or repeated
        let guard = snap.read().await;
        let rx = tx.subscribe();
        Some((guard.clone(), rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> Frame {
        Frame::new("delta", json!({"text": text}).to_string())
    }

    #[tokio::test]
    async fn retired_handle_cannot_unregister_or_write_into_current_generation() {
        let registry = Arc::new(Registry::default());
        let id = Uuid::now_v7();
        let old = registry.begin(id).await.unwrap();
        old.start().await;
        old.emit(delta("old")).await;
        old.retire();
        let current = registry.begin(id).await.unwrap();
        current.start().await;
        current.emit(delta("new")).await;
        let (snapshot, mut rx) = registry.attach(id).await.unwrap();
        assert_eq!(snapshot.content, "new");
        old.emit(delta("late old text")).await;
        drop(old);
        let (snapshot, _) = registry
            .attach(id)
            .await
            .expect("new generation still running");
        assert_eq!(snapshot.content, "new");
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        current.emit(delta(" answer")).await;
        assert_eq!(rx.recv().await.unwrap().data, delta(" answer").data);
        drop(current);
        assert!(registry.attach(id).await.is_none());
    }

    #[tokio::test]
    async fn only_current_owner_can_remove_entry_in_any_finish_order() {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let registry = Arc::new(Registry::default());
            let id = Uuid::now_v7();
            let other_id = Uuid::now_v7();
            let other = registry.begin(other_id).await.unwrap();
            other.start().await;
            other.emit(delta("unrelated")).await;
            let mut handles = Vec::new();
            for index in 0..3 {
                let handle = registry.begin(id).await.unwrap();
                handle.start().await;
                if index < 2 {
                    handle.retire();
                }
                handles.push(Some(handle));
            }
            let mut current_finished = false;
            for index in order {
                drop(handles[index].take().unwrap());
                current_finished |= index == 2;
                assert_eq!(
                    registry.attach(id).await.is_none(),
                    current_finished,
                    "{order:?}"
                );
                assert_eq!(
                    registry.attach(other_id).await.unwrap().0.content,
                    "unrelated"
                );
            }
            drop(other);
            assert!(registry.attach(other_id).await.is_none());
        }
    }

    #[tokio::test]
    async fn snapshot_and_subscription_partition_concurrent_emission() {
        let registry = Arc::new(Registry::default());
        let id = Uuid::now_v7();
        let handle = registry.begin(id).await.unwrap();
        handle.start().await;
        // Both lock orders are valid: every increment must appear exactly once, either in the snapshot or in the subscription,
        // neither duplicated nor missed.
        for index in 0..64 {
            let attached = if index % 2 == 0 {
                tokio::join!(handle.emit(delta("x")), registry.attach(id)).1
            } else {
                tokio::join!(registry.attach(id), handle.emit(delta("x"))).0
            };
            let (snapshot, mut rx) = attached.unwrap();
            let mut combined = snapshot.content;
            while let Ok(frame) = rx.try_recv() {
                combined.push_str(
                    serde_json::from_str::<serde_json::Value>(&frame.data).unwrap()["text"]
                        .as_str()
                        .unwrap(),
                );
            }
            assert_eq!(combined, registry.attach(id).await.unwrap().0.content);
        }
        drop(handle);
    }

    #[tokio::test]
    async fn a_preparing_reservation_is_exclusive_but_not_subscribable() {
        let registry = Arc::new(Registry::default());
        let id = Uuid::now_v7();
        let handle = registry.begin(id).await.unwrap();
        assert!(matches!(
            registry.begin(id).await,
            Err(AppError::CodedConflict {
                code: "answer_running",
                ..
            })
        ));
        let attached = registry.attach(id);
        tokio::pin!(attached);
        assert!(futures_util::poll!(&mut attached).is_pending());
        drop(handle);
        assert!(attached.await.is_none());
        let _next = registry.begin(id).await.unwrap();
    }

    #[tokio::test]
    async fn an_attachment_waiting_for_preparation_joins_the_started_generation() {
        let registry = Arc::new(Registry::default());
        let id = Uuid::now_v7();
        let handle = registry.begin(id).await.unwrap();
        let attached = registry.attach(id);
        tokio::pin!(attached);
        assert!(futures_util::poll!(&mut attached).is_pending());

        handle.start().await;
        let (snapshot, mut receiver) = attached.await.unwrap();
        assert_eq!(snapshot.generation_id, handle.generation_id());
        handle.emit(delta("answer")).await;
        assert_eq!(receiver.recv().await.unwrap().data, delta("answer").data);
        handle
            .complete(Frame::new("done", json!({"stopped":false}).to_string()))
            .await;
        assert_eq!(receiver.recv().await.unwrap().event, "done");
    }

    #[tokio::test]
    async fn admission_is_exclusive_and_dropping_a_reservation_releases_it() {
        let registry = Arc::new(Registry::default());
        let id = Uuid::now_v7();
        let (first, second) = tokio::join!(registry.begin(id), registry.begin(id));
        assert_ne!(first.is_ok(), second.is_ok());
        let (handle, error) = match (first, second) {
            (Ok(handle), Err(error)) | (Err(error), Ok(handle)) => (handle, error),
            _ => unreachable!(),
        };
        assert!(matches!(
            error,
            AppError::CodedConflict {
                code: "answer_running",
                ..
            }
        ));
        let old_generation = handle.generation_id();
        drop(handle);
        assert!(registry.attach(id).await.is_none());

        let current = registry.begin(id).await.unwrap();
        let cancellation = current.cancellation();
        registry.stop(id, old_generation).await;
        assert!(!cancellation.is_cancelled());
        registry.stop(id, current.generation_id()).await;
        registry.stop(id, current.generation_id()).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), cancellation.cancelled())
            .await
            .expect("Stop is retained even before the first waiter");
    }

    #[tokio::test]
    async fn the_terminal_is_sent_after_the_conversation_is_available() {
        let registry = Arc::new(Registry::default());
        let id = Uuid::now_v7();
        let handle = registry.begin(id).await.unwrap();
        handle.start().await;
        let (_, mut receiver) = registry.attach(id).await.unwrap();
        handle
            .complete(Frame::new("done", json!({"stopped":true}).to_string()))
            .await;
        assert_eq!(receiver.recv().await.unwrap().event, "done");
        let _next = registry
            .begin(id)
            .await
            .expect("a subscriber may immediately follow up");
    }
}
