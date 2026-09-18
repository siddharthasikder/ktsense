//! Lifecycle tests for `daemon start|stop|status`.
//!
//! Every invocation gets its own `XDG_RUNTIME_DIR`, passed per command rather than set on this
//! process, so the tests cannot disturb a developer's running daemon, cannot collide with each other,
//! and need no global environment mutation. The idle window is squeezed to seconds so a daemon left
//! behind by a failed assertion expires instead of lingering for an hour.
//!
//! The cases that need no engine run in the default suite. Starting a daemon means launching the real
//! upstream engine, so the full start-status-stop cycle is gated behind `real-lsp`, matching how the
//! other engine-dependent tests are gated.

use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The socket path the command reported, so a test can act on the real path rather than
    /// recomputing the daemon's hashing scheme and drifting from it.
    fn socket(&self) -> String {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("socket: "))
            .map(|rest| {
                rest.trim_end_matches(" does not exist")
                    .split(" is a stale")
                    .next()
                    .unwrap_or(rest)
                    .to_string()
            })
            .expect("status names the socket")
    }
}

fn daemon(runtime_dir: &Path, args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("KTSENSE_DAEMON_IDLE_SECS", "20")
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

fn status(runtime_dir: &Path) -> Run {
    daemon(runtime_dir, &["daemon", "status", "--root", FIXTURE])
}

/// Absence is a successful answer, not a failure: an agent asking whether a daemon is running gets
/// told, and it learns where the socket would be so it can look for itself.
#[test]
fn status_and_stop_report_absence_as_success_rather_than_as_an_error() {
    let temp = tempfile::tempdir().expect("temp dir");

    let queried = status(temp.path());
    let stopped = daemon(temp.path(), &["daemon", "stop", "--root", FIXTURE]);

    assert_eq!(
        (
            queried.code,
            queried.stdout.contains("no daemon running"),
            queried.stdout.contains("does not exist"),
            queried.stderr.as_str(),
            stopped.code,
            stopped.stdout.contains("no daemon was running"),
            stopped.stderr.as_str(),
        ),
        (Some(0), true, true, "", Some(0), true, "")
    );
}

/// A leftover socket must be named as stale rather than reported as a live daemon, because the two
/// lead an agent to opposite conclusions.
#[test]
fn a_leftover_socket_is_reported_as_stale_and_not_as_a_running_daemon() {
    let temp = tempfile::tempdir().expect("temp dir");
    let socket = status(temp.path()).socket();
    let path = Path::new(&socket);
    std::fs::create_dir_all(path.parent().expect("socket has a parent")).expect("create dir");
    std::fs::write(path, b"not a socket").expect("plant a leftover");

    let queried = status(temp.path());

    assert_eq!(
        (
            queried.code,
            queried.stdout.contains("stale leftover"),
            // Anchored on the message prefix: "no daemon running for" contains "daemon running for",
            // so a bare substring check could never tell absence from liveness.
            queried.stdout.contains("ktsense: daemon running for"),
        ),
        (Some(0), true, false)
    );
}

/// `--root` selects the daemon, so two roots must not share one. A collision here would mean one
/// repository's answers served from another's index.
#[test]
fn distinct_roots_address_distinct_sockets() {
    let temp = tempfile::tempdir().expect("temp dir");

    let multi = status(temp.path()).socket();
    let tiny = daemon(
        temp.path(),
        &["daemon", "status", "--root", "fixtures/tiny-app"],
    )
    .socket();

    assert_ne!(multi, tiny);
}

/// The serve subcommand backs `start` and is deliberately absent from help, so a reader is not
/// invited to run a blocking daemon body by hand.
#[test]
fn serve_is_hidden_from_help_because_start_owns_it() {
    let temp = tempfile::tempdir().expect("temp dir");

    let help = daemon(temp.path(), &["daemon", "--help"]);

    assert_eq!(
        (
            help.code,
            help.stdout.contains("start"),
            help.stdout.contains("stop"),
            help.stdout.contains("status"),
            help.stdout.contains("serve"),
        ),
        (Some(0), true, true, true, false)
    );
}

/// The whole cycle against the real engine: a start that reports a reachable daemon, a second start
/// that reports the existing one instead of racing a duplicate, a stop, and a stop that finds nothing.
#[cfg(feature = "real-lsp")]
#[test]
fn the_lifecycle_starts_once_is_idempotent_and_stops_cleanly() {
    let temp = tempfile::tempdir().expect("temp dir");
    let started = daemon(temp.path(), &["daemon", "start", "--root", FIXTURE]);
    let live = status(temp.path());
    let again = daemon(temp.path(), &["daemon", "start", "--root", FIXTURE]);
    let stopped = daemon(temp.path(), &["daemon", "stop", "--root", FIXTURE]);
    let after = status(temp.path());
    let stopped_twice = daemon(temp.path(), &["daemon", "stop", "--root", FIXTURE]);

    assert_eq!(
        (
            (started.code, started.stdout.contains("daemon started for")),
            (live.code, live.stdout.contains("daemon running for")),
            (again.code, again.stdout.contains("already running")),
            (stopped.code, stopped.stdout.contains("daemon stopped for")),
            (after.code, after.stdout.contains("no daemon running")),
            (
                stopped_twice.code,
                stopped_twice.stdout.contains("no daemon was running")
            ),
        ),
        (
            (Some(0), true),
            (Some(0), true),
            (Some(0), true),
            (Some(0), true),
            (Some(0), true),
            (Some(0), true),
        )
    );
}
