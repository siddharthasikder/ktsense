//! Process-level tests for the startup version check, driving the KT-15 `fake_lsp` binary with
//! `--version`. The fake reports whatever `FAKE_LSP_VERSION` holds (defaulting to the pin), and the
//! `__HANG__` sentinel makes it never answer, so the timeout and no-leak guarantees are exercised
//! without a real `kmp-lsp` install. `FAKE_LSP_VERSION` is process-global, so cases that set it run
//! under a shared guard held across the probe.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ktsense_lsp::{check_version, check_version_within, VersionCheck};
use serde_json::json;
use tokio::sync::Mutex;

static ENV_GUARD: Mutex<()> = Mutex::const_new(());

fn fake_lsp() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_lsp"))
}

async fn check_reporting(version: &str) -> VersionCheck {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_LSP_VERSION", version);
    let verdict = check_version(&fake_lsp()).await;
    std::env::remove_var("FAKE_LSP_VERSION");
    drop(guard);
    verdict
}

#[tokio::test]
async fn supported_version_starts_silently() {
    assert_eq!(
        check_reporting("kmp-lsp 0.26.0").await,
        VersionCheck::Compatible
    );
}

#[tokio::test]
async fn major_mismatch_is_refused_with_an_actionable_diagnostic() {
    let verdict = check_reporting("kmp-lsp 2.0.0").await;
    let observed = match &verdict {
        VersionCheck::Refuse(message) => json!({
            "refused": true,
            "names_reported": message.contains("2.0.0"),
            "names_pin": message.contains("0.26.0"),
        }),
        other => json!({ "refused": false, "was": format!("{other:?}") }),
    };
    assert_eq!(
        observed,
        json!({ "refused": true, "names_reported": true, "names_pin": true })
    );
}

#[tokio::test]
async fn a_binary_that_cannot_run_warns_rather_than_refusing() {
    let verdict = check_version(Path::new("/nonexistent/kmp-lsp-KT26")).await;
    assert!(
        matches!(verdict, VersionCheck::Warn(_)),
        "verdict was {verdict:?}"
    );
}

#[tokio::test]
async fn a_hanging_version_probe_is_bounded_and_never_blocks() {
    let guard = ENV_GUARD.lock().await;
    std::env::set_var("FAKE_LSP_VERSION", "__HANG__");
    let started = Instant::now();
    let verdict = check_version_within(&fake_lsp(), Duration::from_millis(300)).await;
    let elapsed = started.elapsed();
    std::env::remove_var("FAKE_LSP_VERSION");
    drop(guard);

    let observed = json!({
        "warned": matches!(verdict, VersionCheck::Warn(_)),
        "returned_before_hang": elapsed < Duration::from_secs(2),
    });
    assert_eq!(
        observed,
        json!({ "warned": true, "returned_before_hang": true }),
        "verdict {verdict:?} after {elapsed:?}"
    );
}
