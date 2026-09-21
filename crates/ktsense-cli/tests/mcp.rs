//! A scripted MCP stdio session against `ktsense mcp`: initialize, tools/list, one fast tool call
//! and one failing call, as newline-delimited JSON-RPC. This is the KT-31 proof that the transport,
//! the catalogue and the delegation to the binary agree end to end; the per-tool snapshots are
//! KT-34's. Only `outline` is called, which needs no engine, so this runs in the default suite.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";

struct Session {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Session {
    fn start() -> Self {
        let mut child = Command::cargo_bin("ktsense")
            .expect("binary builds")
            .current_dir(WORKSPACE_ROOT)
            .args(["--root", FIXTURE, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("server starts");
        let reader = BufReader::new(child.stdout.take().expect("stdout"));
        Self { child, reader }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{message}").expect("write");
        stdin.flush().expect("flush");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read");
        serde_json::from_str(&line).expect("json line")
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.receive()
    }

    fn finish(mut self) -> (Option<i32>, String) {
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

use std::io::Read;

#[test]
fn a_scripted_session_lists_eight_tools_and_answers_an_outline_call() {
    let mut session = Session::start();

    let init = session.request(
        1,
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" } }),
    );
    session.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let listed = session.request(2, "tools/list", json!({}));
    let outline = session.request(
        3,
        "tools/call",
        json!({ "name": "get_kotlin_outline",
                "arguments": { "file": "core/src/main/kotlin/shop/order/OrderRepository.kt" } }),
    );
    let missing = session.request(
        4,
        "tools/call",
        json!({ "name": "find_kotlin_symbol", "arguments": { "query": "ZzzNope", "root": FIXTURE } }),
    );
    let status = session.request(
        5,
        "tools/call",
        json!({ "name": "ktsense_status", "arguments": {} }),
    );
    let (exit, stderr) = session.finish();

    let tools = listed["result"]["tools"].as_array().expect("tools array");
    let mut names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    names.sort_unstable();
    let every_schema_is_an_object = tools.iter().all(|tool| {
        tool["inputSchema"]["type"] == json!("object")
            && tool["inputSchema"]["properties"].is_object()
    });
    let outline_text = outline["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();

    let observed = (
        init["result"]["serverInfo"]["name"].clone(),
        init["result"]["capabilities"]["tools"].is_object(),
        names,
        every_schema_is_an_object,
        outline["result"]["isError"].clone(),
        outline_text.contains("interface OrderRepository")
            && outline_text.contains("fun save(order: Order): OrderId"),
        missing["result"]["isError"].clone(),
        missing["result"]["content"][0]["text"].clone(),
        status["result"]["isError"].clone(),
        status["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("# Status: ") && text.contains("\ndaemon: ")),
        exit,
        stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (
            json!("ktsense"),
            true,
            vec![
                "analyze_kotlin_dependencies",
                "check_kotlin_syntax",
                "explain_kotlin_symbol",
                "find_kotlin_symbol",
                "get_kotlin_outline",
                "get_kotlin_repo_map",
                "ktsense_status",
                "trace_kotlin_symbol",
            ],
            true,
            json!(false),
            true,
            json!(true),
            json!("ktsense: no declaration named ZzzNope"),
            json!(false),
            true,
            Some(0),
            true,
        ),
        "outline text was: {outline_text}"
    );
}
