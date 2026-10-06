//! KT-105: an engine-backed command that answers in process because no daemon is live starts one in
//! the background, so the next question in the session is answered warm. The first answer is
//! byte-identical to one taken with autostart off, and `KTSENSE_NO_AUTOSTART=1` suppresses the start
//! entirely.
//!
//! The command driven here is an ambiguous `trace`, which answers from command-mode `find` alone and
//! opens no LSP session of its own (see `tests/trace.rs`). That keeps the parent's own engine script
//! out of the picture: the only engine session is the one the autostarted daemon warms, so the fake's
//! `FAKE_LSP_SCRIPT` serves that daemon and `FAKE_CMD_STDOUT` serves the parent's resolution. Every
//! invocation gets its own `XDG_RUNTIME_DIR`, so a daemon this test starts cannot be reached by any
//! other test or by a developer's daemon, and the idle window is squeezed so a daemon left behind by
//! a failed assertion expires in seconds.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";

/// The session the autostarted daemon's warm engine replays: handshake, a completed index, then
/// teardown. Announcing the index is what lets the warm-up finish. The parent's ambiguous trace never
/// opens a session, so this script is consumed only by the daemon.
const WARM_SESSION: &str = r#"{"steps":[
  {"kind":"expect","method":"initialize","respond":{"result":{"capabilities":{}}}},
  {"kind":"expect","method":"initialized"},
  {"kind":"notify","method":"$/progress",
   "params":{"token":"indexing","value":{"kind":"begin","title":"Indexing"}}},
  {"kind":"notify","method":"$/progress","params":{"token":"indexing","value":{"kind":"end"}}},
  {"kind":"expect","method":"shutdown","respond":{"result":null}},
  {"kind":"expect","method":"exit"}
]}"#;

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

/// Three declarations of one name, so the parent `trace` is ambiguous: it lists them, exits 3, and
/// opens no session of its own.
fn every_save() -> Value {
    let candidate = |relative: &str, line: u32, col: u32| {
        json!({ "file": fixture_root().join(relative).display().to_string(),
                "line": line, "col": col, "name": "save" })
    };
    json!([
        candidate("db/src/main/kotlin/shop/db/JdbcOrderRepository.kt", 8, 18),
        candidate(
            "db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt",
            13,
            18
        ),
        candidate("core/src/main/kotlin/shop/order/OrderRepository.kt", 4, 9),
    ])
}

/// Runs an ambiguous `trace` against the fake, in its own runtime directory. Autostart is on unless
/// `KTSENSE_NO_AUTOSTART=1` is requested. The idle window is short so a daemon left behind expires.
fn ambiguous_trace(runtime: &Path, opt_out: bool) -> Run {
    let mut command = Command::cargo_bin("ktsense").expect("binary builds");
    command
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("KTSENSE_LSP_PATH", fake_lsp())
        .env("FAKE_CMD_STDOUT", every_save().to_string())
        .env("FAKE_LSP_SCRIPT", WARM_SESSION)
        .env("KTSENSE_DAEMON_IDLE_SECS", "15")
        .args(["--root", FIXTURE, "trace", "save"]);
    if opt_out {
        command.env("KTSENSE_NO_AUTOSTART", "1");
    }
    output(command)
}

/// Whether a daemon is live for the fixture under `runtime`, read from `daemon status`. The live
/// headline is `ktsense: daemon running for`, which the absent headline (`ktsense: no daemon running
/// for`) does not contain, so the check cannot read an absent daemon as live.
fn daemon_live(runtime: &Path) -> bool {
    let mut command = Command::cargo_bin("ktsense").expect("binary builds");
    command
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime)
        .args(["--root", FIXTURE, "daemon", "status"]);
    output(command)
        .stdout
        .contains("ktsense: daemon running for")
}

/// Stops the fixture's daemon under `runtime`, reporting whether one was stopped.
fn stop(runtime: &Path) -> bool {
    let mut command = Command::cargo_bin("ktsense").expect("binary builds");
    command
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime)
        .args(["--root", FIXTURE, "daemon", "stop"]);
    output(command).stdout.contains("daemon stopped for")
}

fn poll_until_live(runtime: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if daemon_live(runtime) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn output(mut command: Command) -> Run {
    let output = command.output().expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// With autostart on, an engine-backed command that answered in process leaves a live daemon behind,
/// and its answer is byte-identical to the same command run with autostart off. The opt-out run is in
/// its own runtime so it starts nothing of its own to compare against.
#[test]
fn an_in_process_engine_answer_warms_a_daemon_and_the_answer_is_unchanged() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let baseline_runtime = tempfile::tempdir().expect("runtime dir");

    let warmed = ambiguous_trace(runtime.path(), false);
    let baseline = ambiguous_trace(baseline_runtime.path(), true);
    let became_live = poll_until_live(runtime.path(), Duration::from_secs(15));
    let stopped = stop(runtime.path());

    assert_eq!(
        (
            warmed.code,
            warmed.stdout == baseline.stdout,
            warmed.stderr == baseline.stderr,
            became_live,
            stopped,
        ),
        (Some(3), true, true, true, true),
        "warmed stdout: {}\nwarmed stderr: {}\nbaseline stdout: {}",
        warmed.stdout,
        warmed.stderr,
        baseline.stdout
    );
}

/// `KTSENSE_NO_AUTOSTART=1` answers in process and starts nothing: no daemon ever becomes live. The
/// wait is bounded because the fake daemon, were one wrongly started, comes up in well under a second.
#[test]
fn the_opt_out_answers_in_process_and_starts_no_daemon() {
    let runtime = tempfile::tempdir().expect("runtime dir");

    let answered = ambiguous_trace(runtime.path(), true);
    let stayed_absent = !poll_until_live(runtime.path(), Duration::from_secs(2));
    let stopped_nothing = !stop(runtime.path());

    assert_eq!(
        (answered.code, stayed_absent, stopped_nothing),
        (Some(3), true, true),
        "answered stdout: {}",
        answered.stdout
    );
}
