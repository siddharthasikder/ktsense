//! A scripted stdio session that drives `ktsense mcp`, shared by the roots test and the per-tool
//! session snapshots.
//!
//! The transport is newline-delimited JSON-RPC, which is what `rmcp`'s stdio transport speaks. Two
//! things make this more than a request/response loop. The server asks the client questions of its
//! own, so `roots/list` arrives as a request interleaved with our responses and has to be answered
//! before the response we were waiting for turns up. And the engine only ever stops when its stdin
//! reaches EOF (see AGENTS.md), so finishing a session means dropping stdin and then waiting.
//!
//! The binary under test is located with `assert_cmd`, which resolves it out of the target directory
//! rather than from a `CARGO_BIN_EXE_` variable, because `ktsense` is a target of a different crate.
//! `cargo test --workspace` builds every target before running any test, so it is there; a bare
//! `cargo test -p ktsense-mcp` may not have built it, and the panic below says so.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

/// A running `ktsense mcp` server and the client side of its stdio.
pub struct Session {
    child: Child,
    reader: BufReader<ChildStdout>,
    /// Filesystem paths to answer `roots/list` with, as the client's advertised roots.
    roots: Vec<String>,
    /// Every `roots/list` request the server made, so a test can assert it asked exactly once.
    roots_requests: usize,
}

impl Session {
    /// Starts a server rooted at `root`, from `directory` as its working directory.
    pub fn rooted(directory: &Path, root: &str) -> Self {
        let child = Command::cargo_bin("ktsense")
            .expect("the ktsense binary must be built; run `cargo test --workspace`")
            .current_dir(directory)
            .args(["--root", root, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("server starts");
        Self::around(child)
    }

    fn around(mut child: Child) -> Self {
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            reader,
            roots: Vec::new(),
            roots_requests: 0,
        }
    }

    /// Advertises these filesystem paths whenever the server asks for roots.
    pub fn advertising(mut self, roots: &[&Path]) -> Self {
        self.roots = roots
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        self
    }

    /// Runs the handshake, declaring the roots capability only when roots were advertised, so a
    /// session without them exercises the path where the server must not ask.
    pub fn initialize(&mut self) -> Value {
        let capabilities = if self.roots.is_empty() {
            json!({})
        } else {
            json!({ "roots": { "listChanged": false } })
        };
        let initialized = self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": { "name": "ktsense-session-test", "version": "0" }
            }),
        );
        self.notify("notifications/initialized", json!({}));
        initialized
    }

    pub fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
    }

    /// Sends a request and returns its response, answering anything the server asks along the way.
    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self.receive();
            match message.get("method").and_then(Value::as_str) {
                Some(asked) => self.answer(asked, &message),
                None => return message,
            }
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    pub fn roots_requests(&self) -> usize {
        self.roots_requests
    }

    /// Answers a request the server made of us. `roots/list` is the only one it makes; anything else
    /// is an error reply rather than silence, so a new server-to-client request shows up as a test
    /// failure instead of a hang.
    fn answer(&mut self, method: &str, message: &Value) {
        let Some(id) = message.get("id").cloned() else {
            return;
        };
        if method == "roots/list" {
            self.roots_requests += 1;
            let roots: Vec<Value> = self
                .roots
                .iter()
                .map(|root| json!({ "uri": format!("file://{root}") }))
                .collect();
            self.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "roots": roots } }));
            return;
        }
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("this client does not implement {method}") }
        }));
    }

    fn send(&mut self, message: Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).expect("read");
        assert!(read > 0, "the server closed stdout before answering");
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("not a JSON-RPC line: {error}\n{line}"))
    }

    /// Closes stdin, which is what ends the server, and reports how it went.
    pub fn finish(mut self) -> (Option<i32>, String) {
        drop(self.child.stdin.take());
        let status = self.child.wait().expect("server exits");
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        (status.code(), stderr)
    }
}

/// The text content of a tool result.
pub fn text(result: &Value) -> &str {
    result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

/// The structured half of a tool result.
pub fn structured(result: &Value) -> &Value {
    &result["result"]["structuredContent"]
}
