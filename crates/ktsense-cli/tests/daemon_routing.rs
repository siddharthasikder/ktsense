//! KT-54: `trace` and `map` answered through the warm daemon session equal the in-process answers.
//! KT-60: and a routed `trace` resolves its symbol from the warm session instead of spawning a
//! command-mode `find` child, without changing which candidates it reports.
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
/// A name no fixture declares, so both paths must report the same absence.
#[cfg(feature = "real-lsp")]
const ABSENT_SYMBOL: &str = "NoSuchDeclarationAnywhere";
/// A name whose declarations include two locals inside function bodies. The engine's index holds
/// them, a Kotlin skeleton does not, so this is the case that proves warm resolution reads the
/// engine's own symbol table rather than a re-derived one.
#[cfg(feature = "real-lsp")]
const LOCAL_SYMBOL: &str = "id";

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
/// flattened to success. The candidate order is not asserted: the engine reports its index in an
/// order that is not stable across invocations, so each list is checked as a set.
///
/// Two names are checked. `save` is three declarations of an interface method and its overrides.
/// `id` is three properties of which two are locals inside function bodies: the engine's index holds
/// them and a Kotlin skeleton does not, so a warm resolver that answered from re-derived syntax would
/// find one declaration, resolve it, and exit 0 instead of listing three and exiting 3.
#[cfg(feature = "real-lsp")]
#[test]
fn a_routed_ambiguous_trace_exits_three_through_the_daemon() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let locations = |symbol: &str, knob: &str| {
        let run = routed(
            runtime.path(),
            FIXTURE,
            knob,
            &["trace", symbol, "--wait-index"],
        );
        let mut found: Vec<String> = run
            .stdout
            .split_whitespace()
            .filter(|token| token.contains(".kt:"))
            .map(str::to_string)
            .collect();
        found.sort();
        (run.code, found)
    };

    let save_via_daemon = locations(SYMBOL, "KTSENSE_REQUIRE_DAEMON");
    let save_in_process = locations(SYMBOL, "KTSENSE_NO_DAEMON");
    let locals_via_daemon = locations(LOCAL_SYMBOL, "KTSENSE_REQUIRE_DAEMON");
    let locals_in_process = locations(LOCAL_SYMBOL, "KTSENSE_NO_DAEMON");

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);

    let expected_save = (
        Some(3),
        vec![
            "core/src/main/kotlin/shop/order/OrderRepository.kt:4".to_string(),
            "db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt:13".to_string(),
            "db/src/main/kotlin/shop/db/JdbcOrderRepository.kt:8".to_string(),
        ],
    );
    let expected_locals = (
        Some(3),
        vec![
            "app/src/main/kotlin/shop/app/checkout/CheckoutService.kt:13".to_string(),
            "core/src/main/kotlin/shop/order/Order.kt:6".to_string(),
            "db/src/main/kotlin/shop/db/InMemoryOrderRepository.kt:14".to_string(),
        ],
    );
    assert_eq!(
        (
            save_via_daemon.clone(),
            locals_via_daemon.clone(),
            save_via_daemon == save_in_process,
            locals_via_daemon == locals_in_process,
            stopped.code,
        ),
        (expected_save, expected_locals, true, true, Some(0)),
        "save via daemon: {save_via_daemon:?} / locals via daemon: {locals_via_daemon:?}"
    );
}

/// A name nothing declares must fail the same way on both paths. The warm index cannot distinguish
/// "absent" from "not indexed", so the routed path asks command-mode `find` before concluding
/// anything, and ktsense's own non-zero for no such symbol is what both report rather than the
/// engine's exit-0-with-no-output.
#[cfg(feature = "real-lsp")]
#[test]
fn a_routed_trace_of_an_absent_name_fails_exactly_as_the_in_process_one_does() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let args = ["trace", ABSENT_SYMBOL, "--wait-index"];
    let via_daemon = routed(runtime.path(), FIXTURE, "KTSENSE_REQUIRE_DAEMON", &args);
    let in_process = routed(runtime.path(), FIXTURE, "KTSENSE_NO_DAEMON", &args);

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);

    assert_eq!(
        (
            via_daemon.code,
            via_daemon.stderr.trim().to_string(),
            via_daemon.stdout.is_empty(),
            via_daemon.stderr == in_process.stderr,
            in_process.code,
            stopped.code,
        ),
        (
            Some(1),
            format!("ktsense: no declaration named {ABSENT_SYMBOL}"),
            true,
            true,
            Some(1),
            Some(0),
        ),
        "daemon stderr: {} / in-process stderr: {}",
        via_daemon.stderr,
        in_process.stderr
    );
}

/// The mechanism KT-60 exists to remove: a warm-daemon trace must resolve its symbol without starting
/// a command-mode `find` child.
///
/// Every engine process ktsense starts goes through a wrapper that records its argv and then execs
/// the real engine, so the count is of this test's own spawns and needs no host-wide process scan. The
/// in-process run is the control: it must show a `find`, which is what proves a zero on the daemon
/// path is an absent subprocess rather than a wrapper that never observed anything.
#[cfg(feature = "real-lsp")]
#[test]
fn a_routed_trace_resolves_its_symbol_without_spawning_a_command_mode_find() {
    use std::os::unix::fs::PermissionsExt;

    let runtime = tempfile::tempdir().expect("runtime dir");
    let engine_log = runtime.path().join("engine-invocations.log");
    let wrapper = runtime.path().join("engine-wrapper.sh");
    // Resolved before the override is installed, so the wrapper cannot re-enter itself.
    let real_engine = std::env::var("KTSENSE_LSP_PATH").unwrap_or_else(|_| "kmp-lsp".to_string());
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\nexec {engine} \"$@\"\n",
            log = engine_log.display(),
            engine = real_engine,
        ),
    )
    .expect("write wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("wrapper is executable");

    let instrumented = |knob: Option<&str>, args: &[&str]| {
        let mut command = Command::cargo_bin("ktsense").expect("binary builds");
        command
            .current_dir(WORKSPACE_ROOT)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("KTSENSE_DAEMON_IDLE_SECS", "20")
            .env("KTSENSE_LSP_PATH", &wrapper);
        if let Some(knob) = knob {
            command.env(knob, "1");
        }
        let output = command
            .args(["--root", FIXTURE])
            .args(args)
            .output()
            .expect("binary runs");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    };
    let finds = || {
        std::fs::read_to_string(&engine_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("find "))
            .count()
    };

    // The daemon must run under the wrapper too, so its own session spawn is recorded.
    let started = instrumented(None, &["daemon", "start"]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let args = ["trace", SYMBOL, "--pick", PICK, "--wait-index"];
    let finds_before = finds();
    let via_daemon = instrumented(Some("KTSENSE_REQUIRE_DAEMON"), &args);
    let finds_after_daemon = finds();
    let in_process = instrumented(Some("KTSENSE_NO_DAEMON"), &args);
    let finds_after_in_process = finds();

    let stopped = instrumented(None, &["daemon", "stop"]);
    let invocations = std::fs::read_to_string(&engine_log).unwrap_or_default();

    assert_eq!(
        (
            finds_after_daemon - finds_before,
            finds_after_in_process - finds_after_daemon,
            via_daemon.code,
            via_daemon.stdout == in_process.stdout,
            via_daemon.stdout.is_empty(),
            stopped.code,
        ),
        (0, 1, Some(0), true, false, Some(0)),
        "engine invocations:\n{invocations}"
    );
}
