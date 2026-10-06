//! KT-104: a declaration lookup reads generated Kotlin under `build/generated`, labels it
//! `generated`, and words its not-found answer by whether generated sources exist; `map` keeps its
//! build-free view.
//!
//! The fixture `generated-sources` holds one hand-written `app.Widget` and one generated
//! `app.GeneratedWidget` under `build/generated/ksp`. The engine (`find`) never sees the generated
//! tree, so the exact-name tests drive the `fake_lsp` replay in command mode with an empty answer to
//! stand in for that, which forces the syntax-index fallback the card specifies. `--contains` and
//! `map` need no engine at all.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/generated-sources";
const MULTI_MODULE: &str = "fixtures/multi-module";

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

/// Runs `ktsense` with the fake engine answering `find` with `find_stdout`, so an exact-name lookup
/// takes the syntax-index fallback the card specifies when the engine sees nothing.
fn run_with_empty_engine(args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_LSP_PATH", fake_lsp())
        .env("KTSENSE_NO_AUTOSTART", "1")
        .env("FAKE_CMD_STDOUT", "[]")
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// Runs an engine-free `ktsense` command (`--contains`, `map`).
fn run_engine_free(args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_NO_AUTOSTART", "1")
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// The row the listing prints for the generated declaration, if any: the whole line, so the test
/// reads its kind marker and path together.
fn row_for<'a>(stdout: &'a str, fqn: &str) -> Option<&'a str> {
    stdout.lines().find(|line| line.contains(fqn))
}

/// An exact-name lookup the engine answers with nothing falls back to the syntax index, which reads
/// generated sources: the generated declaration is listed, labelled `(generated)`, from the
/// generated path, under `source: syntax index`, exit 0.
#[test]
fn an_exact_name_the_engine_misses_is_found_in_generated_sources_and_labelled() {
    let run = run_with_empty_engine(&["--root", FIXTURE, "symbols", "GeneratedWidget"]);

    let row = row_for(&run.stdout, "app.GeneratedWidget").map(str::to_string);
    assert_eq!(
        (
            run.code,
            run.stdout.contains("source: syntax index"),
            row.as_deref().map(|row| row.contains("class (generated)")),
            row.as_deref().map(|row| {
                row.contains("build/generated/ksp/main/kotlin/app/GeneratedWidget.kt:4")
            }),
            run.stderr,
        ),
        (Some(0), true, Some(true), Some(true), String::new()),
        "stdout was:\n{}",
        run.stdout
    );
}

/// The same lookup in JSON carries `"generated": true` on the generated declaration, and the
/// `source` is the syntax index.
#[test]
fn the_generated_flag_is_carried_in_json() {
    let run = run_with_empty_engine(&[
        "--format",
        "json",
        "--root",
        FIXTURE,
        "symbols",
        "GeneratedWidget",
    ]);
    let parsed: serde_json::Value = serde_json::from_str(&run.stdout).expect("json stdout");

    assert_eq!(
        (
            run.code,
            parsed["source"].clone(),
            parsed["matches"][0]["fqn"].clone(),
            parsed["matches"][0]["generated"].clone(),
        ),
        (
            Some(0),
            serde_json::json!("syntax index"),
            serde_json::json!("app.GeneratedWidget"),
            serde_json::json!(true),
        ),
        "stdout was:\n{}",
        run.stdout
    );
}

/// `--contains` reads the generated tree too: it lists the hand-written and the generated
/// declaration, and marks only the generated one. The hand-written row carries no `generated`
/// marker, which is what keeps every ordinary row's JSON byte-identical.
#[test]
fn contains_lists_generated_declarations_marked_and_leaves_ordinary_rows_plain() {
    let run = run_engine_free(&["--root", FIXTURE, "symbols", "--contains", "Widget"]);

    let widget = row_for(&run.stdout, "app.Widget ").map(str::to_string);
    let generated = row_for(&run.stdout, "app.GeneratedWidget").map(str::to_string);
    assert_eq!(
        (
            run.code,
            run.stdout.contains("source: syntax index"),
            widget.map(|row| row.contains("(generated)")),
            generated.map(|row| row.contains("(generated)")),
        ),
        (Some(0), true, Some(false), Some(true)),
        "stdout was:\n{}",
        run.stdout
    );
}

/// `map` keeps its build-free view: the generated declaration never appears, while the hand-written
/// file does. `map` and `outline` defaults are unchanged by the generated-source reading.
#[test]
fn map_ignores_generated_sources() {
    let run = run_engine_free(&["--root", FIXTURE, "map", "--budget", "4000"]);

    assert_eq!(
        (
            run.code,
            run.stdout.contains("src/main/kotlin/app/Widget.kt"),
            run.stdout.contains("GeneratedWidget"),
            run.stdout.contains("build/generated"),
        ),
        (Some(0), true, false, false),
        "stdout was:\n{}",
        run.stdout
    );
}

/// The not-found wording turns on whether generated sources exist. In a tree that has them, a name
/// nothing declares says they were searched; in one that has none, it says generated code was looked
/// for and is absent and to run the build.
#[test]
fn the_not_found_wording_reflects_whether_generated_sources_exist() {
    let with_generated = run_with_empty_engine(&["--root", FIXTURE, "symbols", "Nonexistent"]);
    let without_generated =
        run_with_empty_engine(&["--root", MULTI_MODULE, "symbols", "Nonexistent"]);

    assert_eq!(
        (
            with_generated.code,
            with_generated
                .stderr
                .contains("Generated Kotlin sources under build/generated were also searched"),
            without_generated.code,
            without_generated
                .stderr
                .contains("No generated Kotlin sources were found under build/generated")
                && without_generated.stderr.contains("run the build and retry"),
        ),
        (Some(1), true, Some(1), true),
        "with generated stderr: {}\nwithout generated stderr: {}",
        with_generated.stderr,
        without_generated.stderr
    );
}
