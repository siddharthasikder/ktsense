//! Process-level tests for [`LspClient`] driving the KT-15 `fake_lsp` replay binary.
//!
//! The fake reads its scenario from `FAKE_LSP_SCRIPT` in its inherited environment. Since that
//! variable is process-global, each test sets it under a shared guard and spawns the child while
//! holding the guard; the child copies the environment at spawn time, so the value cannot race
//! another test once spawning has returned. No test depends on the real `kmp-lsp` binary.

use std::path::Path;
use std::time::{Duration, Instant};

use ktsense_lsp::{InitializeConfig, LspClient, LspError, Teardown};
use serde_json::{json, Value};
use tokio::sync::Mutex;

static SCRIPT_GUARD: Mutex<()> = Mutex::const_new(());

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
        ignore_patterns: vec!["**/build/**".to_string()],
    }
}

#[tokio::test]
async fn initializes_receives_a_notification_and_shuts_down_cleanly() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {
                "capabilities": { "referencesProvider": true }
            } } },
            { "kind": "notify", "method": "$/progress",
              "params": { "token": "indexing", "value": { "kind": "end" } } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "textDocument/references", "respond": { "result": [] } },
            { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
            { "kind": "expect", "method": "exit" }
        ]
    });
    let mut client = spawn_with_script(&script).await;

    let capabilities = client.initialize(&init_config()).await.expect("initialize");
    let references = client
        .request("textDocument/references", json!({}))
        .await
        .expect("references");
    let progress = client.next_notification().await;
    let teardown = client.shutdown().await.expect("shutdown");

    let observed = json!({
        "capabilities": capabilities,
        "references": references,
        "progress": progress.map(|n| json!({ "method": n.method, "params": n.params })),
        "teardown_exited": teardown == Teardown::Exited,
    });
    let expected = json!({
        "capabilities": { "capabilities": { "referencesProvider": true } },
        "references": [],
        "progress": { "method": "$/progress",
                      "params": { "token": "indexing", "value": { "kind": "end" } } },
        "teardown_exited": true,
    });
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn kills_the_child_when_it_ignores_exit() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
            { "kind": "delay", "ms": 10000 }
        ]
    });
    let mut client = spawn_with_script(&script).await;
    client.set_exit_grace(Duration::from_millis(300));
    client.initialize(&init_config()).await.expect("initialize");

    let started = Instant::now();
    let teardown = client.shutdown().await.expect("shutdown");
    let elapsed = started.elapsed();

    let observed = json!({
        "teardown_killed": teardown == Teardown::Killed,
        "returned_before_hang": elapsed < Duration::from_secs(5),
    });
    assert_eq!(
        observed,
        json!({ "teardown_killed": true, "returned_before_hang": true }),
        "shutdown took {elapsed:?}"
    );
}

#[tokio::test]
async fn request_without_a_reply_returns_a_typed_timeout() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "textDocument/references" },
            { "kind": "delay", "ms": 3000 }
        ]
    });
    let mut client = spawn_with_script(&script).await;
    client.set_request_timeout(Duration::from_millis(300));
    client.initialize(&init_config()).await.expect("initialize");

    let started = Instant::now();
    let outcome = client.request("textDocument/references", json!({})).await;
    let elapsed = started.elapsed();

    let observed = json!({
        "is_timeout": matches!(&outcome, Err(LspError::Timeout { method, .. }) if method == "textDocument/references"),
        "returned_before_hang": elapsed < Duration::from_secs(5),
    });
    assert_eq!(
        observed,
        json!({ "is_timeout": true, "returned_before_hang": true }),
        "outcome was {outcome:?} after {elapsed:?}"
    );
}

#[tokio::test]
async fn request_surfaces_a_jsonrpc_error_response() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "textDocument/references",
              "respond": { "error": { "code": -32601, "message": "method not found" } } },
            { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
            { "kind": "expect", "method": "exit" }
        ]
    });
    let mut client = spawn_with_script(&script).await;
    client.initialize(&init_config()).await.expect("initialize");

    let outcome = client.request("textDocument/references", json!({})).await;
    let teardown = client.shutdown().await.expect("shutdown");

    let observed = json!({
        "is_response_error": matches!(&outcome, Err(LspError::Response { method, code, message })
            if method == "textDocument/references" && *code == -32601 && message == "method not found"),
        "teardown_exited": teardown == Teardown::Exited,
    });
    assert_eq!(
        observed,
        json!({ "is_response_error": true, "teardown_exited": true }),
        "outcome was {outcome:?}"
    );
}

#[tokio::test]
async fn request_reports_child_exit_when_the_stream_closes_mid_request() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "textDocument/references" }
        ]
    });
    let mut client = spawn_with_script(&script).await;
    client.set_request_timeout(Duration::from_secs(5));
    client.initialize(&init_config()).await.expect("initialize");

    let started = Instant::now();
    let outcome = client.request("textDocument/references", json!({})).await;
    let elapsed = started.elapsed();

    let observed = json!({
        "is_child_exited": matches!(&outcome, Err(LspError::ChildExited { method }) if method == "textDocument/references"),
        "returned_before_timeout": elapsed < Duration::from_secs(4),
    });
    assert_eq!(
        observed,
        json!({ "is_child_exited": true, "returned_before_timeout": true }),
        "outcome was {outcome:?} after {elapsed:?}"
    );
}

#[tokio::test]
async fn malformed_frame_faults_the_client_and_fails_later_requests_promptly() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "raw", "bytes": "garbage-not-a-frame\r\n\r\n" },
            { "kind": "delay", "ms": 3000 }
        ]
    });
    let mut client = spawn_with_script(&script).await;
    client.set_request_timeout(Duration::from_millis(500));
    client.initialize(&init_config()).await.expect("initialize");

    let stream_after_fault = client.next_notification().await;
    let started = Instant::now();
    let outcome = client.request("textDocument/references", json!({})).await;
    let elapsed = started.elapsed();

    let observed = json!({
        "stream_closed": stream_after_fault.is_none(),
        "is_framing_fault": matches!(&outcome, Err(LspError::Framing(_))),
        "returned_promptly": elapsed < Duration::from_millis(400),
    });
    assert_eq!(
        observed,
        json!({ "stream_closed": true, "is_framing_fault": true, "returned_promptly": true }),
        "outcome was {outcome:?} after {elapsed:?}"
    );
}

#[tokio::test]
async fn shutdown_of_a_wedged_child_does_not_wait_out_the_request_timeout() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "shutdown" },
            { "kind": "delay", "ms": 10000 }
        ]
    });
    let mut client = spawn_with_script(&script).await;
    client.set_request_timeout(Duration::from_secs(10));
    client.set_shutdown_timeout(Duration::from_millis(300));
    client.set_exit_grace(Duration::from_millis(300));
    client.initialize(&init_config()).await.expect("initialize");

    let started = Instant::now();
    let teardown = client.shutdown().await.expect("shutdown");
    let elapsed = started.elapsed();

    let observed = json!({
        "teardown_killed": teardown == Teardown::Killed,
        "faster_than_request_timeout": elapsed < Duration::from_secs(2),
    });
    assert_eq!(
        observed,
        json!({ "teardown_killed": true, "faster_than_request_timeout": true }),
        "shutdown took {elapsed:?}"
    );
}
