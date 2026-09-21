//! KT-34: scripted stdio sessions that drive `initialize`, `tools/list` and a `tools/call` for every
//! tool the server lists, with the results pinned as golden snapshots.
//!
//! The engine is the `fake_lsp` replay binary, so the default suite needs no `kmp-lsp` install. Three
//! sessions rather than one, because the fake is scripted by environment and one script serves one
//! conversation: `find` and `check` both read `FAKE_CMD_STDOUT` and want different shapes in it, and
//! a traced command's LSP session is a different conversation from a warm client's. Every tool is
//! called, and the record below names which session called it.
//!
//! `explain_kotlin_symbol` is covered for real, not as a stub: KT-35 landed the `context` command,
//! so it drives the same depth-1 engine session a trace does and is pinned alongside it. Ambiguity is
//! pinned too, in all three tools that share the resolver: a name that resolves to several
//! declarations is an answer, the candidate list under exit 3, not a failure.

mod session;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use session::{fake_lsp, structured, text, Launch, Session};

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";
const SHORT_INDEX_CAP_MS: &str = "150";

const REPOSITORY: &str = "core/src/main/kotlin/shop/order/OrderRepository.kt";
const JDBC: &str = "db/src/main/kotlin/shop/db/JdbcOrderRepository.kt";
const IN_MEMORY: &str = "db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt";
const CHECKOUT: &str = "app/src/main/kotlin/shop/app/checkout/CheckoutService.kt";
const AUDIT: &str = "app/src/main/kotlin/shop/app/reporting/AuditTrail.kt";

fn workspace() -> PathBuf {
    PathBuf::from(WORKSPACE_ROOT)
}

fn fixture_root() -> PathBuf {
    workspace().join(FIXTURE).canonicalize().expect("fixture")
}

fn absolute(relative: &str) -> String {
    fixture_root().join(relative).display().to_string()
}

fn uri(relative: &str) -> String {
    format!("file://{}", absolute(relative))
}

/// One tool call as the snapshot records it: the envelope, the structured half, then the text.
///
/// Absolute paths are replaced with placeholders, because they carry this machine's checkout
/// location and the run-time socket directory, and a golden file that changes with the working
/// directory pins nothing.
fn record(tool: &str, result: &Value) -> String {
    let structured = serde_json::to_string_pretty(structured(result)).expect("json");
    let body = format!(
        "=== {tool} ===\nisError: {}\nstructuredContent:\n{structured}\n---\n{}",
        result["result"]["isError"],
        text(result)
    );
    stable(&body)
}

/// Replaces everything machine-specific with a placeholder: the fixture root, the workspace, the
/// fake engine's path, and the daemon socket directory.
fn stable(body: &str) -> String {
    let socket = body
        .lines()
        .map(|line| match line.split_once("socket: ") {
            Some((before, _)) => format!("{before}socket: <SOCKET>"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    socket
        .replace(&fixture_root().display().to_string(), "<FIXTURE>")
        .replace(
            &fake_lsp()
                .parent()
                .expect("target dir")
                .display()
                .to_string(),
            "<TARGET>",
        )
        .replace(&workspace().display().to_string(), "<WORKSPACE>")
}

fn candidate(relative: &str, line: u32, col: u32, name: &str) -> Value {
    json!({ "file": absolute(relative), "line": line, "col": col, "name": name })
}

fn location(relative: &str, line: u32, col: u32) -> Value {
    let start = json!({ "line": line - 1, "character": col - 1 });
    json!({ "uri": uri(relative), "range": { "start": start, "end": start } })
}

fn at(relative: &str, line: u32, col: u32) -> Value {
    json!({
        "textDocument": { "uri": uri(relative) },
        "position": { "line": line - 1, "character": col - 1 }
    })
}

fn progress(value: Value) -> Value {
    json!({ "kind": "notify", "method": "$/progress",
            "params": { "token": "indexing", "value": value } })
}

/// The handshake and a completed index, which is all a warm client ever drives.
fn warm_handshake() -> Value {
    json!({ "steps": [
        { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
        { "kind": "expect", "method": "initialized" },
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "end" })),
        { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
        { "kind": "expect", "method": "exit" },
    ]})
}

/// The session a traced command drives, which `context` drives identically because it is a depth-1
/// trace: handshake, a completed index, the two requests at the declaration, then teardown.
fn trace_session() -> Value {
    let mut references = at(REPOSITORY, 3, 11);
    references["context"] = json!({ "includeDeclaration": true });
    json!({ "steps": [
        { "kind": "expect", "method": "initialize", "respond": { "result": {} } },
        { "kind": "expect", "method": "initialized" },
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "end" })),
        { "kind": "expect", "method": "textDocument/implementation",
          "params": at(REPOSITORY, 3, 11),
          "respond": { "result": [location(JDBC, 9, 7), location(IN_MEMORY, 9, 7)] } },
        { "kind": "expect", "method": "textDocument/references",
          "params": references,
          "respond": { "result": [
              location(REPOSITORY, 3, 11),
              location(JDBC, 9, 7),
              location(IN_MEMORY, 9, 7),
              location(CHECKOUT, 8, 34),
          ] } },
        { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
        { "kind": "expect", "method": "exit" },
    ]})
}

fn drive(
    launch: Launch,
    calls: &[(&str, Value)],
) -> (Vec<String>, Vec<String>, Option<i32>, String) {
    let mut server = launch.start(Path::new(WORKSPACE_ROOT));
    server.initialize();
    let listed = server.request(2, "tools/list", json!({}));
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    let records = calls
        .iter()
        .enumerate()
        .map(|(index, (tool, arguments))| {
            let result = server.call(10 + index as u64, tool, arguments.clone());
            record(tool, &result)
        })
        .collect();
    let (exit, stderr) = server.finish();
    (names, records, exit, stderr)
}

/// Pins one session: the catalogue it listed, every call it answered, and how it exited.
fn pin(name: &str, launch: Launch, calls: &[(&str, Value)]) {
    let (names, records, exit, stderr) = drive(launch, calls);
    let report = format!(
        "tools/list: {}\n\n{}\n\nserver exit: {exit:?}\nserver stderr: {}\n",
        names.join(", "),
        records.join("\n\n"),
        if stderr.is_empty() {
            "empty".to_string()
        } else {
            stable(stderr.trim())
        }
    );
    insta::assert_snapshot!(name, report);
}

#[test]
fn a_session_answers_every_tool_that_needs_no_index_while_holding_a_warm_engine() {
    pin(
        "no_index_tools",
        Launch::rooted(FIXTURE)
            .with_engine(fake_lsp())
            .replaying(&warm_handshake().to_string(), SHORT_INDEX_CAP_MS),
        &[
            ("get_kotlin_outline", json!({ "file": REPOSITORY })),
            (
                "get_kotlin_outline",
                json!({ "file": AUDIT, "private": true }),
            ),
            (
                "get_kotlin_outline",
                json!({ "file": "core/src/main/kotlin/shop/order/Nope.kt" }),
            ),
            ("analyze_kotlin_dependencies", json!({})),
            ("analyze_kotlin_dependencies", json!({ "level": "file" })),
            ("get_kotlin_repo_map", json!({ "budget": 400 })),
            ("ktsense_status", json!({})),
        ],
    );
}

#[test]
fn a_session_resolves_a_name_through_the_engine_while_holding_a_warm_one() {
    pin(
        "symbol_lookup",
        Launch::rooted(FIXTURE)
            .with_engine(fake_lsp())
            .answering_commands_with(
                &json!([
                    candidate(REPOSITORY, 4, 9, "save"),
                    candidate(JDBC, 9, 18, "save"),
                ])
                .to_string(),
            )
            .replaying(&warm_handshake().to_string(), SHORT_INDEX_CAP_MS),
        &[
            ("find_kotlin_symbol", json!({ "query": "save" })),
            ("find_kotlin_symbol", json!({ "query": "save", "limit": 1 })),
            (
                "find_kotlin_symbol",
                json!({ "query": "save", "pick": "shop.db.JdbcOrderRepository.save" }),
            ),
            ("find_kotlin_symbol", json!({ "query": "ZzzNope" })),
        ],
    );
}

#[test]
fn a_session_traces_and_explains_a_symbol_and_checks_syntax_through_a_scripted_engine() {
    pin(
        "trace_and_context",
        Launch::rooted(FIXTURE)
            .with_engine(fake_lsp())
            .answering_commands_with(
                &json!([candidate(REPOSITORY, 3, 11, "OrderRepository")]).to_string(),
            )
            .replaying(&trace_session().to_string(), SHORT_INDEX_CAP_MS)
            .without_a_warm_engine(),
        &[
            (
                "trace_kotlin_symbol",
                json!({ "symbol": "OrderRepository" }),
            ),
            (
                "explain_kotlin_symbol",
                json!({ "symbol": "OrderRepository" }),
            ),
            (
                "explain_kotlin_symbol",
                json!({ "symbol": "OrderRepository", "budget": 120 }),
            ),
        ],
    );
    pin(
        "ambiguous_name",
        Launch::rooted(FIXTURE)
            .with_engine(fake_lsp())
            .answering_commands_with(
                &json!([
                    candidate(JDBC, 9, 18, "save"),
                    candidate(IN_MEMORY, 13, 18, "save"),
                ])
                .to_string(),
            )
            .replaying(&json!({ "steps": [] }).to_string(), SHORT_INDEX_CAP_MS)
            .without_a_warm_engine(),
        &[
            ("trace_kotlin_symbol", json!({ "symbol": "save" })),
            ("explain_kotlin_symbol", json!({ "symbol": "save" })),
            ("find_kotlin_symbol", json!({ "query": "save" })),
        ],
    );
    pin(
        "syntax_check",
        Launch::rooted(FIXTURE)
            .with_engine(fake_lsp())
            .answering_commands_with(
                &json!({
                    "errors": [{
                        "file": absolute("app/src/main/kotlin/shop/app/Broken.kt"),
                        "line": 12,
                        "col": 5,
                        "message": "unexpected `fun`"
                    }],
                    "files_ok": 8,
                    "files_with_errors": 1
                })
                .to_string(),
            ),
        &[("check_kotlin_syntax", json!({ "path": "app" }))],
    );
}

/// Every tool in the catalogue is called by one of the sessions above. Asserting it here rather than
/// trusting the reading means a tool added later fails this test until it is covered.
#[test]
fn every_listed_tool_is_covered_by_one_of_the_sessions() {
    let covered = [
        "get_kotlin_outline",
        "analyze_kotlin_dependencies",
        "get_kotlin_repo_map",
        "ktsense_status",
        "explain_kotlin_symbol",
        "find_kotlin_symbol",
        "trace_kotlin_symbol",
        "check_kotlin_syntax",
    ];
    let mut server = Session::rooted(Path::new(WORKSPACE_ROOT), FIXTURE);
    server.initialize();
    let listed = server.request(2, "tools/list", json!({}));
    let mut names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    let (exit, stderr) = server.finish();

    names.sort_unstable();
    let mut expected = covered;
    expected.sort_unstable();
    assert_eq!(
        (names, exit, stderr.is_empty()),
        (expected.to_vec(), Some(0), true)
    );
}
