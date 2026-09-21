//! KT-54: `trace` and `map` answered through the warm daemon session equal the in-process answers.
//!
//! This is a new file rather than an addition to `tests/daemon.rs` (KT-30 owns that one). It mirrors
//! that file's isolation discipline: every invocation gets its own `XDG_RUNTIME_DIR`, passed per
//! command, so the tests cannot disturb a developer's daemon or collide with each other, and the
//! idle window is squeezed to seconds so a daemon left behind by a failed assertion expires.
//!
//! The parity cases start a real daemon, so they are gated behind `real-lsp` exactly as the other
//! engine-dependent tests are. `--wait-index` is passed to both paths so each answers off a complete
//! index, which is the comparable condition the equality is asserted under. The fallback case needs
//! no engine: it proves that `KTSENSE_REQUIRE_DAEMON=1` turns an absent daemon into a failure rather
//! than a silent in-process answer, so a parity pass can never be a fallback in disguise.

use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const FIXTURE: &str = "fixtures/multi-module";
const SYMBOL: &str = "save";
#[cfg(feature = "real-lsp")]
const PICK: &str = "shop.order.OrderRepository.save";

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[cfg(feature = "real-lsp")]
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

/// Runs a command with exactly one of the routing knobs set, so a `daemon`-path answer is proven to
/// have come from the socket and an `in_process` one from the fallback, never a silent mix.
fn routed(runtime_dir: &Path, root: &str, knob: &str, args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env(knob, "1")
        .args(["--root", root])
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// One command's parity between the two paths: whether the answers are byte-identical, the exit each
/// path reported, whether both stayed silent on stderr, and whether the daemon path produced output
/// at all. Composed so a case asserts the whole record once.
#[cfg(feature = "real-lsp")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parity {
    identical: bool,
    daemon_code: Option<i32>,
    in_process_code: Option<i32>,
    both_silent: bool,
    produced_output: bool,
}

#[cfg(feature = "real-lsp")]
fn parity(runtime_dir: &Path, root: &str, args: &[&str]) -> Parity {
    let via_daemon = routed(runtime_dir, root, "KTSENSE_REQUIRE_DAEMON", args);
    let in_process = routed(runtime_dir, root, "KTSENSE_NO_DAEMON", args);
    Parity {
        identical: via_daemon.stdout == in_process.stdout,
        daemon_code: via_daemon.code,
        in_process_code: in_process.code,
        both_silent: via_daemon.stderr.is_empty() && in_process.stderr.is_empty(),
        produced_output: !via_daemon.stdout.is_empty(),
    }
}

/// `KTSENSE_REQUIRE_DAEMON=1` with no daemon listening must fail rather than fall back, and it must
/// fail before ever resolving the symbol, so this needs no engine. Without this guard a parity test
/// could pass with the daemon never consulted.
#[test]
fn require_daemon_forbids_the_fallback_for_trace_and_map() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let trace = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_REQUIRE_DAEMON",
        &["trace", SYMBOL],
    );
    let map = routed(runtime.path(), FIXTURE, "KTSENSE_REQUIRE_DAEMON", &["map"]);

    assert_eq!(
        (
            trace.code,
            trace.stderr.contains("no daemon answered"),
            trace.stdout.is_empty(),
            map.code,
            map.stderr.contains("no daemon answered"),
            map.stdout.is_empty(),
        ),
        (Some(1), true, true, Some(1), true, true),
        "trace stderr: {} / map stderr: {}",
        trace.stderr,
        map.stderr
    );
}

/// The acceptance property: with a live daemon, `trace` and `map` answered through the socket are
/// byte-identical to the in-process answers, across markdown and JSON, with `--pick` and `--depth`,
/// and whether the root is given relatively or absolutely (path neutrality). `--wait-index` pins
/// both paths to a complete index so the comparison is under comparable conditions.
#[cfg(feature = "real-lsp")]
#[test]
fn routed_trace_and_map_match_the_in_process_answers_byte_for_byte() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let absolute_root = Path::new(WORKSPACE_ROOT)
        .join(FIXTURE)
        .canonicalize()
        .expect("fixture exists")
        .display()
        .to_string();

    let cases: [(&str, Vec<&str>); 6] = [
        (
            FIXTURE,
            vec!["trace", SYMBOL, "--pick", PICK, "--wait-index"],
        ),
        (
            FIXTURE,
            vec![
                "--format",
                "json",
                "trace",
                SYMBOL,
                "--pick",
                PICK,
                "--wait-index",
            ],
        ),
        (
            FIXTURE,
            vec![
                "trace",
                SYMBOL,
                "--pick",
                PICK,
                "--depth",
                "2",
                "--wait-index",
            ],
        ),
        (FIXTURE, vec!["map", "--budget", "4000"]),
        (FIXTURE, vec!["--format", "json", "map", "--budget", "4000"]),
        // Path neutrality: an absolute root must yield the same fixture-relative answer.
        (absolute_root.as_str(), vec!["map", "--budget", "4000"]),
    ];
    let observed: Vec<Parity> = cases
        .iter()
        .map(|(root, args)| parity(runtime.path(), root, args))
        .collect();

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);

    let expected = Parity {
        identical: true,
        daemon_code: Some(0),
        in_process_code: Some(0),
        both_silent: true,
        produced_output: true,
    };
    assert_eq!(
        (observed, stopped.code),
        (vec![expected; cases.len()], Some(0))
    );
}

/// An ambiguous name routed through the daemon must still list every candidate and exit 3, the same
/// contract the in-process path holds, which proves the exit code travels the wire rather than being
/// flattened to success. The candidate order is not asserted: it comes from the engine's
/// command-mode `find`, which is not order-stable across invocations, so the list is checked as a
/// set. Ambiguity is resolved before any session, so this exercises the wire's exit handling, not
/// the warm session.
#[cfg(feature = "real-lsp")]
#[test]
fn a_routed_ambiguous_trace_exits_three_through_the_daemon() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let via_daemon = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_REQUIRE_DAEMON",
        &["trace", SYMBOL],
    );
    let in_process = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_NO_DAEMON",
        &["trace", SYMBOL],
    );

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);

    let lists_every_candidate = |out: &str| {
        out.contains("## Symbols: save")
            && out.contains("shop.order.OrderRepository.save")
            && out.contains("shop.db.JdbcOrderRepository.save")
            && out.contains("shop.db.InMemoryOrderRepository.save")
    };
    assert_eq!(
        (
            via_daemon.code,
            lists_every_candidate(&via_daemon.stdout),
            in_process.code,
            lists_every_candidate(&in_process.stdout),
            stopped.code,
        ),
        (Some(3), true, Some(3), true, Some(0)),
        "daemon stdout: {}",
        via_daemon.stdout
    );
}
