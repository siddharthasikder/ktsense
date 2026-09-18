//! Daemon lifecycle and protocol tests.
//!
//! Every test binds a real socket inside its own `tempdir`, so tests run concurrently and leave
//! nothing behind. The daemon is driven through its public API against in-crate fake engines, so the
//! default suite needs no upstream `kmp-lsp` install. Each test that could hang carries its own
//! deadline, and the socket is removed by the daemon on exit and the directory by the `tempdir`.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ktsense_daemon::{
    probe, run, stop, write_frame, Client, ClientError, DaemonConfig, Engine, EngineRequest,
    HandlerOutcome, Liveness, ServerFrame, StopOutcome, StopReason, PROTOCOL_VERSION,
};

const DEADLINE: Duration = Duration::from_secs(5);
const SHORT_IDLE: Duration = Duration::from_millis(50);

/// A fake warm session: it echoes a per-instance session id and a monotonic served count, so two
/// clients hitting the same instance prove the session was reused rather than rebuilt.
struct EchoEngine {
    session: u64,
    served: Arc<AtomicU64>,
}

impl EchoEngine {
    fn new() -> Self {
        static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
        Self {
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            served: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Engine for EchoEngine {
    async fn handle(&self, request: EngineRequest) -> HandlerOutcome {
        let served = self.served.fetch_add(1, Ordering::Relaxed) + 1;
        HandlerOutcome::Reply(json!({
            "session": self.session,
            "served": served,
            "method": request.method,
        }))
    }

    async fn shutdown(self) {}
}

/// A fake session that has reached a terminal fault: every request faults it.
struct FaultingEngine;

impl Engine for FaultingEngine {
    async fn handle(&self, _request: EngineRequest) -> HandlerOutcome {
        HandlerOutcome::Faulted("engine faulted".to_string())
    }

    async fn shutdown(self) {}
}

struct Daemon {
    _dir: TempDir,
    socket_path: PathBuf,
    handle: JoinHandle<Result<StopReason, ktsense_daemon::DaemonError>>,
}

impl Daemon {
    fn start<E: Engine>(engine: E, idle: Duration) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let config = DaemonConfig::new(socket_path.clone(), idle);
        let handle = tokio::spawn(run(config, engine));
        Self {
            _dir: dir,
            socket_path,
            handle,
        }
    }

    async fn await_live(&self) {
        timeout(DEADLINE, async {
            while probe(&self.socket_path).await != Liveness::Live {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("daemon never became live");
    }

    async fn stop_reason(self) -> StopReason {
        timeout(DEADLINE, self.handle)
            .await
            .expect("daemon did not stop before the deadline")
            .expect("daemon task panicked")
            .expect("daemon returned an error")
    }
}

#[tokio::test]
async fn start_then_hello_then_one_request_then_stop() {
    let daemon = Daemon::start(EchoEngine::new(), DEADLINE);
    daemon.await_live().await;

    let mut client = Client::connect(&daemon.socket_path).await.expect("connect");
    let answer = client
        .request("outline", json!({ "file": "App.kt" }))
        .await
        .expect("request");
    client.stop().await.expect("stop");
    let socket_path = daemon.socket_path.clone();
    let reason = daemon.stop_reason().await;

    let observed = json!({
        "served": answer["served"],
        "method": answer["method"],
        "reason_stopped": reason == StopReason::Stopped,
        "socket_removed": !socket_path.exists(),
    });
    assert_eq!(
        observed,
        json!({ "served": 1, "method": "outline", "reason_stopped": true, "socket_removed": true })
    );
}

#[tokio::test]
async fn a_mismatched_protocol_daemon_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("legacy.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind legacy");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _ = write_frame(
                &mut stream,
                &ServerFrame::Hello {
                    protocol_version: PROTOCOL_VERSION + 1,
                },
            )
            .await;
            tokio::time::sleep(DEADLINE).await;
        }
    });

    let refusal = timeout(DEADLINE, Client::connect(&socket_path))
        .await
        .expect("connect did not resolve");

    let observed = match refusal {
        Err(ClientError::Protocol { expected, actual }) => (expected, actual),
        Err(other) => panic!("expected a protocol mismatch, got error {other:?}"),
        Ok(_live) => panic!("expected a protocol mismatch, got a live connection"),
    };
    assert_eq!(observed, (PROTOCOL_VERSION, PROTOCOL_VERSION + 1));
}

#[tokio::test]
async fn a_stale_socket_is_recovered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("stale.sock");
    drop(UnixListener::bind(&socket_path).expect("bind then abandon"));
    let stale_before = probe(&socket_path).await;

    let config = DaemonConfig::new(socket_path.clone(), DEADLINE);
    let handle = tokio::spawn(run(config, EchoEngine::new()));
    timeout(DEADLINE, async {
        while probe(&socket_path).await != Liveness::Live {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("daemon never rebound the stale socket");

    let mut client = Client::connect(&socket_path).await.expect("connect");
    let answer = client.request("ping", json!({})).await.expect("request");
    client.stop().await.expect("stop");
    let reason = timeout(DEADLINE, handle)
        .await
        .expect("no stop")
        .expect("panic")
        .expect("error");

    let observed = json!({
        "was_stale": stale_before == Liveness::Stale,
        "recovered_served": answer["served"],
        "reason_stopped": reason == StopReason::Stopped,
    });
    assert_eq!(
        observed,
        json!({ "was_stale": true, "recovered_served": 1, "reason_stopped": true })
    );
}

#[tokio::test]
async fn an_idle_daemon_exits_without_a_connection() {
    let daemon = Daemon::start(EchoEngine::new(), SHORT_IDLE);
    let socket_path = daemon.socket_path.clone();
    let reason = daemon.stop_reason().await;

    let observed = json!({
        "reason_idle": reason == StopReason::Idle,
        "socket_removed": !socket_path.exists(),
    });
    assert_eq!(
        observed,
        json!({ "reason_idle": true, "socket_removed": true })
    );
}

#[tokio::test]
async fn two_sequential_clients_reuse_the_one_warm_session() {
    let daemon = Daemon::start(EchoEngine::new(), DEADLINE);
    daemon.await_live().await;

    let mut first = Client::connect(&daemon.socket_path)
        .await
        .expect("connect 1");
    let first_answer = first.request("ping", json!({})).await.expect("request 1");
    drop(first);

    let mut second = Client::connect(&daemon.socket_path)
        .await
        .expect("connect 2");
    let second_answer = second.request("ping", json!({})).await.expect("request 2");
    second.stop().await.expect("stop");
    daemon.stop_reason().await;

    let observed = json!({
        "same_session": first_answer["session"] == second_answer["session"],
        "first_served": first_answer["served"],
        "second_served": second_answer["served"],
    });
    assert_eq!(
        observed,
        json!({ "same_session": true, "first_served": 1, "second_served": 2 })
    );
}

#[tokio::test]
async fn a_faulted_session_stops_the_daemon() {
    let daemon = Daemon::start(FaultingEngine, DEADLINE);
    daemon.await_live().await;

    let mut client = Client::connect(&daemon.socket_path).await.expect("connect");
    let request = client.request("ping", json!({})).await;
    let socket_path = daemon.socket_path.clone();
    let reason = daemon.stop_reason().await;

    let observed = json!({
        "request_reported_error": matches!(request, Err(ClientError::Engine { .. })),
        "reason_faulted": reason == StopReason::EngineFaulted,
        "socket_removed": !socket_path.exists(),
    });
    assert_eq!(
        observed,
        json!({ "request_reported_error": true, "reason_faulted": true, "socket_removed": true })
    );
}

#[tokio::test]
async fn the_socket_and_its_directory_are_owner_only() {
    let daemon = Daemon::start(EchoEngine::new(), DEADLINE);
    daemon.await_live().await;

    let dir: &Path = daemon.socket_path.parent().expect("parent");
    let dir_meta = std::fs::metadata(dir).expect("dir metadata");
    let socket_meta = std::fs::metadata(&daemon.socket_path).expect("socket metadata");

    let observed = (
        dir_meta.permissions().mode() & 0o777,
        socket_meta.permissions().mode() & 0o777,
        socket_meta.uid() == dir_meta.uid(),
    );

    let _ = stop(&daemon.socket_path).await;
    let outcome = timeout(DEADLINE, daemon.handle)
        .await
        .expect("no stop")
        .expect("panic");
    assert!(outcome.is_ok(), "daemon returned an error: {outcome:?}");

    assert_eq!(observed, (0o700, 0o600, true));
}

#[tokio::test]
async fn stopping_an_absent_daemon_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("absent.sock");

    let outcome = timeout(DEADLINE, stop(&socket_path))
        .await
        .expect("stop resolved")
        .expect("stop ok");

    assert_eq!(outcome, StopOutcome::NotRunning);
}
