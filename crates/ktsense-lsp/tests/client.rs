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
