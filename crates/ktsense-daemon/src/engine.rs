//! The engine port the daemon serves, and the warm `LspClient` adapter behind it.
//!
//! The server core depends only on [`Engine`], so the daemon's transport, lifecycle and idle
//! handling are testable against a hand-built engine with no upstream install. [`WarmEngine`] is the
//! production adapter: it owns one initialized `LspClient` for the daemon's whole life, which is the
//! point of the daemon, and it decides what happens when that client faults.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ktsense_lsp::{IndexPhase, InitializeConfig, LspClient, LspError, Notification};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// One request routed from a connected client to the warm engine.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineRequest {
    pub method: String,
    pub params: Value,
}

/// What the engine produced for a request, and whether the daemon may keep serving afterwards.
#[derive(Debug, Clone, PartialEq)]
pub enum HandlerOutcome {
    /// A successful answer.
    Reply(Value),
    /// The request failed, but the warm session is intact and the daemon keeps serving.
    Error(String),
    /// The warm session reached a terminal fault; the daemon must stop rather than serve a dead
    /// session for the rest of its hour-long life.
    Faulted(String),
}

/// The behaviour the daemon serves. One warm session lives behind an implementation.
pub trait Engine: Send + Sync + 'static {
    fn handle(&self, request: EngineRequest) -> impl Future<Output = HandlerOutcome> + Send;
    fn shutdown(self) -> impl Future<Output = ()> + Send;
}

/// The production engine: one initialized `LspClient` kept warm for the daemon's whole life.
pub struct WarmEngine {
    client: LspClient,
    index: IndexTracker,
}

/// Follows the engine's indexing lifecycle from its progress notifications on a background task,
/// so a status request can read the current phase without anyone having to drain the stream at
/// that moment. Once the stream closes the last observed phase stays readable, and `closed` flips
/// true so a waiter can stop waiting for a `Ready` that can no longer arrive.
#[derive(Clone)]
pub struct IndexTracker {
    phase: Arc<Mutex<IndexPhase>>,
    closed: Arc<AtomicBool>,
}

impl IndexTracker {
    /// Starts following `notifications`. Must be called on a tokio runtime, which is where every
    /// daemon lives. A `None` stream, already taken by someone else, leaves the phase at
    /// [`IndexPhase::Pending`] for good, which is the honest answer when nothing is being observed.
    pub fn spawn(notifications: Option<UnboundedReceiver<Notification>>) -> Self {
        let tracker = Self {
            phase: Arc::new(Mutex::new(IndexPhase::Pending)),
            closed: Arc::new(AtomicBool::new(false)),
        };
        if let Some(stream) = notifications {
            tokio::spawn(tracker.clone().follow(stream));
        }
        tracker
    }

    pub fn phase(&self) -> IndexPhase {
        *self
            .phase
            .lock()
            .expect("index phase lock is never poisoned")
    }

    /// Whether the notification stream has ended. False until the following task observes the stream
    /// close; once true the phase read alongside it is the final observed phase.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    async fn follow(self, mut stream: UnboundedReceiver<Notification>) {
        while let Some(notification) = stream.recv().await {
            let mut phase = self
                .phase
                .lock()
                .expect("index phase lock is never poisoned");
            *phase = phase.observe(&notification);
        }
        self.closed.store(true, Ordering::Release);
    }
}

impl WarmEngine {
    /// Launches the upstream engine and runs the `initialize` handshake, returning an engine ready
    /// to serve. This is the thin call a later CLI `daemon start` wraps around [`crate::run`].
    pub async fn warm_up(config: InitializeConfig) -> Result<Self, LspError> {
        let client = ktsense_lsp::launch().await?;
        client.initialize(&config).await?;
        Ok(Self::from_client(client))
    }

    /// Wraps an already-spawned client, so a session prepared elsewhere can back the daemon.
    pub fn from_client(mut client: LspClient) -> Self {
        let index = IndexTracker::spawn(client.take_notifications());
        Self { client, index }
    }

    /// Where the engine's indexing stands, as last reported through its progress notifications.
    pub fn index_phase(&self) -> IndexPhase {
        self.index.phase()
    }

    /// Whether the engine's progress stream has ended. Once true no further phase change can arrive,
    /// so a waiter watching for `Ready` can stop rather than wait out its cap.
    pub fn index_closed(&self) -> bool {
        self.index.is_closed()
    }

    /// The warm session itself, so CLI orchestration that a warm daemon serves can drive the very
    /// client the daemon keeps initialized instead of launching a second engine child. This is the
    /// narrowest port the layering allows: the daemon crate already owns the client, and high-level
    /// orchestration such as `trace` cannot live here because it needs the syntax crate this one may
    /// not depend on.
    pub fn client(&self) -> &LspClient {
        &self.client
    }
}

impl Engine for WarmEngine {
    async fn handle(&self, request: EngineRequest) -> HandlerOutcome {
        outcome_for(self.client.request(&request.method, request.params).await)
    }

    async fn shutdown(self) {
        let mut client = self.client;
        let _ = client.shutdown().await;
    }
}

/// Maps a raw engine result to a handler outcome. A framing fault is terminal for the client, so it
/// stops the daemon; every other error is reported to the caller while the warm session lives on.
fn outcome_for(result: Result<Value, LspError>) -> HandlerOutcome {
    match result {
        Ok(value) => HandlerOutcome::Reply(value),
        Err(fault @ LspError::Framing(_)) => HandlerOutcome::Faulted(fault.to_string()),
        Err(other) => HandlerOutcome::Error(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktsense_lsp::FramingError;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn progress(kind: &str) -> Notification {
        Notification {
            method: "$/progress".to_string(),
            params: json!({ "token": "index", "value": { "kind": kind, "title": "Indexing" } }),
        }
    }

    async fn settled(tracker: &IndexTracker, expected: IndexPhase) -> IndexPhase {
        for _ in 0..200 {
            if tracker.phase() == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tracker.phase()
    }

    async fn settled_closed(tracker: &IndexTracker) -> bool {
        for _ in 0..200 {
            if tracker.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tracker.is_closed()
    }

    #[tokio::test]
    async fn the_tracker_follows_begin_and_end_and_keeps_the_last_phase_after_the_stream_closes() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let tracker = IndexTracker::spawn(Some(receiver));
        let untracked = IndexTracker::spawn(None);

        let before = tracker.phase();
        sender.send(progress("begin")).expect("open");
        let during = settled(&tracker, IndexPhase::Indexing).await;
        sender.send(progress("end")).expect("open");
        let after = settled(&tracker, IndexPhase::Ready).await;
        drop(sender);
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(
            (before, during, after, tracker.phase(), untracked.phase()),
            (
                IndexPhase::Pending,
                IndexPhase::Indexing,
                IndexPhase::Ready,
                IndexPhase::Ready,
                IndexPhase::Pending
            )
        );
    }

    #[tokio::test]
    async fn the_stream_closing_marks_the_tracker_closed_while_the_last_phase_is_preserved() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let tracker = IndexTracker::spawn(Some(receiver));

        let closed_initially = tracker.is_closed();
        sender.send(progress("begin")).expect("open");
        let phase_indexing = settled(&tracker, IndexPhase::Indexing).await;
        let closed_while_indexing = tracker.is_closed();
        drop(sender);
        let closed_after_drop = settled_closed(&tracker).await;

        assert_eq!(
            (
                closed_initially,
                phase_indexing,
                closed_while_indexing,
                closed_after_drop,
                tracker.phase(),
            ),
            (
                false,
                IndexPhase::Indexing,
                false,
                true,
                IndexPhase::Indexing
            )
        );
    }

    #[test]
    fn only_a_framing_fault_stops_the_daemon() {
        let observed = [
            outcome_for(Ok(Value::Null)),
            outcome_for(Err(LspError::Framing(FramingError::MissingContentLength))),
            outcome_for(Err(LspError::Timeout {
                method: "textDocument/references".to_string(),
                timeout: Duration::from_secs(10),
            })),
        ]
        .map(|outcome| matches!(outcome, HandlerOutcome::Faulted(_)));

        assert_eq!(observed, [false, true, false]);
    }
}
