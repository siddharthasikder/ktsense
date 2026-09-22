//! Snapshots of `trace` over the `multi-module` fixture, driven by the `fake_lsp` replay binary.
//!
//! The fake plays both roles the command needs: `find` in command mode for name resolution, and a
//! scripted LSP session for the implementation and reference requests. The binary runs from the
//! workspace root with a relative `--root`, so the pinned output carries fixture-relative paths and
//! never this machine's absolute ones, even though the fake's replies, like the engine's, are
//! absolute `file://` URIs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";
const SHORT_INDEX_CAP_MS: &str = "150";

const REPOSITORY: &str = "core/src/main/kotlin/shop/order/OrderRepository.kt";
const JDBC: &str = "db/src/main/kotlin/shop/db/JdbcOrderRepository.kt";
const IN_MEMORY: &str = "db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt";
const CHECKOUT: &str = "app/src/main/kotlin/shop/app/checkout/CheckoutService.kt";
const IMPORTER: &str = "app/src/main/kotlin/shop/app/checkout/OrderImporter.kt";
const BACKFILL: &str = "app/src/main/kotlin/shop/app/reporting/ReportBackfill.kt";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// The fake sits next to the `ktsense` binary under test, both built into the same target dir. A
/// workspace-wide `cargo test` has already built it; a package-scoped run has not, so it is built
/// here on first use rather than letting the suite fail on a missing helper.
fn fake_lsp() -> PathBuf {
    static FAKE: OnceLock<PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let path = Path::new(env!("CARGO_BIN_EXE_ktsense")).with_file_name("fake_lsp");
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
            let status = Command::new(cargo)
                .args(["build", "-p", "ktsense-lsp", "--bin", "fake_lsp"])
                .current_dir(WORKSPACE_ROOT)
                .status()
                .expect("cargo runs");
            assert!(status.success(), "building fake_lsp failed");
        }
        path
    })
    .clone()
}

fn fixture_root() -> PathBuf {
    Path::new(WORKSPACE_ROOT)
        .join(FIXTURE)
        .canonicalize()
        .expect("fixture exists")
}

fn absolute(relative: &str) -> String {
    fixture_root().join(relative).display().to_string()
}

fn uri(relative: &str) -> String {
    format!("file://{}", absolute(relative))
}

fn trace(args: &[&str], find: &Value, script: &Value) -> Run {
    trace_with_find(args, &find.to_string(), script)
}

/// `trace` with the engine's command-mode `find` stdout given verbatim rather than as a JSON value,
/// so a test can hand it the empty stream a cold `--fast` `find` writes when it has no `rg` to run.
fn trace_with_find(args: &[&str], find_stdout: &str, script: &Value) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_LSP_PATH", fake_lsp())
        .env("FAKE_CMD_STDOUT", find_stdout)
        .env("FAKE_LSP_SCRIPT", script.to_string())
        .env("KTSENSE_INDEX_CAP_MS", SHORT_INDEX_CAP_MS)
        .args(["--root", FIXTURE, "trace"])
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// One composed record, so the snapshot proves the exit code and the silent stderr as well as the
/// text.
fn record(run: &Run) -> String {
    format!(
        "exit {}\nstderr {}\n---\n{}",
        run.code.expect("exited normally"),
        if run.stderr.is_empty() {
            "empty"
        } else {
            run.stderr.trim()
        },
        run.stdout
    )
}

fn candidate(relative: &str, line: u32, col: u32) -> Value {
    json!({ "file": absolute(relative), "line": line, "col": col, "name": "save" })
}

fn only_the_interface_method() -> Value {
    json!([candidate(REPOSITORY, 4, 9)])
}

fn every_save() -> Value {
    json!([
        candidate(JDBC, 8, 18),
        candidate(IN_MEMORY, 13, 18),
        candidate(REPOSITORY, 4, 9),
    ])
}

/// An LSP location at a 1-based line and column, the way the fixture is read by a human.
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

fn implementors() -> Value {
    json!([location(JDBC, 8, 18), location(IN_MEMORY, 13, 18)])
}

fn every_reference() -> Value {
    json!([
        location(REPOSITORY, 4, 9),
        location(JDBC, 8, 18),
        location(IN_MEMORY, 13, 18),
        location(CHECKOUT, 13, 29),
        location(IMPORTER, 11, 24),
        location(BACKFILL, 13, 31),
    ])
}

/// The session `trace` drives for the interface method: handshake, the given progress stream, the
/// two requests at the declaration with their params pinned, then teardown.
fn session(progress_stream: Vec<Value>, deeper: Vec<Value>) -> Value {
    resolving_session(progress_stream, Vec::new(), deeper)
}

/// The same session with `lookup` inserted between the index and the positional requests, which is
/// where a fresh trace sends its `workspace/symbol` when command-mode `find` answered with nothing.
fn resolving_session(progress_stream: Vec<Value>, lookup: Vec<Value>, deeper: Vec<Value>) -> Value {
    let mut steps = handshake();
    steps.extend(progress_stream);
    steps.extend(lookup);
    steps.push(json!({
        "kind": "expect", "method": "textDocument/implementation",
        "params": at(REPOSITORY, 4, 9),
        "respond": { "result": implementors() }
    }));
    let mut references = at(REPOSITORY, 4, 9);
    references["context"] = json!({ "includeDeclaration": true });
    steps.push(json!({
        "kind": "expect", "method": "textDocument/references",
        "params": references,
        "respond": { "result": every_reference() }
    }));
    steps.extend(deeper);
    steps.extend(teardown());
    json!({ "steps": steps })
}

/// A session opened, waited on and torn down without being asked anything: what a fresh trace drives
/// when `find` found nothing and the index never completes, so there is no index worth asking.
fn unanswered_session(progress_stream: Vec<Value>) -> Value {
    let mut steps = handshake();
    steps.extend(progress_stream);
    steps.extend(teardown());
    json!({ "steps": steps })
}

fn handshake() -> Vec<Value> {
    vec![
        json!({ "kind": "expect", "method": "initialize", "respond": { "result": {} } }),
        json!({ "kind": "expect", "method": "initialized" }),
    ]
}

fn teardown() -> Vec<Value> {
    vec![
        json!({ "kind": "expect", "method": "shutdown", "respond": { "result": null } }),
        json!({ "kind": "expect", "method": "exit" }),
    ]
}

/// The one `workspace/symbol` exchange a fresh trace makes when `find` reported nothing: the exact
/// name, answered with the interface method's LSP position, whose `character` is a 0-based UTF-16
/// offset the resolver relocates onto the name.
fn indexed_interface_method() -> Vec<Value> {
    vec![json!({
        "kind": "expect", "method": "workspace/symbol",
        "params": { "query": "save" },
        "respond": { "result": [
            { "name": "save", "kind": 6, "location": location(REPOSITORY, 4, 9) }
        ] }
    })]
}

fn completed_index() -> Vec<Value> {
    vec![
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "end" })),
    ]
}

/// An index that begins and never reports an end, so the wait leaves it partial at the cap.
fn unfinished_index() -> Vec<Value> {
    vec![progress(json!({ "kind": "begin", "title": "Indexing" }))]
}

#[test]
fn a_complete_index_yields_the_definition_two_implementors_and_three_callers() {
    let run = trace(
        &["save"],
        &only_the_interface_method(),
        &session(completed_index(), Vec::new()),
    );

    insta::assert_snapshot!(record(&run));
}

#[test]
fn json_carries_the_same_answer_with_the_index_marker_as_data() {
    let run = trace(
        &["save", "--format", "json"],
        &only_the_interface_method(),
        &session(completed_index(), Vec::new()),
    );

    insta::assert_snapshot!(record(&run));
}

#[test]
fn an_index_that_never_finishes_is_answered_within_the_cap_and_marked_partial() {
    let run = trace(
        &["save"],
        &only_the_interface_method(),
        &session(unfinished_index(), Vec::new()),
    );

    insta::assert_snapshot!(record(&run));
}

/// KT-67: an empty command-mode `find` is not proof of absence. On a cold cache the engine takes its
/// `--fast` path, documented as "use rg/fd only", and really does exec `rg`: with no `rg` on `PATH` it
/// searches nothing and writes empty stdout with a silent stderr, which is byte for byte the shape it
/// uses for a name that genuinely matched nothing. A fresh trace therefore asks the session's own
/// index before agreeing, through the same `workspace/symbol` lookup a routed trace asks first, and
/// reports absence only when that index cannot answer either. The third arm is that case: an index
/// still building answers with a subset of the declarations, so it is not asked at all.
#[test]
fn a_name_an_empty_find_missed_is_resolved_from_the_index_unless_it_is_still_building() {
    let confirmed = |find_stdout: &str| {
        trace_with_find(
            &["save"],
            find_stdout,
            &resolving_session(completed_index(), indexed_interface_method(), Vec::new()),
        )
    };
    let silent_engine = confirmed("");
    let empty_array = confirmed("[]");
    let still_building = trace_with_find(&["save"], "", &unanswered_session(unfinished_index()));

    let observed = |run: &Run| {
        (
            run.code,
            run.stdout.contains("## Callers (3)"),
            run.stderr.trim().to_string(),
        )
    };
    assert_eq!(
        (
            observed(&silent_engine),
            observed(&empty_array),
            observed(&still_building),
        ),
        (
            (Some(0), true, String::new()),
            (Some(0), true, String::new()),
            (
                Some(1),
                false,
                "ktsense: no declaration named save".to_string()
            ),
        ),
        "silent engine: {}\nempty array: {}\nstill building: {}",
        record(&silent_engine),
        record(&empty_array),
        record(&still_building),
    );
}

#[test]
fn an_ambiguous_name_lists_every_candidate_and_exits_three_without_a_session() {
    let never_reached = json!({ "steps": [] });
    let run = trace(&["save"], &every_save(), &never_reached);

    insta::assert_snapshot!(record(&run));
}

#[test]
fn a_pick_narrows_an_ambiguous_name_to_one_declaration() {
    let run = trace(
        &["save", "--pick", "shop.order.OrderRepository.save"],
        &every_save(),
        &session(completed_index(), Vec::new()),
    );

    let observed = (
        run.code,
        run.stdout.contains("## Callers (3)"),
        run.stdout.contains("index: complete"),
    );
    assert_eq!(
        observed,
        (Some(0), true, true),
        "stdout was: {}",
        run.stdout
    );
}

/// The index finishes only after the 150 ms cap: by default the answer is taken at the cap and
/// marked partial; `--wait-index` removes the cap and the same session ends complete.
#[test]
fn wait_index_removes_the_cap_so_a_slow_index_ends_complete_instead_of_partial() {
    let slow_index = || {
        vec![
            progress(json!({ "kind": "begin", "title": "Indexing" })),
            json!({ "kind": "delay", "ms": 400 }),
            progress(json!({ "kind": "end" })),
        ]
    };
    let capped = trace(
        &["save"],
        &only_the_interface_method(),
        &session(slow_index(), Vec::new()),
    );
    let uncapped = trace(
        &["save", "--wait-index"],
        &only_the_interface_method(),
        &session(slow_index(), Vec::new()),
    );

    let marker = |run: &Run| {
        run.stdout
            .lines()
            .find(|line| line.starts_with("index: "))
            .map(str::to_string)
    };
    let observed = (
        capped.code,
        marker(&capped),
        uncapped.code,
        marker(&uncapped),
    );
    assert_eq!(
        observed,
        (
            Some(0),
            Some("index: partial".to_string()),
            Some(0),
            Some("index: complete".to_string()),
        ),
        "capped stderr: {} / uncapped stderr: {}",
        capped.stderr,
        uncapped.stderr
    );
}

/// On a cold cache the engine's command-mode `find` takes a text-search path and can report a
/// column inside the keyword before the name (observed on ktor: `val CallLogging` reported at the
/// `v`). Asking for references there returns every use of the keyword, so trace must locate the
/// name on the declaration line itself; the script pins the corrected position.
#[test]
fn a_find_column_inside_the_keyword_is_corrected_to_the_name_before_asking_the_engine() {
    let keyword_column = json!([candidate(REPOSITORY, 4, 5)]);
    let run = trace(
        &["save"],
        &keyword_column,
        &session(completed_index(), Vec::new()),
    );

    let observed = (
        run.code,
        run.stdout.contains("## Callers (3)"),
        run.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (Some(0), true, true),
        "stderr was: {}",
        run.stderr
    );
}

/// Depth 2 asks for the references of each direct caller, in path order, at the position of the
/// caller's own name; only the first is scripted to have a caller of its own.
#[test]
fn depth_two_follows_each_direct_caller_and_reports_who_calls_them() {
    let excluded_context = |mut params: Value| {
        params["context"] = json!({ "includeDeclaration": false });
        params
    };
    let deeper = vec![
        json!({ "kind": "expect", "method": "textDocument/references",
                "params": excluded_context(at(CHECKOUT, 12, 9)),
                "respond": { "result": [location(IMPORTER, 11, 24)] } }),
        json!({ "kind": "expect", "method": "textDocument/references",
                "params": excluded_context(at(IMPORTER, 8, 9)),
                "respond": { "result": [] } }),
        json!({ "kind": "expect", "method": "textDocument/references",
                "params": excluded_context(at(BACKFILL, 9, 9)),
                "respond": { "result": [] } }),
    ];
    let run = trace(
        &["save", "--depth", "2"],
        &only_the_interface_method(),
        &session(completed_index(), deeper),
    );

    let callers_section: String = run
        .stdout
        .lines()
        .skip_while(|line| !line.starts_with("## Callers"))
        .take_while(|line| !line.starts_with("## Usages"))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(format!(
        "exit {}\nstderr {}\n---\n{callers_section}",
        run.code.expect("exited normally"),
        if run.stderr.is_empty() {
            "empty"
        } else {
            run.stderr.trim()
        },
    ));
}

#[cfg(feature = "real-lsp")]
mod real {
    use super::*;

    #[test]
    fn the_real_engine_reports_the_fixture_counts_with_a_complete_index() {
        let output = Command::cargo_bin("ktsense")
            .expect("binary builds")
            .current_dir(WORKSPACE_ROOT)
            .args([
                "--root",
                FIXTURE,
                "trace",
                "save",
                "--pick",
                "shop.order.OrderRepository.save",
            ])
            .output()
            .expect("binary runs");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");

        let observed = (
            output.status.code(),
            stdout.contains("index: complete"),
            stdout.contains("## Implementors (2)"),
            stdout.contains("## Callers (3)"),
            stdout.contains("## Usages (6 sites in 6 files)"),
            output.stderr.is_empty(),
        );
        assert_eq!(
            observed,
            (Some(0), true, true, true, true, true),
            "stdout was: {stdout}"
        );
    }
}
