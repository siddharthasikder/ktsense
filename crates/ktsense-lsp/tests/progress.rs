//! Pure transition tests for [`IndexPhase`] over the `$/progress` notification stream.
//!
//! These need no child process: a [`Notification`] is a value type, so the state machine is folded
//! over hand-built notifications and the observed phases asserted in one composed value.

use ktsense_lsp::{IndexPhase, Notification};
use serde_json::{json, Value};

fn progress(value: Value) -> Notification {
    Notification {
        method: "$/progress".to_string(),
        params: json!({ "token": "indexing", "value": value }),
    }
}

#[test]
fn progress_stream_drives_pending_through_indexing_to_ready() {
    let stream = [
        Notification {
            method: "window/logMessage".to_string(),
            params: json!({ "message": "starting" }),
        },
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "report", "percentage": 40 })),
        progress(json!({ "kind": "end" })),
    ];

    let mut phase = IndexPhase::default();
    let mut observed = vec![phase];
    for notification in &stream {
        phase = phase.observe(notification);
        observed.push(phase);
    }

    assert_eq!(
        observed,
        vec![
            IndexPhase::Pending,
            IndexPhase::Pending,
            IndexPhase::Indexing,
            IndexPhase::Indexing,
            IndexPhase::Ready,
        ]
    );
}

#[test]
fn malformed_progress_is_ignored_and_a_lone_end_completes() {
    let malformed = Notification {
        method: "$/progress".to_string(),
        params: json!({ "nonsense": true }),
    };
    let end = progress(json!({ "kind": "end" }));

    let observed = (
        IndexPhase::Indexing.observe(&malformed),
        IndexPhase::Pending.observe(&end),
    );

    assert_eq!(observed, (IndexPhase::Indexing, IndexPhase::Ready));
}

/// Process-level tests for the Ready-or-cap waiter, driving the `fake_lsp` replay binary.
mod waiter {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use ktsense_lsp::{wait_for_index, IndexPhase, InitializeConfig, LspClient};
    use serde_json::{json, Value};
    use tokio::sync::Mutex;

    static SCRIPT_GUARD: Mutex<()> = Mutex::const_new(());

    const CAP: Duration = Duration::from_millis(200);
    /// One scheduling tick past the cap, the slack the acceptance allows.
    const CAP_PLUS_TICK: Duration = Duration::from_millis(300);

    async fn spawn_with_script(script: &Value) -> LspClient {
        let guard = SCRIPT_GUARD.lock().await;
        std::env::set_var("FAKE_LSP_SCRIPT", script.to_string());
        let client = LspClient::spawn(Path::new(env!("CARGO_BIN_EXE_fake_lsp")))
            .await
            .expect("spawn fake_lsp");
        drop(guard);
        client
    }

    fn init_config() -> InitializeConfig {
        InitializeConfig {
            root_uri: "file:///workspace".to_string(),
            ignore_patterns: Vec::new(),
        }
    }

    fn session(progress: Vec<Value>) -> Value {
        let mut steps = vec![
            json!({ "kind": "expect", "method": "initialize", "respond": { "result": {} } }),
            json!({ "kind": "expect", "method": "initialized" }),
        ];
        steps.extend(progress);
        steps
            .push(json!({ "kind": "expect", "method": "shutdown", "respond": { "result": null } }));
        steps.push(json!({ "kind": "expect", "method": "exit" }));
        json!({ "steps": steps })
    }

    fn begin() -> Value {
        notify(json!({ "kind": "begin", "title": "Indexing" }))
    }

    fn end() -> Value {
        notify(json!({ "kind": "end" }))
    }

    fn notify(value: Value) -> Value {
        json!({ "kind": "notify", "method": "$/progress",
                "params": { "token": "indexing", "value": value } })
    }

    #[tokio::test]
    async fn returns_ready_as_soon_as_the_index_reports_finishing() {
        let script = session(vec![begin(), end()]);
        let mut client = spawn_with_script(&script).await;
        client.initialize(&init_config()).await.expect("initialize");

        let started = Instant::now();
        let outcome = wait_for_index(&mut client, CAP).await;
        let elapsed = started.elapsed();
        client.shutdown().await.expect("shutdown");

        assert_eq!(
            (outcome.phase, elapsed < CAP),
            (IndexPhase::Ready, true),
            "waited {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn returns_the_phase_reached_within_the_cap_when_the_index_never_finishes() {
        let script = session(vec![begin()]);
        let mut client = spawn_with_script(&script).await;
        client.initialize(&init_config()).await.expect("initialize");

        let started = Instant::now();
        let outcome = wait_for_index(&mut client, CAP).await;
        let elapsed = started.elapsed();
        client.shutdown().await.expect("shutdown");

        assert_eq!(
            (outcome.phase, elapsed >= CAP, elapsed < CAP_PLUS_TICK),
            (IndexPhase::Indexing, true, true),
            "waited {elapsed:?}"
        );
    }
}
