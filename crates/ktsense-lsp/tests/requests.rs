//! Replay tests for the KT-14 typed request wrappers, driven against the `fake_lsp` binary.
//!
//! Each test scripts the fake to verify the exact params the wrapper puts on the wire and to hand
//! back a canned result, then asserts the decoded, re-serialized value matches that result in a
//! single composed assertion. `FAKE_LSP_SCRIPT` is process-global, so the child is spawned under a
//! shared guard. No test depends on the real `kmp-lsp` binary.

use std::path::Path;
use std::time::Duration;

use ktsense_lsp::{DeclarationScope, FilePosition, InitializeConfig, LspClient};
use serde_json::{json, Value};
use tokio::sync::Mutex;

static SCRIPT_GUARD: Mutex<()> = Mutex::const_new(());

async fn spawn_with_script(script: &Value) -> LspClient {
    let guard = SCRIPT_GUARD.lock().await;
    std::env::set_var("FAKE_LSP_SCRIPT", script.to_string());
    let mut client = LspClient::spawn(Path::new(env!("CARGO_BIN_EXE_fake_lsp")))
        .await
        .expect("spawn fake_lsp");
    drop(guard);
    client.set_request_timeout(Duration::from_secs(5));
    client.initialize(&init_config()).await.expect("initialize");
    client
}

fn init_config() -> InitializeConfig {
    InitializeConfig {
        root_uri: "file:///workspace".to_string(),
        ignore_patterns: vec![],
    }
}

fn at() -> FilePosition {
    FilePosition {
        uri: "file:///workspace/app/User.kt".to_string(),
        line: 3,
        character: 8,
    }
}

fn position_wire() -> Value {
    json!({
        "textDocument": { "uri": "file:///workspace/app/User.kt" },
        "position": { "line": 3, "character": 8 }
    })
}

fn handshake_then(steps: Vec<Value>) -> Value {
    let mut all = vec![
        json!({ "kind": "expect", "method": "initialize", "respond": { "result": {} } }),
        json!({ "kind": "expect", "method": "initialized" }),
    ];
    all.extend(steps);
    all.push(json!({ "kind": "expect", "method": "shutdown", "respond": { "result": null } }));
    all.push(json!({ "kind": "expect", "method": "exit" }));
    json!({ "steps": all })
}

#[tokio::test]
async fn definition_and_implementation_decode_goto_responses() {
    let definition_locations = json!([
        { "uri": "file:///workspace/app/User.kt",
          "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 8 } } }
    ]);
    let implementors = json!([
        { "uri": "file:///workspace/app/UserImpl.kt",
          "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 5, "character": 6 } } }
    ]);
    let script = handshake_then(vec![
        json!({ "kind": "expect", "method": "textDocument/definition", "params": position_wire(),
                "respond": { "result": definition_locations.clone() } }),
        json!({ "kind": "expect", "method": "textDocument/implementation", "params": position_wire(),
                "respond": { "result": implementors.clone() } }),
    ]);
    let mut client = spawn_with_script(&script).await;

    let definition = client.definition(&at()).await.expect("definition");
    let implementation = client.implementation(&at()).await.expect("implementation");
    client.shutdown().await.expect("shutdown");

    let observed = json!({
        "definition": serde_json::to_value(&definition).unwrap(),
        "implementation": serde_json::to_value(&implementation).unwrap(),
    });
    assert_eq!(
        observed,
        json!({ "definition": definition_locations, "implementation": implementors })
    );
}

#[tokio::test]
async fn references_sends_declaration_scope_and_decodes_locations() {
    let locations = json!([
        { "uri": "file:///workspace/app/A.kt",
          "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } } },
        { "uri": "file:///workspace/app/B.kt",
          "range": { "start": { "line": 9, "character": 4 }, "end": { "line": 9, "character": 8 } } }
    ]);
    let expected_params = json!({
        "textDocument": { "uri": "file:///workspace/app/User.kt" },
        "position": { "line": 3, "character": 8 },
        "context": { "includeDeclaration": false }
    });
    let script = handshake_then(vec![json!({
        "kind": "expect", "method": "textDocument/references", "params": expected_params,
        "respond": { "result": locations.clone() }
    })]);
    let mut client = spawn_with_script(&script).await;

    let observed = client
        .references(&at(), DeclarationScope::Excluded)
        .await
        .expect("references");
    client.shutdown().await.expect("shutdown");

    assert_eq!(serde_json::to_value(&observed).unwrap(), locations);
}

#[tokio::test]
async fn references_null_result_becomes_empty_including_declaration() {
    let expected_params = json!({
        "textDocument": { "uri": "file:///workspace/app/User.kt" },
        "position": { "line": 3, "character": 8 },
        "context": { "includeDeclaration": true }
    });
    let script = handshake_then(vec![json!({
        "kind": "expect", "method": "textDocument/references", "params": expected_params,
        "respond": { "result": null }
    })]);
    let mut client = spawn_with_script(&script).await;

    let observed = client
        .references(&at(), DeclarationScope::Included)
        .await
        .expect("references");
    client.shutdown().await.expect("shutdown");

    assert_eq!(serde_json::to_value(&observed).unwrap(), json!([]));
}

#[tokio::test]
async fn document_symbols_decodes_a_nested_outline() {
    let outline = json!([
        { "name": "UserService", "kind": 5,
          "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 10, "character": 1 } },
          "selectionRange": { "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 17 } },
          "children": [
            { "name": "save", "kind": 6,
              "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 3, "character": 3 } },
              "selectionRange": { "start": { "line": 1, "character": 6 }, "end": { "line": 1, "character": 10 } } }
          ] }
    ]);
    let expected_params = json!({ "textDocument": { "uri": "file:///workspace/app/User.kt" } });
    let script = handshake_then(vec![json!({
        "kind": "expect", "method": "textDocument/documentSymbol", "params": expected_params,
        "respond": { "result": outline.clone() }
    })]);
    let mut client = spawn_with_script(&script).await;

    let observed = client
        .document_symbols("file:///workspace/app/User.kt")
        .await
        .expect("documentSymbol");
    client.shutdown().await.expect("shutdown");

    assert_eq!(serde_json::to_value(&observed).unwrap(), outline);
}

#[tokio::test]
async fn hover_decodes_markup_contents() {
    let hover =
        json!({ "contents": { "kind": "markdown", "value": "```kotlin\nfun save()\n```" } });
    let script = handshake_then(vec![json!({
        "kind": "expect", "method": "textDocument/hover", "params": position_wire(),
        "respond": { "result": hover.clone() }
    })]);
    let mut client = spawn_with_script(&script).await;

    let observed = client.hover(&at()).await.expect("hover");
    client.shutdown().await.expect("shutdown");

    assert_eq!(serde_json::to_value(&observed).unwrap(), hover);
}
