//! Process-level tests for the command-mode passthrough, driving the KT-15 `fake_lsp` binary in
//! command mode. The fake emits whatever `FAKE_CMD_STDOUT`/`FAKE_CMD_STDERR`/`FAKE_CMD_EXIT` hold
//! and hangs on `FAKE_CMD_HANG`, so success, reported findings, a genuine failure, noisy output,
//! and a wedged engine are all reproduced without a real `kmp-lsp` install. Those env vars are
//! process-global, so every case runs under a shared guard held across the invocation.
//!
//! The `real-lsp`-gated tests at the bottom run the pinned upstream engine against a fixture and
//! are not compiled into the default, install-free suite.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ktsense_lsp::{CheckReport, EngineCommand, PassthroughError, Severity, SyntaxError};
use tokio::sync::Mutex;

static ENV_GUARD: Mutex<()> = Mutex::const_new(());

fn fake_lsp() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_lsp"))
}

async fn check_replay(
    root: &Path,
    path: &Path,
    stdout: &str,
    exit: i32,
) -> Result<CheckReport, PassthroughError> {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_CMD_STDOUT", stdout);
    std::env::set_var("FAKE_CMD_EXIT", exit.to_string());
    let result = EngineCommand::new(&fake_lsp(), root).check(path).await;
    std::env::remove_var("FAKE_CMD_STDOUT");
    std::env::remove_var("FAKE_CMD_EXIT");
    drop(guard);
    result
}

async fn diagnose_replay(
    root: &Path,
    file: &Path,
    stdout: &str,
    stderr: &str,
    exit: i32,
) -> Result<ktsense_lsp::DiagnoseReport, PassthroughError> {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_CMD_STDOUT", stdout);
    std::env::set_var("FAKE_CMD_STDERR", stderr);
    std::env::set_var("FAKE_CMD_EXIT", exit.to_string());
    let result = EngineCommand::new(&fake_lsp(), root).diagnose(file).await;
    std::env::remove_var("FAKE_CMD_STDOUT");
    std::env::remove_var("FAKE_CMD_STDERR");
    std::env::remove_var("FAKE_CMD_EXIT");
    drop(guard);
    result
}

#[tokio::test]
async fn check_success_and_reported_errors_normalize_paths_and_signal_failure() {
    let root = Path::new("/repo");
    let ok = check_replay(
        root,
        Path::new("/repo/src/Ok.kt"),
        r#"{"errors":[],"files_ok":1,"files_with_errors":0}"#,
        0,
    )
    .await
    .expect("clean check");
    let broken = check_replay(
        root,
        Path::new("/repo/src/Bad.kt"),
        r#"{"errors":[{"col":1,"file":"/repo/src/Bad.kt","line":2,"message":"unexpected `fun`"}],"files_ok":0,"files_with_errors":1}"#,
        1,
    )
    .await
    .expect("errored check");

    let observed = (ok.has_errors(), broken.has_errors(), broken.errors);
    assert_eq!(
        observed,
        (
            false,
            true,
            vec![SyntaxError {
                file: "src/Bad.kt".to_string(),
                line: 2,
                col: 1,
                message: "unexpected `fun`".to_string(),
            }]
        )
    );
}

#[tokio::test]
async fn check_noisy_or_nonjson_output_is_a_parse_error_never_a_silent_pass() {
    let observed = check_replay(
        Path::new("/repo"),
        Path::new("/repo/src/A.kt"),
        "[INFO kmp_lsp] indexing\nthis is not json",
        0,
    )
    .await;
    assert!(
        matches!(observed, Err(PassthroughError::Unparseable { .. })),
        "was {observed:?}"
    );
}

#[tokio::test]
async fn diagnose_reports_findings_with_zero_exit_and_none_on_the_clean_sentinel() {
    let root = Path::new("/repo");
    let findings = diagnose_replay(
        root,
        Path::new("/repo/src/When.kt"),
        "3:15 [warning]: 'when' is missing branches: B\n",
        "Indexing /repo...\nIndexed: 1 files, 2 symbols\n",
        0,
    )
    .await
    .expect("findings");
    let clean = diagnose_replay(
        root,
        Path::new("/repo/src/When.kt"),
        "No diagnostics.\n",
        "",
        0,
    )
    .await
    .expect("clean");

    let observed = (
        findings.file.clone(),
        findings.diagnostics,
        clean.diagnostics.is_empty(),
    );
    assert_eq!(
        observed,
        (
            "src/When.kt".to_string(),
            vec![ktsense_lsp::Diagnostic {
                line: 3,
                col: 15,
                severity: Severity::Warning,
                message: "'when' is missing branches: B".to_string(),
            }],
            true
        )
    );
}

#[tokio::test]
async fn diagnose_nonzero_exit_is_a_failure_carrying_the_engine_stderr() {
    let observed = diagnose_replay(
        Path::new("/repo"),
        Path::new("/repo/src/Missing.kt"),
        "",
        "error: cannot read file: No such file or directory (os error 2)",
        1,
    )
    .await;
    let matched = matches!(
        &observed,
        Err(PassthroughError::Failed { stderr, .. }) if stderr.contains("cannot read file")
    );
    assert!(matched, "was {observed:?}");
}

#[tokio::test]
async fn a_hanging_engine_is_bounded_and_the_child_is_not_left_running() {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_CMD_HANG", "1");
    let binary = fake_lsp();
    let started = Instant::now();
    let result = EngineCommand::within(&binary, Path::new("/repo"), Duration::from_millis(300))
        .check(Path::new("/repo/src/A.kt"))
        .await;
    let elapsed = started.elapsed();
    std::env::remove_var("FAKE_CMD_HANG");
    drop(guard);

    let observed = (
        matches!(result, Err(PassthroughError::Timeout { .. })),
        elapsed < Duration::from_secs(2),
    );
    assert_eq!(
        observed,
        (true, true),
        "result {result:?} after {elapsed:?}"
    );
}

#[cfg(feature = "real-lsp")]
mod real {
    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ktsense-kt23-{name}"));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[tokio::test]
    async fn real_check_distinguishes_a_clean_file_from_a_broken_one() {
        let root = repo_root();
        let good = root.join("fixtures/tiny-app/src/main/kotlin/app/service/Service.kt");
        let broken_dir = scratch("check");
        let broken = broken_dir.join("Broken.kt");
        std::fs::write(&broken, "package p\nfun broken( {\n").expect("write broken");

        let good_report = ktsense_lsp::run_check(&root, &good)
            .await
            .expect("good check");
        let broken_report = ktsense_lsp::run_check(&broken_dir, &broken)
            .await
            .expect("broken check");
        std::fs::remove_dir_all(&broken_dir).ok();

        assert_eq!(
            (good_report.has_errors(), broken_report.has_errors()),
            (false, true)
        );
    }

    #[tokio::test]
    async fn real_diagnose_reports_a_syntax_finding_and_exits_clean() {
        let dir = scratch("diagnose");
        let file = dir.join("Broken.kt");
        std::fs::write(&file, "package p\nfun broken( {\n").expect("write broken");

        let report = ktsense_lsp::run_diagnose(&dir, &file)
            .await
            .expect("diagnose");
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error),
            "expected a syntax error, got {report:?}"
        );
    }
}
