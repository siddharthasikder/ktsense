//! Snapshots of `context` over the `multi-module` fixture, driven by the `fake_lsp` replay binary.
//!
//! `context` runs the same engine session a depth-1 `trace` runs, so the script here is that
//! session: handshake, an index progress stream, implementations and references at the declaration,
//! teardown. The binary runs from the workspace root with a relative `--root`, so the pinned output
//! carries fixture-relative paths and never this machine's absolute ones.

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

fn context(args: &[&str], find: &Value, script: &Value) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_LSP_PATH", fake_lsp())
        .env("FAKE_CMD_STDOUT", find.to_string())
        .env("FAKE_LSP_SCRIPT", script.to_string())
        .env("KTSENSE_INDEX_CAP_MS", SHORT_INDEX_CAP_MS)
        .args(["--root", FIXTURE, "context"])
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
/// text. A bundle that printed the right thing while reporting failure would still be wrong.
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

/// The session a depth-1 trace drives, which is exactly what `context` needs: handshake, the given
/// progress stream, the two requests at the declaration with their params pinned, then teardown.
fn session(progress_stream: Vec<Value>) -> Value {
    let mut steps = vec![
        json!({ "kind": "expect", "method": "initialize", "respond": { "result": {} } }),
        json!({ "kind": "expect", "method": "initialized" }),
    ];
    steps.extend(progress_stream);
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
    steps.push(json!({ "kind": "expect", "method": "shutdown", "respond": { "result": null } }));
    steps.push(json!({ "kind": "expect", "method": "exit" }));
    json!({ "steps": steps })
}

fn completed_index() -> Vec<Value> {
    vec![
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "end" })),
    ]
}

/// The bound line every bundle prints, which is what the budget claim is read off.
fn bound_line(run: &Run) -> Option<&str> {
    run.stdout.lines().find(|line| line.starts_with("Budget "))
}

#[test]
fn a_generous_budget_carries_the_declaration_its_file_outline_callers_and_implementors() {
    let run = context(
        &["save", "--budget", "2000"],
        &only_the_interface_method(),
        &session(completed_index()),
    );

    insta::assert_snapshot!(record(&run));
}

#[test]
fn json_carries_the_same_bundle_with_the_budget_bound_and_index_marker_as_data() {
    let run = context(
        &["save", "--budget", "2000", "--format", "json"],
        &only_the_interface_method(),
        &session(completed_index()),
    );

    insta::assert_snapshot!(record(&run));
}

/// The priority order under pressure: a budget that fits only the declaration keeps it and reports
/// every lower-priority section as omitted rather than dropping the headings, which would read as
/// a symbol with no callers and no implementors.
#[test]
fn a_budget_that_fits_only_the_declaration_says_what_it_dropped() {
    let run = context(
        &["save", "--budget", "9"],
        &only_the_interface_method(),
        &session(completed_index()),
    );

    insta::assert_snapshot!(record(&run));
}

/// The acceptance criterion the card states outright: the reported content bound never exceeds the
/// budget, at any budget from nothing to generous. Swept rather than sampled so an off-by-one in
/// the packing cannot hide between two chosen budgets.
#[test]
fn no_budget_from_zero_upward_is_ever_exceeded() {
    let bounds: Vec<(usize, Option<String>)> = [0usize, 1, 5, 9, 10, 20, 50, 120, 2000]
        .into_iter()
        .map(|budget| {
            let run = context(
                &["save", "--budget", &budget.to_string()],
                &only_the_interface_method(),
                &session(completed_index()),
            );
            (budget, bound_line(&run).map(str::to_string))
        })
        .collect();

    let breaches: Vec<&(usize, Option<String>)> = bounds
        .iter()
        .filter(|(budget, line)| match line {
            Some(line) => reported_bound(line) > *budget,
            None => true,
        })
        .collect();

    assert_eq!(breaches, Vec::<&(usize, Option<String>)>::new());
}

/// The bound out of `Budget N tokens, content bound M. ...`.
fn reported_bound(line: &str) -> usize {
    line.split("content bound ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .and_then(|bound| bound.trim().parse().ok())
        .unwrap_or(usize::MAX)
}

/// An ambiguous name is a dead end without `--pick`, so `context` inherits the same contract
/// `symbols` and `trace` carry: list every candidate, exit 3, and answer for the one that is picked.
#[test]
fn an_ambiguous_name_exits_three_and_a_pick_narrows_it_to_one_bundle() {
    let never_reached = json!({ "steps": [] });
    let ambiguous = context(&["save"], &every_save(), &never_reached);
    let picked = context(
        &["save", "--pick", "shop.order.OrderRepository.save"],
        &every_save(),
        &session(completed_index()),
    );

    let observed = (
        ambiguous.code,
        ambiguous
            .stdout
            .contains("shop.db.JdbcOrderRepository.save"),
        picked.code,
        picked
            .stdout
            .contains("# Context: shop.order.OrderRepository.save"),
        picked.stdout.contains("## Callers (3)"),
        picked.stdout.contains("## Implementors (2)"),
    );
    assert_eq!(
        observed,
        (Some(3), true, Some(0), true, true, true),
        "ambiguous stdout: {}\npicked stdout: {}",
        ambiguous.stdout,
        picked.stdout
    );
}

/// An answer computed against a still-building index is weaker and must say so, exactly as a trace
/// does: the caller list is a lower bound, not the answer.
#[test]
fn an_index_that_never_finishes_is_answered_within_the_cap_and_marked_partial() {
    let still_indexing = vec![progress(json!({ "kind": "begin", "title": "Indexing" }))];
    let run = context(
        &["save", "--budget", "2000"],
        &only_the_interface_method(),
        &session(still_indexing),
    );

    let observed = (
        run.code,
        run.stdout
            .lines()
            .find(|line| line.starts_with("index: "))
            .map(str::to_string),
    );
    assert_eq!(
        observed,
        (Some(0), Some("index: partial".to_string())),
        "stderr was: {}",
        run.stderr
    );
}

#[test]
fn dot_is_refused_because_only_deps_produces_a_graph() {
    let run = context(
        &["save", "--format", "dot"],
        &only_the_interface_method(),
        &session(completed_index()),
    );

    insta::assert_snapshot!(record(&run));
}
