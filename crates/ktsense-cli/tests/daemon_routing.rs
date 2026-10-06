//! KT-54: `trace` and `map` answered through the warm daemon session equal the in-process answers.
//! KT-60: and a routed `trace` resolves its symbol from the warm session instead of spawning a
//! command-mode `find` child, without changing which candidates it reports.
//! KT-89: `context` routes the same way, answered on the warm session as a depth-1 `trace` with its
//! budgeted bundle packed around it.
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
fn parity(runtime_dir: &Path, root: &str, args: &[&str]) -> (Parity, String) {
    let via_daemon = routed(runtime_dir, root, "KTSENSE_REQUIRE_DAEMON", args);
    let in_process = routed(runtime_dir, root, "KTSENSE_NO_DAEMON", args);
    let record = Parity {
        identical: via_daemon.stdout == in_process.stdout,
        daemon_code: via_daemon.code,
        in_process_code: in_process.code,
        both_silent: via_daemon.stderr.is_empty() && in_process.stderr.is_empty(),
        produced_output: !via_daemon.stdout.is_empty(),
    };
    (record, transcript(args, &via_daemon, &in_process))
}

/// What each path wrote for one case. A record of booleans and exit codes cannot say why a path
/// failed, and the message is the only place the failing path's own words survive, so a run that
/// resolves a name on one path and not the other reports which resolution step gave up rather than
/// only that the two disagreed.
#[cfg(feature = "real-lsp")]
fn transcript(args: &[&str], via_daemon: &Run, in_process: &Run) -> String {
    let side = |label: &str, run: &Run| {
        format!(
            "    {label}: exit {:?}\n      stdout: {}\n      stderr: {}",
            run.code,
            first_line(&run.stdout),
            run.stderr.trim()
        )
    };
    format!(
        "  case `{}`:\n{}\n{}",
        args.join(" "),
        side("daemon", via_daemon),
        side("in-process", in_process)
    )
}

/// The first line of an answer, which identifies it without pasting a whole trace into a failure.
#[cfg(feature = "real-lsp")]
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("<empty>")
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
    let (observed, transcripts): (Vec<Parity>, Vec<String>) = cases
        .iter()
        .map(|(root, args)| parity(runtime.path(), root, args))
        .unzip();

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
        (vec![expected; cases.len()], Some(0)),
        "what each path said:\n{}",
        transcripts.join("\n")
    );
}

/// The KT-89 acceptance property: with a live daemon, `context` answered through the socket is
/// byte-identical to the in-process answer in both markdown and JSON, exits 0, stays silent on
/// stderr, and the markdown carries a `## Source` section. `CheckoutService` spans its whole file,
/// so it exercises the outline, source, callers and implementors sections together.
#[cfg(feature = "real-lsp")]
#[test]
fn routed_context_matches_the_in_process_answer_and_names_a_source_section() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let md_via_daemon = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_REQUIRE_DAEMON",
        &["context", "CheckoutService"],
    );
    let md_in_process = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_NO_DAEMON",
        &["context", "CheckoutService"],
    );
    let json_via_daemon = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_REQUIRE_DAEMON",
        &["--format", "json", "context", "CheckoutService"],
    );
    let json_in_process = routed(
        runtime.path(),
        FIXTURE,
        "KTSENSE_NO_DAEMON",
        &["--format", "json", "context", "CheckoutService"],
    );

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);

    assert_eq!(
        (
            md_via_daemon.stdout == md_in_process.stdout,
            json_via_daemon.stdout == json_in_process.stdout,
            md_via_daemon.code,
            json_via_daemon.code,
            md_via_daemon.stderr.is_empty() && md_in_process.stderr.is_empty(),
            json_via_daemon.stderr.is_empty() && json_in_process.stderr.is_empty(),
            md_via_daemon.stdout.contains("## Source"),
            stopped.code,
        ),
        (true, true, Some(0), Some(0), true, true, true, Some(0)),
        "md via daemon:\n{}\nmd in-process:\n{}\ndaemon stderr: {} / in-process stderr: {}",
        md_via_daemon.stdout,
        md_in_process.stdout,
        md_via_daemon.stderr,
        md_in_process.stderr,
    );
}

/// KT-127: the path and test filters cross the wire (protocol 11), so a filtered trace and a filtered
/// context answered by the daemon must equal the in-process answers byte for byte, in markdown and
/// JSON, and each must actually say it was filtered. One record per case, compared as a whole.
#[cfg(feature = "real-lsp")]
#[test]
fn routed_filtered_trace_and_context_match_the_in_process_answers_byte_for_byte() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let cases: [&[&str]; 4] = [
        &["trace", "OrderRepository.save", "--path", "app/"],
        &["trace", "OrderRepository.save", "--no-tests"],
        &[
            "--format",
            "json",
            "trace",
            "OrderRepository.save",
            "--path",
            "app/",
            "--no-tests",
        ],
        &["context", "OrderRepository.save", "--path", "app/"],
    ];
    let mut transcripts = Vec::new();
    let observed: Vec<(Parity, bool)> = cases
        .iter()
        .map(|args| {
            let (record, transcript) = parity(runtime.path(), FIXTURE, args);
            let in_process = routed(runtime.path(), FIXTURE, "KTSENSE_NO_DAEMON", args);
            transcripts.push(transcript);
            let names_the_filter = in_process.stdout.contains("under app/")
                || in_process.stdout.contains("\"filter\"");
            (record, names_the_filter || args.contains(&"--no-tests"))
        })
        .collect();

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);
    let matched = Parity {
        identical: true,
        daemon_code: Some(0),
        in_process_code: Some(0),
        both_silent: true,
        produced_output: true,
    };

    assert_eq!(
        (observed, stopped.code),
        (vec![(matched, true); cases.len()], Some(0)),
        "\n{}",
        transcripts.join("\n")
    );
}

/// KT-111: a `context --match` answer renders the declaration as one `<fqn>  <path>:<line>` location
/// line with no `## Declaration` heading, and the routed answer is byte-identical to the in-process
/// one in markdown and JSON. `--match` already crosses the wire (protocol 5), and KT-111 only changes
/// how the client renders the bundle, so this proves the routed shape is unchanged with no protocol
/// bump: the daemon and the in-process path render the same bundle the same way. `rebuild` is the
/// card's acceptance declaration; matching `break` keeps the one guard line.
#[cfg(feature = "real-lsp")]
#[test]
fn routed_matched_context_renders_a_location_line_identically_to_in_process() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(runtime.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let args = &[
        "context", "rebuild", "--only", "source", "--match", "break", "--around", "0",
    ];
    let json_args = &[
        "--format", "json", "context", "rebuild", "--only", "source", "--match", "break",
        "--around", "0",
    ];
    let md_via_daemon = routed(runtime.path(), FIXTURE, "KTSENSE_REQUIRE_DAEMON", args);
    let md_in_process = routed(runtime.path(), FIXTURE, "KTSENSE_NO_DAEMON", args);
    let json_via_daemon = routed(runtime.path(), FIXTURE, "KTSENSE_REQUIRE_DAEMON", json_args);
    let json_in_process = routed(runtime.path(), FIXTURE, "KTSENSE_NO_DAEMON", json_args);

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", FIXTURE]);

    assert_eq!(
        (
            md_via_daemon.stdout == md_in_process.stdout,
            json_via_daemon.stdout == json_in_process.stdout,
            md_via_daemon.code,
            md_via_daemon.stdout.lines().nth(1),
            md_via_daemon.stdout.contains("## Declaration"),
            md_via_daemon.stdout.contains("## Source"),
            md_via_daemon.stderr.is_empty() && md_in_process.stderr.is_empty(),
            stopped.code,
        ),
        (
            true,
            true,
            Some(0),
            Some(
                "shop.app.reporting.ReportBackfill.rebuild  \
                 app/src/main/kotlin/shop/app/reporting/ReportBackfill.kt:9"
            ),
            false,
            true,
            true,
            Some(0),
        ),
        "md via daemon:\n{}\nmd in-process:\n{}",
        md_via_daemon.stdout,
        md_in_process.stdout,
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
        ((run.code, found), run.stderr)
    };

    let (save_via_daemon, save_daemon_stderr) = locations(SYMBOL, "KTSENSE_REQUIRE_DAEMON");
    let (save_in_process, save_in_process_stderr) = locations(SYMBOL, "KTSENSE_NO_DAEMON");
    let (locals_via_daemon, locals_daemon_stderr) =
        locations(LOCAL_SYMBOL, "KTSENSE_REQUIRE_DAEMON");
    let (locals_in_process, locals_in_process_stderr) =
        locations(LOCAL_SYMBOL, "KTSENSE_NO_DAEMON");

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
        "save via daemon: {save_via_daemon:?} stderr: {}\n\
         save in-process: {save_in_process:?} stderr: {}\n\
         locals via daemon: {locals_via_daemon:?} stderr: {}\n\
         locals in-process: {locals_in_process:?} stderr: {}",
        save_daemon_stderr.trim(),
        save_in_process_stderr.trim(),
        locals_daemon_stderr.trim(),
        locals_in_process_stderr.trim()
    );
}

/// A name nothing declares must answer the same way on both paths. The warm index cannot distinguish
/// "absent" from "not indexed", so the routed path asks command-mode `find` before concluding
/// anything, and both paths then list where the name appears as text under an exit still 1 (KT-94).
/// The routed and in-process answers must be byte-identical on stdout as well as stderr.
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
            via_daemon.stderr.trim().is_empty(),
            via_daemon.stdout.contains(&format!(
                "no declaration named {ABSENT_SYMBOL} in this workspace"
            )),
            via_daemon
                .stdout
                .contains("## Text references (0 sites in 0 files)"),
            via_daemon.stdout == in_process.stdout,
            via_daemon.stderr == in_process.stderr,
            in_process.code,
            stopped.code,
        ),
        (Some(1), true, true, true, true, true, Some(1), Some(0),),
        "daemon stdout: {}\nin-process stdout: {}",
        via_daemon.stdout,
        in_process.stdout
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

/// KT-112: `trace` and `context` grow Java and Kotlin text-reference sections by rendering them into
/// the answer text the daemon already returns, so the routed wire shape is unchanged and no protocol
/// bump is needed. This proves it: over the mixed Java/Kotlin fixture, the routed and in-process
/// answers for a Java-declared symbol are byte-identical in markdown and JSON, for both `trace` and
/// `context`. `--wait-index` pins both `trace` paths to a complete index.
#[cfg(feature = "real-lsp")]
#[test]
fn routed_java_text_references_match_the_in_process_answer_byte_for_byte() {
    const MIXED_FIXTURE: &str = "fixtures/mixed-java";
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(
        runtime.path(),
        &["daemon", "start", "--root", MIXED_FIXTURE],
    );
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let cases: [Vec<&str>; 4] = [
        vec!["trace", "executeUpdate", "--wait-index"],
        vec!["--format", "json", "trace", "executeUpdate", "--wait-index"],
        vec!["context", "executeUpdate"],
        vec!["--format", "json", "context", "executeUpdate"],
    ];
    let (observed, transcripts): (Vec<Parity>, Vec<String>) = cases
        .iter()
        .map(|args| parity(runtime.path(), MIXED_FIXTURE, args))
        .unzip();

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", MIXED_FIXTURE]);

    let expected = Parity {
        identical: true,
        daemon_code: Some(0),
        in_process_code: Some(0),
        both_silent: true,
        produced_output: true,
    };
    assert_eq!(
        (observed, stopped.code),
        (vec![expected; cases.len()], Some(0)),
        "what each path said:\n{}",
        transcripts.join("\n")
    );
}

/// KT-115: a name nothing declares, in a workspace with `.java` sources, lists its Java sites in the
/// not-found answer. The daemon renders those into the answer text it already returns, so the routed
/// request and reply shapes are unchanged and no protocol bump is needed. This proves it: over the
/// mixed Java/Kotlin fixture the routed and in-process answers for an undeclared name are
/// byte-identical in markdown and JSON, for both `trace` and `context`, each exiting 1, and the
/// daemon answer actually carries the Java section so the parity is not a comparison of two bare
/// failures. `--wait-index` pins both `trace` paths to a complete index.
#[cfg(feature = "real-lsp")]
#[test]
fn a_routed_not_found_with_java_sites_matches_the_in_process_answer_byte_for_byte() {
    const MIXED_FIXTURE: &str = "fixtures/mixed-java";
    let runtime = tempfile::tempdir().expect("runtime dir");
    let started = daemon(
        runtime.path(),
        &["daemon", "start", "--root", MIXED_FIXTURE],
    );
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let cases: [Vec<&str>; 4] = [
        vec!["trace", "setStagingEnabled", "--wait-index"],
        vec![
            "--format",
            "json",
            "trace",
            "setStagingEnabled",
            "--wait-index",
        ],
        vec!["context", "setStagingEnabled"],
        vec!["--format", "json", "context", "setStagingEnabled"],
    ];
    let (observed, transcripts): (Vec<Parity>, Vec<String>) = cases
        .iter()
        .map(|args| parity(runtime.path(), MIXED_FIXTURE, args))
        .unzip();
    let via_daemon = routed(
        runtime.path(),
        MIXED_FIXTURE,
        "KTSENSE_REQUIRE_DAEMON",
        &cases[0],
    );

    let stopped = daemon(runtime.path(), &["daemon", "stop", "--root", MIXED_FIXTURE]);

    let expected = Parity {
        identical: true,
        daemon_code: Some(1),
        in_process_code: Some(1),
        both_silent: true,
        produced_output: true,
    };
    assert_eq!(
        (
            observed,
            via_daemon.stdout.contains("## Java text references"),
            stopped.code,
        ),
        (vec![expected; cases.len()], true, Some(0)),
        "what each path said:\n{}",
        transcripts.join("\n")
    );
}
