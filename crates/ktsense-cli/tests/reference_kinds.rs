//! KT-83 against the real engine: `trace` over the `reference-kinds` fixture keeps comment, KDoc and
//! string text, and a same-named `companion object`, out of the callers, and accounts for every site
//! it left out. Gated behind `real-lsp` so the default install-free suite skips it.
//!
//! `kmp-lsp` 0.26.0 answers `textDocument/references` with a whole-word text search, so the seven
//! `Sprocket` sites in the fixture include a KDoc link, a line comment, a string literal and a
//! same-named companion object; only the two code sites, both inside `SprocketUser.assemble`, are a
//! caller. `--pick` selects the class over the companion so resolution is not ambiguous.

#![cfg(feature = "real-lsp")]

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/reference-kinds";

#[test]
fn trace_keeps_text_and_same_named_declarations_out_of_the_callers() {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("KTSENSE_NO_AUTOSTART", "1")
        .args([
            "--root",
            FIXTURE,
            "trace",
            "Sprocket",
            "--pick",
            "refkinds.Sprocket",
            "--wait-index",
        ])
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");

    let observed = (
        output.status.code(),
        stdout.contains("## Implementors (0)"),
        stdout.contains("## Callers (1)"),
        stdout.contains("- refkinds.SprocketUser.assemble  "),
        stdout.contains("## Test callers"),
        stdout.contains(
            "text mentions (comments, KDoc, strings), 1 other declaration named Sprocket",
        ),
        output.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (Some(0), true, true, true, false, true, true),
        "stdout was:\n{stdout}\nstderr was:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
