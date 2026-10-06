//! `trace` and `context` over the `annotations` fixture, driven by the `fake_lsp` replay binary
//! (KT-109).
//!
//! The fake plays `find` in command mode for name resolution and a scripted LSP session for the
//! implementation and reference requests; the annotation use sites themselves come from a
//! tree-sitter pass over the fixture files, independent of the engine, so the fake need only resolve
//! the name and answer the two positional requests. The binary runs from the workspace root with a
//! relative `--root`, so the pinned output carries fixture-relative paths.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/annotations";
const AUDITED: &str = "src/main/kotlin/app/Audited.kt";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

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

fn run(args: &[&str], find_stdout: &str, script: &Value) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_LSP_PATH", fake_lsp())
        .env("KTSENSE_NO_AUTOSTART", "1")
        .env("FAKE_CMD_STDOUT", find_stdout)
        .env("FAKE_LSP_SCRIPT", script.to_string())
        .env("KTSENSE_INDEX_CAP_MS", "150")
        .args(["--root", FIXTURE])
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

fn at(relative: &str, line: u32, col: u32) -> Value {
    json!({
        "textDocument": { "uri": uri(relative) },
        "position": { "line": line - 1, "character": col - 1 }
    })
}

fn completed_index() -> Vec<Value> {
    vec![
        json!({ "kind": "notify", "method": "$/progress",
                "params": { "token": "indexing", "value": { "kind": "begin", "title": "Indexing" } } }),
        json!({ "kind": "notify", "method": "$/progress",
                "params": { "token": "indexing", "value": { "kind": "end" } } }),
    ]
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

/// The session a resolved `trace Audited` drives: the handshake, a complete index, the two
/// positional requests at the annotation class declaration answered with nothing, and teardown. The
/// annotation use sites come from the file scan, not from these replies.
fn annotation_session() -> Value {
    let mut steps = handshake();
    steps.extend(completed_index());
    steps.push(json!({
        "kind": "expect", "method": "textDocument/implementation",
        "params": at(AUDITED, 3, 18),
        "respond": { "result": [] }
    }));
    let mut references = at(AUDITED, 3, 18);
    references["context"] = json!({ "includeDeclaration": true });
    steps.push(json!({
        "kind": "expect", "method": "textDocument/references",
        "params": references,
        "respond": { "result": [] }
    }));
    steps.extend(teardown());
    json!({ "steps": steps })
}

/// A fresh session whose completed index answers the resolving `workspace/symbol` with nothing, so
/// the name is confirmed absent and the answer lists its annotation use sites and text references.
fn not_found_session(symbol: &str) -> Value {
    let mut steps = handshake();
    steps.extend(completed_index());
    steps.push(json!({
        "kind": "expect", "method": "workspace/symbol",
        "params": { "query": symbol },
        "respond": { "result": [] }
    }));
    steps.extend(teardown());
    json!({ "steps": steps })
}

fn audited_candidate() -> String {
    json!([{ "file": absolute(AUDITED), "line": 3, "col": 18, "name": "Audited" }]).to_string()
}

/// KT-109: a name that resolves to an annotation class gains `## Annotated (N)`, listing each
/// declaration the annotation is written on by FQN, kind and file:line, grouped by file, with the
/// by-name precision stated. Exit stays 0.
#[test]
fn a_resolved_annotation_class_lists_the_declarations_it_is_written_on() {
    let run = run(
        &["trace", "Audited"],
        &audited_candidate(),
        &annotation_session(),
    );

    let observed = (
        run.code,
        run.stdout.contains("## Annotated (3)"),
        run.stdout.contains("precision: syntax (matched by name)"),
        run.stdout.contains("- app.SalesReport  class  4"),
        run.stdout.contains("- app.AuditReport  class  8"),
        run.stdout.contains("- app.handlers.runTask  fun  6"),
        run.stdout.find("## Annotated") < run.stdout.find("app/handlers/Tasks.kt"),
        run.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (Some(0), true, true, true, true, true, true, true),
        "stdout was:\n{}",
        run.stdout
    );
}

/// The same resolved answer in JSON carries `annotated` with the by-name precision and the three
/// declarations, and exit stays 0.
#[test]
fn the_resolved_annotation_use_sites_are_carried_in_json() {
    let run = run(
        &["trace", "Audited", "--format", "json"],
        &audited_candidate(),
        &annotation_session(),
    );
    let parsed: Value = serde_json::from_str(&run.stdout).expect("json stdout");

    let observed = (
        run.code,
        parsed["annotated"]["total"].clone(),
        parsed["annotated"]["precision"].clone(),
        parsed["annotated"]["file_count"].clone(),
    );
    assert_eq!(
        observed,
        (
            Some(0),
            json!(3),
            json!("syntax (matched by name)"),
            json!(2),
        ),
        "stdout was:\n{}",
        run.stdout
    );
}

/// KT-109 not-found case: a name nothing declares that is used as `@Name` lists `## Annotated (N)`
/// between the KT-87/KT-104 not-found wording and the KT-94 `## Text references` section, with the
/// by-name precision. Exit stays 1.
#[test]
fn an_undeclared_annotation_lists_its_annotated_declarations_above_text_references() {
    let run = run(
        &["trace", "RequestRouter"],
        "",
        &not_found_session("RequestRouter"),
    );

    let not_found_at = run.stdout.find("no declaration named RequestRouter");
    let annotated_at = run.stdout.find("## Annotated (1)");
    let text_refs_at = run.stdout.find("## Text references");
    let observed = (
        run.code,
        run.stdout.contains("precision: syntax (matched by name)"),
        run.stdout.contains("- app.AuditReport  class  8"),
        not_found_at < annotated_at && annotated_at < text_refs_at,
        run.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (Some(1), true, true, true, true),
        "stdout was:\n{}",
        run.stdout
    );
}

/// A name nothing declares and that annotates nothing keeps the plain KT-94 not-found answer: no
/// `## Annotated` section, exit 1, so an undeclared name that is not an annotation is unchanged.
#[test]
fn an_undeclared_non_annotation_name_has_no_annotated_section() {
    let run = run(
        &["trace", "Nonexistent"],
        "",
        &not_found_session("Nonexistent"),
    );

    let observed = (
        run.code,
        run.stdout.contains("## Annotated"),
        run.stdout.contains("## Text references"),
    );
    assert_eq!(
        observed,
        (Some(1), false, true),
        "stdout was:\n{}",
        run.stdout
    );
}

/// `context` gains the same section after its callers when the symbol resolves to an annotation
/// class, and leaves it out otherwise. Here the budget is generous, so all three are listed.
#[test]
fn context_lists_the_annotation_use_sites_after_callers() {
    let run = run(
        &["context", "Audited", "--budget", "4000"],
        &audited_candidate(),
        &annotation_session(),
    );

    let observed = (
        run.code,
        run.stdout.contains("## Annotated (3)"),
        run.stdout
            .contains("- app.SalesReport  class  src/main/kotlin/app/Reports.kt:4"),
        run.stdout.find("## Callers") < run.stdout.find("## Annotated"),
        run.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (Some(0), true, true, true, true),
        "stdout was:\n{}",
        run.stdout
    );
}

#[cfg(feature = "real-lsp")]
mod real {
    use super::*;

    fn real(args: &[&str]) -> Run {
        let output = Command::cargo_bin("ktsense")
            .expect("binary builds")
            .current_dir(WORKSPACE_ROOT)
            .env("KTSENSE_NO_AUTOSTART", "1")
            .args(["--root", FIXTURE])
            .args(args)
            .output()
            .expect("binary runs");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }

    /// Against the real engine: a resolved annotation class lists its three use sites and exits 0,
    /// an undeclared annotation lists its one use site above the text references and exits 1. Needs
    /// `rg` for the reference and text scans.
    #[test]
    fn the_real_engine_lists_annotation_use_sites_resolved_and_not_found() {
        let resolved = real(&["trace", "Audited"]);
        let not_found = real(&["trace", "RequestRouter"]);

        let observed = (
            resolved.code,
            resolved.stdout.contains("## Annotated (3)"),
            resolved.stdout.contains("- app.handlers.runTask  fun  6"),
            not_found.code,
            not_found.stdout.contains("## Annotated (1)"),
            not_found.stdout.find("## Annotated") < not_found.stdout.find("## Text references"),
        );
        assert_eq!(
            observed,
            (Some(0), true, true, Some(1), true, true),
            "resolved:\n{}\nnot found:\n{}",
            resolved.stdout,
            not_found.stdout
        );
    }
}
