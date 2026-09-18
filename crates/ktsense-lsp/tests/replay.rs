//! Self-tests for the `fake_lsp` replay helper.
//!
//! A client is driven against the fake by pointing `KTSENSE_LSP_PATH` at `CARGO_BIN_EXE_fake_lsp`
//! and setting `FAKE_LSP_SCRIPT` (inline JSON or a file path) before spawning. These tests stand in
//! for that client: they frame requests onto the child's stdin, read framed replies off its stdout,
//! and assert the scripted exchange, the loud-mismatch behaviour, the initialize/exit hang, and the
//! log-noise hazard.

use std::io::{Cursor, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

fn spawn(script: &str, rust_log: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fake_lsp"));
    command
        .env("FAKE_LSP_SCRIPT", script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match rust_log {
        Some(value) => command.env("RUST_LOG", value),
        None => command.env_remove("RUST_LOG"),
    };
    command.spawn().expect("spawn fake_lsp")
}

fn frame(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).expect("serialize frame");
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(&body);
    framed
}

fn feed(child: &mut Child, messages: &[Value]) {
    let mut stdin = child.stdin.take().expect("child stdin");
    for message in messages {
        stdin.write_all(&frame(message)).expect("write frame");
    }
}

fn read_frame(reader: &mut Cursor<Vec<u8>>) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if std::io::BufRead::read_line(reader, &mut line).ok()? == 0 {
            return None;
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() && content_length.is_some() {
            break;
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let length = content_length?;
    let mut body = vec![0u8; length];
    Read::read_exact(reader, &mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn parse_frames(bytes: Vec<u8>) -> Vec<Value> {
    let mut cursor = Cursor::new(bytes);
    let mut frames = Vec::new();
    while let Some(frame) = read_frame(&mut cursor) {
        frames.push(frame);
    }
    frames
}

fn full_session_script() -> String {
    json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {
                "capabilities": { "referencesProvider": true },
                "serverInfo": { "name": "kmp-lsp", "version": "0.26.0" }
            } } },
            { "kind": "notify", "method": "$/progress",
              "params": { "token": "idx", "value": { "kind": "end" } } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "textDocument/references", "respond": { "result": [] } },
            { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
            { "kind": "expect", "method": "exit" }
        ]
    })
    .to_string()
}

fn client_session() -> Vec<Value> {
    vec![
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "capabilities": {} } }),
        json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "textDocument/references", "params": {} }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
        json!({ "jsonrpc": "2.0", "method": "exit" }),
    ]
}

#[test]
fn replays_a_scripted_session_and_exits_on_exit() {
    let mut child = spawn(&full_session_script(), Some("error"));
    feed(&mut child, &client_session());
    let output = child.wait_with_output().expect("wait for fake_lsp");

    let observed = json!({
        "frames": parse_frames(output.stdout),
        "exited_ok": output.status.success(),
    });
    let expected = json!({
        "frames": [
            { "jsonrpc": "2.0", "id": 1, "result": {
                "capabilities": { "referencesProvider": true },
                "serverInfo": { "name": "kmp-lsp", "version": "0.26.0" } } },
            { "jsonrpc": "2.0", "method": "$/progress",
              "params": { "token": "idx", "value": { "kind": "end" } } },
            { "jsonrpc": "2.0", "id": 2, "result": [] },
            { "jsonrpc": "2.0", "id": 3, "result": null }
        ],
        "exited_ok": true,
    });
    assert_eq!(observed, expected);
}

#[test]
fn reports_a_legible_mismatch_and_fails() {
    let script = json!({
        "steps": [ { "kind": "expect", "method": "initialize" } ]
    })
    .to_string();
    let mut child = spawn(&script, Some("error"));
    feed(
        &mut child,
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "shutdown" })],
    );
    let output = child.wait_with_output().expect("wait for fake_lsp");
    let stderr = String::from_utf8_lossy(&output.stderr);

    let observed = json!({
        "succeeded": output.status.success(),
        "names_expected": stderr.contains("initialize"),
        "names_actual": stderr.contains("shutdown"),
    });
    assert_eq!(
        observed,
        json!({ "succeeded": false, "names_expected": true, "names_actual": true }),
        "stderr was: {stderr}"
    );
}

#[test]
fn refuses_to_exit_when_initialized_was_skipped() {
    let script = json!({
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
            { "kind": "expect", "method": "exit" }
        ]
    })
    .to_string();
    let mut child = spawn(&script, Some("error"));
    feed(
        &mut child,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
            json!({ "jsonrpc": "2.0", "method": "exit" }),
        ],
    );

    std::thread::sleep(Duration::from_millis(500));
    let still_running = child.try_wait().expect("poll fake_lsp").is_none();
    child.kill().expect("kill fake_lsp");
    child.wait().expect("reap fake_lsp");

    assert!(
        still_running,
        "fake exited on `exit` even though `initialized` was never sent"
    );
}

#[test]
fn leaks_log_noise_onto_stdout_only_without_rust_log_error() {
    let script = json!({
        "noisy": true,
        "steps": [
            { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
            { "kind": "expect", "method": "initialized" },
            { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
            { "kind": "expect", "method": "exit" }
        ]
    })
    .to_string();
    let handshake = vec![
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
        json!({ "jsonrpc": "2.0", "method": "initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }),
        json!({ "jsonrpc": "2.0", "method": "exit" }),
    ];

    let mut noisy = spawn(&script, None);
    feed(&mut noisy, &handshake);
    let noisy_out = noisy.wait_with_output().expect("wait noisy");

    let mut quiet = spawn(&script, Some("error"));
    feed(&mut quiet, &handshake);
    let quiet_out = quiet.wait_with_output().expect("wait quiet");

    let marker = "INFO  kmp_lsp";
    let observed = json!({
        "noisy_leaks": String::from_utf8_lossy(&noisy_out.stdout).contains(marker),
        "quiet_leaks": String::from_utf8_lossy(&quiet_out.stdout).contains(marker),
    });
    assert_eq!(
        observed,
        json!({ "noisy_leaks": true, "quiet_leaks": false })
    );
}
