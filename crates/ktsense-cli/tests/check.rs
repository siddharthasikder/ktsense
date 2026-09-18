//! End-to-end `check`/`diagnose` tests against the real pinned `kmp-lsp`, gated behind `real-lsp`
//! so the default install-free suite never compiles or runs them. They pin the process contract
//! the calling agent branches on: a clean file exits 0, a broken file exits 1 while still printing
//! its report to stdout, and the engine's own progress noise never leaks onto stderr.
#![cfg(feature = "real-lsp")]

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const GOOD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tiny-app/src/main/kotlin/app/service/Service.kt"
);
const REPO_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn ktsense(args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

#[test]
fn check_exits_zero_on_a_clean_file_and_one_on_a_broken_one_without_leaking_noise() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broken = dir.path().join("Broken.kt");
    std::fs::write(&broken, "package p\nfun broken( {\n").expect("write broken");
    let broken_arg = broken.to_string_lossy();
    let dir_arg = dir.path().to_string_lossy();

    let clean = ktsense(&["check", GOOD, "--root", REPO_ROOT]);
    let errored = ktsense(&["check", broken_arg.as_ref(), "--root", dir_arg.as_ref()]);

    let observed = (
        clean.code,
        clean.stdout.contains("0 with errors."),
        clean.stderr.is_empty(),
        errored.code,
        errored.stdout.contains("1 with errors."),
        errored.stdout.contains("Broken.kt:2:"),
        errored.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (Some(0), true, true, Some(1), true, true, true),
        "clean={clean:?} errored={errored:?}",
    );
}

impl std::fmt::Debug for Run {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Run")
            .field("code", &self.code)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

#[test]
fn diagnose_reports_a_syntax_finding_and_exits_zero() {
    let dir = tempfile::tempdir().expect("temp dir");
    let broken = dir.path().join("Broken.kt");
    std::fs::write(&broken, "package p\nfun broken( {\n").expect("write broken");
    let broken_arg = broken.to_string_lossy();
    let dir_arg = dir.path().to_string_lossy();

    let run = ktsense(&["diagnose", broken_arg.as_ref(), "--root", dir_arg.as_ref()]);

    let observed = (
        run.code,
        run.stdout.contains("[error]:"),
        run.stderr.is_empty(),
    );
    assert_eq!(observed, (Some(0), true, true), "run={run:?}");
}
