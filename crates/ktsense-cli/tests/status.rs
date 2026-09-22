//! `ktsense status` reports the installation's state without ever failing because of it.
//!
//! Each case gets its own `XDG_RUNTIME_DIR` so no developer daemon is consulted, and the engine is
//! the fake, pointed at through `KTSENSE_LSP_PATH`, so the version line is deterministic without an
//! upstream install. Host-specific paths are neutralized before the goldens are compared. The case
//! that needs a real daemon, and therefore the real engine, sits behind `real-lsp`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/tiny-app";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn status(runtime_dir: &Path, engine: &Path, args: &[&str]) -> Run {
    let mut command = Command::cargo_bin("ktsense").expect("binary builds");
    command
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("KTSENSE_LSP_PATH", engine)
        .env("KTSENSE_DAEMON_IDLE_SECS", "20");
    // Discovery falls back from the override to `PATH`, so an engine can only be made absent by
    // emptying `PATH` as well; otherwise a host with kmp-lsp installed would truthfully report it.
    // Only an absolute path that does not exist means absence: a bare name is itself a `PATH`
    // lookup, and clearing `PATH` for one makes the engine unspawnable rather than unfound.
    if engine.is_absolute() && !engine.exists() {
        command.env("PATH", runtime_dir);
    }
    let output = command.args(args).output().expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

fn fake_lsp() -> PathBuf {
    static FAKE: OnceLock<PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let path = Path::new(env!("CARGO_BIN_EXE_ktsense")).with_file_name("fake_lsp");
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
            let built = Command::new(cargo)
                .args(["build", "-p", "ktsense-lsp", "--bin", "fake_lsp"])
                .current_dir(WORKSPACE_ROOT)
                .status()
                .expect("cargo runs");
            assert!(built.success(), "building fake_lsp failed");
        }
        path
    })
    .clone()
}

/// Replaces the host-specific values with stable placeholders: the engine path, the runtime
/// directory, the workspace root, and the socket file's sixteen-hex-digit root hash.
fn neutralize(text: &str, runtime_dir: &Path, engine: &Path) -> String {
    let root = Path::new(WORKSPACE_ROOT)
        .canonicalize()
        .expect("workspace exists");
    let text = text
        .replace(&engine.display().to_string(), "<engine>")
        .replace(&runtime_dir.display().to_string(), "<runtime>")
        .replace(&root.display().to_string(), "<workspace>");
    neutralize_root_hashes(&text)
}

fn neutralize_root_hashes(text: &str) -> String {
    const HASH_LEN: usize = 16;
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(".sock") {
        let (before, after) = rest.split_at(at);
        let split = before.len().saturating_sub(HASH_LEN);
        let is_hash = before.is_char_boundary(split)
            && before.len() - split == HASH_LEN
            && before[split..].chars().all(|c| c.is_ascii_hexdigit());
        if is_hash {
            out.push_str(&before[..split]);
            out.push_str("<root-hash>");
        } else {
            out.push_str(before);
        }
        out.push_str(".sock");
        rest = &after[".sock".len()..];
    }
    out.push_str(rest);
    out
}

/// A runtime directory shallow enough that a socket derived from it is placed inside it on every
/// platform.
///
/// The goldens below pin the socket as `<runtime>/ktsense/<root-hash>.sock`, and that is the
/// placement only a runtime directory within the socket budget gets: a deeper one is answered from a
/// short per-uid base instead, correctly, and the golden would then not match. A default temporary
/// directory is a handful of bytes deep on Linux and around sixty on macOS, close enough to the
/// budget that the pinned shape would be a property of the host rather than of this test, so the
/// shallow base is chosen here deliberately (KT-66).
fn runtime_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ktsense-status.")
        .tempdir_in(SHALLOW_TEMP_BASE)
        .expect("temp dir")
}

const SHALLOW_TEMP_BASE: &str = "/tmp";

#[test]
fn status_without_a_daemon_is_pinned_in_both_formats() {
    let temp = runtime_dir();
    let engine = fake_lsp();
    let md = status(temp.path(), &engine, &["--root", FIXTURE, "status"]);
    let json = status(
        temp.path(),
        &engine,
        &["--root", FIXTURE, "--format", "json", "status"],
    );

    assert_eq!(
        (
            md.code,
            md.stderr.is_empty(),
            json.code,
            json.stderr.is_empty()
        ),
        (Some(0), true, Some(0), true)
    );
    insta::assert_snapshot!(
        "status_absent_md",
        neutralize(&md.stdout, temp.path(), &engine)
    );
    insta::assert_snapshot!(
        "status_absent_json",
        neutralize(&json.stdout, temp.path(), &engine)
    );
}

/// A leftover socket file and a missing engine are both states to report, not reasons to fail.
#[test]
fn a_stale_socket_and_a_missing_engine_are_reported_with_exit_zero() {
    let temp = runtime_dir();
    let engine = fake_lsp();
    let absent = status(temp.path(), &engine, &["--root", FIXTURE, "status"]);
    let socket = absent
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("socket: "))
        .and_then(|rest| rest.strip_suffix(" does not exist"))
        .expect("status names the absent socket");
    std::fs::create_dir_all(Path::new(socket).parent().expect("socket has a dir")).expect("dir");
    std::fs::write(socket, b"").expect("a plain file where the socket would be");
    let stale = status(temp.path(), &engine, &["--root", FIXTURE, "status"]);
    let no_engine = status(
        temp.path(),
        Path::new("/nonexistent/kmp-lsp"),
        &["--root", FIXTURE, "--format", "json", "status"],
    );
    let engine_json: serde_json::Value =
        serde_json::from_str(&no_engine.stdout).expect("json output");

    assert_eq!(
        (
            stale.code,
            stale
                .stdout
                .contains("is a stale leftover and will be reclaimed by the next start"),
            no_engine.code,
            engine_json["engine"]["version"].clone(),
            engine_json["engine"]["compatibility"].clone(),
        ),
        (
            Some(0),
            true,
            Some(0),
            serde_json::Value::Null,
            serde_json::json!("unavailable")
        ),
        "stale: {}\nno engine: {}",
        stale.stdout,
        no_engine.stdout
    );
}

/// KT-36 against a real daemon: the snapshot is live state, so the index settles to complete and the
/// served count moves when a routed command runs. `daemon status` shares the same section.
#[cfg(feature = "real-lsp")]
#[test]
fn a_running_daemon_reports_uptime_a_settled_index_and_a_moving_request_count() {
    use std::time::{Duration, Instant};

    const FIXTURE: &str = "fixtures/multi-module";
    let temp = runtime_dir();
    let real = PathBuf::from("kmp-lsp");
    let lifecycle =
        |action: &str| status(temp.path(), &real, &["--root", FIXTURE, "daemon", action]);
    let snapshot = || {
        let run = status(
            temp.path(),
            &real,
            &["--root", FIXTURE, "--format", "json", "status"],
        );
        let value: serde_json::Value = serde_json::from_str(&run.stdout).expect("json output");
        value["daemon"].clone()
    };

    let started = lifecycle("start");
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = snapshot();
    while settled["index"] != "complete" && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        settled = snapshot();
    }
    let mut routed = Command::cargo_bin("ktsense").expect("binary builds");
    routed
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", temp.path())
        .env("KTSENSE_REQUIRE_DAEMON", "1")
        .args(["--root", FIXTURE, "deps"]);
    let routed = routed.output().expect("binary runs");
    let after = snapshot();
    let via_daemon_command = lifecycle("status");
    let stopped = lifecycle("stop");

    assert_eq!(
        (
            settled["state"].clone(),
            settled["index"].clone(),
            settled["uptime_secs"].is_u64(),
            settled["requests_served"].clone(),
            routed.status.code(),
            after["requests_served"].clone(),
            via_daemon_command.stdout.contains("daemon running for")
                && via_daemon_command.stdout.contains("index: complete")
                && via_daemon_command.stdout.contains("requests served: 1"),
            stopped.code,
        ),
        (
            serde_json::json!("running"),
            serde_json::json!("complete"),
            true,
            serde_json::json!(0),
            Some(0),
            serde_json::json!(1),
            true,
            Some(0),
        ),
        "daemon status was: {}",
        via_daemon_command.stdout
    );
}
