//! Lifecycle tests for `daemon start|stop|status`.
//!
//! Every invocation gets its own `XDG_RUNTIME_DIR`, passed per command rather than set on this
//! process, so the tests cannot disturb a developer's running daemon, cannot collide with each other,
//! and need no global environment mutation. The idle window is squeezed to seconds so a daemon left
//! behind by a failed assertion expires instead of lingering for an hour.
//!
//! The cases that need no engine at all, and the lifecycle cases that can drive the `fake_lsp` replay
//! engine, run in the default suite. Cases whose subject is the engine's own answers need the real
//! upstream engine and are gated behind `real-lsp`, matching how the other engine-dependent tests
//! are gated.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

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

/// The replay engine, built alongside `ktsense` into the same target directory. A daemon needs an
/// engine before it will bind its socket, and the replay engine is one, so the lifecycle itself can
/// be exercised without an upstream install.
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

/// A script that answers the handshake and then waits for teardown, which is what a warm session
/// looks like from the daemon's side. The `exit` step leaves the replay engine draining its stdin,
/// because kmp-lsp 0.26.0 ends on stdin EOF rather than on `exit` (see AGENTS.md).
const WARM_SESSION: &str = r#"{"steps":[
  {"kind":"expect","method":"initialize","respond":{"result":{"capabilities":{}}}},
  {"kind":"expect","method":"initialized"},
  {"kind":"expect","method":"shutdown","respond":{"result":null}},
  {"kind":"expect","method":"exit"}
]}"#;

/// How long a lifecycle test waits for an observation to settle, and how often it looks. A fixed
/// sleep would pass on an idle host and flake on one running several builds at once.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
const SETTLE_INTERVAL: Duration = Duration::from_millis(20);

/// Polls `observe` until `settled` accepts what it saw or the deadline passes, returning the last
/// observation either way so a failure reports the state that was actually reached.
fn settle<T>(mut observe: impl FnMut() -> T, settled: impl Fn(&T) -> bool) -> T {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let observed = observe();
        if settled(&observed) || Instant::now() >= deadline {
            return observed;
        }
        std::thread::sleep(SETTLE_INTERVAL);
    }
}

/// One daemon and everything it touches, under a single temporary directory: a copy of the fixture as
/// its workspace root, a symlink to the replay engine, and its own runtime directory.
///
/// That shared prefix is load-bearing rather than tidiness. The daemon carries `--root <home>/root`
/// in its argv and its engine child is executed through `<home>/engine`, so one substring names both
/// of this test's processes and nothing else on a host running several test binaries at once. Without
/// it, every daemon in the process table shares one argv and no test could tell its own from another's.
///
/// Dropping the harness stops the daemon, so a test that fails part-way cannot leave a live daemon
/// behind to make the next test's start or stale-socket assertion lie.
struct Lifecycle {
    home: tempfile::TempDir,
}

impl Lifecycle {
    fn new() -> Self {
        let lifecycle = Self {
            home: tempfile::tempdir().expect("temp home"),
        };
        std::fs::create_dir_all(lifecycle.root()).expect("create the workspace root");
        copy_tree(Path::new(FIXTURE_ROOT), &lifecycle.root());
        std::os::unix::fs::symlink(fake_lsp(), lifecycle.engine()).expect("link the engine");
        std::fs::create_dir_all(lifecycle.runtime()).expect("create the runtime dir");
        lifecycle
    }

    fn root(&self) -> PathBuf {
        self.home.path().join("root")
    }

    fn engine(&self) -> PathBuf {
        self.home.path().join("engine")
    }

    fn runtime(&self) -> PathBuf {
        self.home.path().join("run")
    }

    /// The file a routed request asks about, inside this harness's own copy of the fixture, so no
    /// test touches the checked-in tree.
    fn source_file(&self) -> PathBuf {
        self.root()
            .join("core/src/main/kotlin/shop/order/OrderRepository.kt")
    }

    fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Run {
        let mut command = Command::cargo_bin("ktsense").expect("binary builds");
        command
            .current_dir(WORKSPACE_ROOT)
            .env("XDG_RUNTIME_DIR", self.runtime())
            .env("KTSENSE_LSP_PATH", self.engine())
            .env("FAKE_LSP_SCRIPT", WARM_SESSION)
            .env("KTSENSE_DAEMON_IDLE_SECS", "20")
            .args(["--root", self.root().to_str().expect("utf-8 root")]);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        let output = command.args(args).output().expect("binary runs");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }

    fn act(&self, action: &str) -> Run {
        self.run(&["daemon", action], &[])
    }

    /// Where this harness's daemon listens, taken from what the CLI reports rather than recomputed,
    /// so a test cannot drift from the daemon's own socket naming.
    fn socket(&self) -> PathBuf {
        let socket = PathBuf::from(self.act("status").socket());
        std::fs::create_dir_all(socket.parent().expect("socket has a parent"))
            .expect("create the socket dir");
        socket
    }

    /// Runs one routed `outline` and reports the daemon's served count afterwards. A daemon answers
    /// `outline` from its own parsed-skeleton cache rather than through the engine, so this is real
    /// work done by this daemon, and `KTSENSE_REQUIRE_DAEMON=1` forbids the in-process fallback so a
    /// request that never reached the daemon fails instead of counting for nothing.
    fn serve_one_request(&self) -> (Option<i32>, Option<u64>) {
        let answered = self.run(
            &[
                "outline",
                self.source_file().to_str().expect("utf-8 source file"),
            ],
            &[("KTSENSE_REQUIRE_DAEMON", "1")],
        );
        (answered.code, self.requests_served())
    }

    /// The daemon's own count of the work it has done. A daemon that was replaced rather than reused
    /// counts from zero again, which is what makes this an identity and not merely a number.
    fn requests_served(&self) -> Option<u64> {
        self.act("status")
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("requests served: "))
            .and_then(|count| count.parse().ok())
    }

    /// How many daemons and how many engine children this harness owns, read from the process table.
    /// `ps` is asked for untruncated arguments because the identifying path is long.
    fn population(&self) -> (usize, usize) {
        let listing = Command::new("ps")
            .args(["-e", "-ww", "-o", "args="])
            .output()
            .expect("ps runs");
        let prefix = self.home.path().display().to_string();
        let engine = self.engine().display().to_string();
        let mine: Vec<String> = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.contains(&prefix))
            .map(str::to_string)
            .collect();
        (
            mine.iter()
                .filter(|line| line.ends_with("daemon serve"))
                .count(),
            mine.iter().filter(|line| line.trim() == engine).count(),
        )
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        let _ = self.act("stop");
    }
}

/// Whether connecting to `socket` is refused, which is what makes a leftover stale rather than live:
/// the file is there and nothing is listening behind it.
fn refused(socket: &Path) -> bool {
    matches!(
        std::os::unix::net::UnixStream::connect(socket),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused
    )
}

/// Whether both paths name one existing file, by device and inode.
fn same_file(left: &Path, right: &Path) -> bool {
    let identity = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| (meta.dev(), meta.ino()))
            .ok()
    };
    identity(left).is_some() && identity(left) == identity(right)
}

fn link_count(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|meta| meta.nlink())
        .unwrap_or_default()
}

/// KT-30: a socket left behind by a dead daemon is unlinked and rebound, not mistaken for a live one.
///
/// The stale condition is created for real rather than simulated: a socket is bound and then
/// abandoned, so the file exists and connecting to it is refused. A hard link planted beside it
/// witnesses the unlink, because an inode number cannot: the kernel reuses inode numbers, and this
/// path was measured handing the freed number straight back to the new socket. The link keeps the
/// dead socket alive as a separate name, so after the start the two paths naming one file becomes two
/// paths naming two, the witness drops to a single link, and the witness still refuses a connection
/// while the socket path accepts one.
#[test]
fn a_stale_socket_is_unlinked_and_the_daemon_restarts_on_it() {
    let lifecycle = Lifecycle::new();
    let socket = lifecycle.socket();
    let witness = socket.with_extension("witness");
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind then abandon a socket"));
    std::fs::hard_link(&socket, &witness).expect("witness the planted socket");
    let planted = (
        socket.exists(),
        refused(&socket),
        same_file(&socket, &witness),
        link_count(&witness),
        lifecycle
            .act("status")
            .stdout
            .contains("is a stale leftover"),
    );

    let started = lifecycle.act("start");
    let live = lifecycle.act("status");

    assert_eq!(
        (
            planted,
            started.code,
            (
                same_file(&socket, &witness),
                link_count(&witness),
                refused(&witness),
                refused(&socket),
                socket.exists(),
            ),
            live.stdout.contains("daemon running for"),
            lifecycle.population(),
        ),
        (
            (true, true, true, 2, true),
            Some(0),
            (false, 1, true, false, true),
            true,
            (1, 1),
        ),
        "start said: {}{}",
        started.stdout,
        started.stderr
    );
}

/// KT-30: a second start reuses the running daemon instead of racing a duplicate.
///
/// A success code would not show that, so the outcome is pinned from four independent facts. The
/// socket path still names the same file, and the daemon's served count carries on from where the
/// first daemon left it rather than restarting at zero, which together rule out a second daemon
/// having taken over. The process table holds exactly one daemon and one engine child, sampled both
/// straight after the second start and after the next request, which rules out a duplicate that
/// lingers. And the reported outcome is "already running" rather than "started", which is the fact
/// that distinguishes a start the CLI recognised as unnecessary from one it attempted anyway: a
/// duplicate spawn is also refused by the daemon's own bind, so without this the wasted spawn would
/// leave no trace in the state the other three facts observe.
#[test]
fn a_second_start_reuses_the_running_daemon_rather_than_adding_one() {
    let lifecycle = Lifecycle::new();
    let started = lifecycle.act("start");
    let socket = lifecycle.socket();
    let first_served = lifecycle.serve_one_request();
    let before = lifecycle.population();

    let again = lifecycle.act("start");
    let immediately_after = lifecycle.population();
    let second_served = lifecycle.serve_one_request();

    assert_eq!(
        (
            started.code,
            first_served,
            before,
            (
                again.code,
                again.stdout.contains("already running"),
                again.stderr.clone(),
            ),
            immediately_after,
            same_file(&socket, &lifecycle.socket()),
            second_served,
            lifecycle.population(),
        ),
        (
            Some(0),
            (Some(0), Some(1)),
            (1, 1),
            (Some(0), true, String::new()),
            (1, 1),
            true,
            (Some(0), Some(2)),
            (1, 1),
        ),
        "start said: {}{}\nsecond start said: {}{}",
        started.stdout,
        started.stderr,
        again.stdout,
        again.stderr
    );
}

/// KT-30: stopping twice is quiet, and the first stop leaves nothing behind.
///
/// The negative is asserted with the positive. After the second stop the socket file is gone, status
/// reports absence rather than the staleness a leaked socket would show, and neither the daemon nor
/// its engine child is in the process table. Both stops are required to print nothing on stderr,
/// which is what "quiet" means for a command an agent parses. The process count is polled to a
/// deadline rather than slept on, because a loaded host takes longer to reap a child than an idle one.
#[test]
fn stopping_twice_is_quiet_and_leaves_no_process_or_socket_behind() {
    let lifecycle = Lifecycle::new();
    let started = lifecycle.act("start");
    let socket = lifecycle.socket();
    let running = (started.code, socket.exists(), lifecycle.population());

    let stopped = lifecycle.act("stop");
    let again = lifecycle.act("stop");
    let remaining = settle(
        || lifecycle.population(),
        |population| population == &(0, 0),
    );
    let after = lifecycle.act("status");

    assert_eq!(
        (
            running,
            (
                stopped.code,
                stopped.stdout.contains("daemon stopped for"),
                stopped.stderr.clone(),
            ),
            (
                again.code,
                again.stdout.contains("no daemon was running"),
                again.stderr.clone(),
            ),
            remaining,
            socket.exists(),
            (
                after.stdout.contains("does not exist"),
                after.stdout.contains("stale leftover"),
            ),
        ),
        (
            (Some(0), true, (1, 1)),
            (Some(0), true, String::new()),
            (Some(0), true, String::new()),
            (0, 0),
            false,
            (true, false),
        ),
        "start said: {}{}\nfirst stop said: {}\nsecond stop said: {}",
        started.stdout,
        started.stderr,
        stopped.stdout,
        again.stdout
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

/// KT-29: with a live daemon, `outline` and `deps` answered through the socket are byte-identical
/// to the in-process answers, in both formats. `KTSENSE_REQUIRE_DAEMON=1` forbids the fallback so
/// the daemon path is proven to have answered, and `KTSENSE_NO_DAEMON=1` forces the in-process
/// path; without both knobs the equality could pass with the daemon never consulted.
#[cfg(feature = "real-lsp")]
#[test]
fn routed_outline_and_deps_match_the_in_process_answers_byte_for_byte() {
    let temp = tempfile::tempdir().expect("temp dir");
    let started = daemon(temp.path(), &["daemon", "start", "--root", FIXTURE]);
    assert_eq!(started.code, Some(0), "start failed: {}", started.stderr);

    let cases: [&[&str]; 4] = [
        &[
            "outline",
            "core/src/main/kotlin/shop/order/OrderRepository.kt",
        ],
        &[
            "--format",
            "json",
            "outline",
            "app/src/main/kotlin/shop/app/checkout/CheckoutService.kt",
            "--private",
        ],
        &["deps"],
        &["--format", "dot", "deps", "--level", "file"],
    ];
    let mut answers = Vec::new();
    for case in cases {
        let via_daemon = routed(temp.path(), case, "KTSENSE_REQUIRE_DAEMON");
        let in_process = routed(temp.path(), case, "KTSENSE_NO_DAEMON");
        answers.push((
            via_daemon.code,
            in_process.code,
            via_daemon.stdout == in_process.stdout,
            via_daemon.stderr.is_empty() && in_process.stderr.is_empty(),
            !via_daemon.stdout.is_empty(),
        ));
    }
    let unreachable = routed(
        temp.path(),
        &[
            "--root",
            "fixtures/tiny-app",
            "outline",
            "src/main/kotlin/app/service/Service.kt",
        ],
        "KTSENSE_REQUIRE_DAEMON",
    );
    let stopped = daemon(temp.path(), &["daemon", "stop", "--root", FIXTURE]);

    assert_eq!(
        (
            answers,
            unreachable.code,
            unreachable.stderr.contains("no daemon answered"),
            stopped.code,
        ),
        (
            vec![(Some(0), Some(0), true, true, true); 4],
            Some(1),
            true,
            Some(0),
        ),
        "unreachable stderr: {}",
        unreachable.stderr
    );
}

#[cfg(feature = "real-lsp")]
fn routed(runtime_dir: &Path, args: &[&str], knob: &str) -> Run {
    let mut command = Command::cargo_bin("ktsense").expect("binary builds");
    command
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env(knob, "1");
    if !args.contains(&"--root") {
        command.args(["--root", FIXTURE]);
    }
    let output = command.args(args).output().expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// KT-28: a warm daemon caches parsed skeletons, so it must still see a source edit. An edit to a
/// file the daemon has already outlined is reflected in the next routed outline, and within the
/// two-second budget the card fixes. `KTSENSE_REQUIRE_DAEMON=1` forbids the in-process fallback so
/// the edit is proven to have travelled through the live socket, not around it. The fixture is
/// copied to a scratch workspace first, so the edit never touches the checked-in tree.
#[cfg(feature = "real-lsp")]
const REFRESH_BUDGET: Duration = Duration::from_secs(2);

#[cfg(feature = "real-lsp")]
#[test]
fn an_edited_fixture_is_reflected_in_the_daemon_outline_within_the_refresh_budget() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let workspace = tempfile::tempdir().expect("workspace copy");
    copy_tree(Path::new(FIXTURE_ROOT), workspace.path());
    let root = workspace.path();
    let target = root.join("core/src/main/kotlin/shop/order/OrderRepository.kt");

    let started = daemon_rooted(runtime.path(), root, &["daemon", "start"]);
    let before = outline_via_daemon(runtime.path(), root, &target);

    std::fs::write(&target, EDITED_ORDER_REPOSITORY).expect("edit the copied fixture");
    let observed_at = Instant::now();
    let after = outline_via_daemon(runtime.path(), root, &target);
    let elapsed = observed_at.elapsed();

    let stopped = daemon_rooted(runtime.path(), root, &["daemon", "stop"]);

    assert_eq!(
        (
            started.code,
            (before.code, before.stdout.contains("purge")),
            (
                after.code,
                after.stdout.contains("purge"),
                after.stderr.is_empty()
            ),
            elapsed <= REFRESH_BUDGET,
            stopped.code,
        ),
        (
            Some(0),
            (Some(0), false),
            (Some(0), true, true),
            true,
            Some(0),
        ),
        "start: {} / after edit: {}",
        started.stderr,
        after.stderr,
    );
}

const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/multi-module");

#[cfg(feature = "real-lsp")]
const EDITED_ORDER_REPOSITORY: &str = "package shop.order\n\ninterface OrderRepository {\n    fun save(order: Order): OrderId\n\n    fun findById(id: OrderId): Order?\n\n    fun purge(id: OrderId)\n}\n";

#[cfg(feature = "real-lsp")]
fn daemon_rooted(runtime_dir: &Path, root: &Path, args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("KTSENSE_DAEMON_IDLE_SECS", "20")
        .args(args)
        .args(["--root", root.to_str().expect("utf-8 root")])
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

#[cfg(feature = "real-lsp")]
fn outline_via_daemon(runtime_dir: &Path, root: &Path, target: &Path) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("KTSENSE_REQUIRE_DAEMON", "1")
        .args([
            "--root",
            root.to_str().expect("utf-8 root"),
            "outline",
            target.to_str().expect("utf-8 target"),
        ])
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&destination).expect("create dir");
            copy_tree(&source, &destination);
        } else {
            std::fs::copy(&source, &destination).expect("copy file");
        }
    }
}
