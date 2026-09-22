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
//!
//! Two lifecycle proofs need a mechanism this repository has only verified on Linux: a process census
//! through `ps -e -ww -o args=`, and a hard link to a socket file, which POSIX leaves
//! implementation-defined for a non-regular file. Those live in tests whose names begin `on_linux_`
//! and carry `#[cfg(target_os = "linux")]`, so the platform boundary is visible in the test list
//! rather than buried in a helper. CI runs ubuntu and macos-14; a green macOS run has made the
//! portable lifecycle proofs and not those two.

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
        let output = self
            .command(extra_env)
            .args(args)
            .output()
            .expect("binary runs");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }

    /// One `ktsense` invocation against this harness: its own runtime directory, its own replay
    /// engine, its own copy of the fixture as the root.
    fn command(&self, extra_env: &[(&str, &str)]) -> Command {
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
        command
    }

    /// Releases `parties` starts into one barrier and waits for every one of them. Each participant
    /// records its arrival after its own liveness check and before it arbitrates, so all of them are
    /// inside the window the arbitration has to settle rather than merely launched close together.
    /// The arrival count is returned because it is what proves the barrier engaged at all: with the
    /// seam unset no participant records anything and the count is zero.
    fn race_to_start(&self, parties: usize, barrier: &Path) -> (Vec<Run>, usize) {
        let racing: Vec<_> = (0..parties)
            .map(|_| self.start_held_at(barrier, parties))
            .collect();
        let runs = racing.into_iter().map(finished).collect();
        (runs, arrivals(barrier))
    }

    /// Spawns one `daemon start` that records its arrival at `barrier` and waits there, after its own
    /// liveness check and before it arbitrates, until `parties` arrivals exist. A test that supplies
    /// one of those arrivals itself decides what the world looks like by the time this start gets to
    /// arbitrate.
    fn start_held_at(&self, barrier: &Path, parties: usize) -> std::process::Child {
        self.command(&[
            (
                "KTSENSE_START_BARRIER_DIR",
                barrier.to_str().expect("utf-8 barrier"),
            ),
            ("KTSENSE_START_BARRIER_PARTIES", &parties.to_string()),
        ])
        .args(["daemon", "start"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start spawns")
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
    ///
    /// Linux only: these `ps` flags are not verified on macOS, and a census that silently reported
    /// zero processes would turn every assertion resting on it into a pass.
    #[cfg(target_os = "linux")]
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

/// How many names the file at `path` has. Linux only, because the hard-link witness it serves relies
/// on `link(2)` accepting a socket, which POSIX leaves implementation-defined for a non-regular file.
#[cfg(target_os = "linux")]
fn link_count(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|meta| meta.nlink())
        .unwrap_or_default()
}

/// How many starts recorded their arrival at a barrier. Zero means the seam never engaged, which is
/// why a race test asserts this count rather than assuming the overlap it asked for.
fn arrivals(barrier: &Path) -> usize {
    std::fs::read_dir(barrier)
        .map(|entries| entries.count())
        .unwrap_or_default()
}

/// Waits for one spawned start and reads what it reported.
fn finished(racer: std::process::Child) -> Run {
    let output = racer.wait_with_output().expect("start finishes");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// Everything sharing the daemon's socket directory, with the socket's own name replaced by a label
/// so an expectation can name it without knowing the per-root hash. Anything a start left behind
/// still appears under its real name, so this is how "the losing attempt survives" would show up.
fn socket_dir_contents(socket: &Path) -> Vec<String> {
    let own = socket.file_name().expect("the socket has a name");
    let mut names: Vec<String> = std::fs::read_dir(socket.parent().expect("socket has a parent"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            if entry.file_name() == own {
                "the daemon socket".to_string()
            } else {
                entry.file_name().to_string_lossy().into_owned()
            }
        })
        .collect();
    names.sort();
    names
}

/// KT-30: a socket left behind by a dead daemon is unlinked and rebound, not mistaken for a live one.
///
/// Runs on every platform, and proves the unlink by consequence rather than by observation. The stale
/// condition is created for real rather than simulated: a socket is bound and then abandoned, so the
/// file exists and connecting to it is refused. `bind` refuses a path that already exists, so a daemon
/// answering on that same path afterwards cannot have got there without the file being removed first.
/// `on_linux_the_stale_socket_unlink_is_witnessed_by_a_hard_link` observes the removal directly.
///
/// Liveness is read from the anchored message prefix, never the bare phrase: "no daemon running for"
/// contains "daemon running for", so the negative is asserted beside the positive and neither message
/// can satisfy both.
#[test]
fn a_stale_socket_is_unlinked_and_the_daemon_restarts_on_it() {
    let lifecycle = Lifecycle::new();
    let socket = lifecycle.socket();
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind then abandon a socket"));
    let planted = lifecycle.act("status");
    let planted = (
        socket.exists(),
        refused(&socket),
        planted.stdout.contains("is a stale leftover"),
        planted.stdout.contains("ktsense: daemon running for"),
    );

    let started = lifecycle.act("start");
    let live = lifecycle.act("status");

    assert_eq!(
        (
            planted,
            started.code,
            (
                live.code,
                live.stdout.contains("ktsense: daemon running for"),
                live.stdout.contains("no daemon running"),
                live.stdout.contains("is a stale leftover"),
            ),
            (socket.exists(), refused(&socket)),
        ),
        (
            (true, true, true, false),
            Some(0),
            (Some(0), true, false, false),
            (true, false),
        ),
        "start said: {}{}",
        started.stdout,
        started.stderr
    );
}

/// KT-30, Linux only: the unlink itself, observed rather than inferred.
///
/// An inode number cannot show it. The kernel reuses inode numbers, and this path was measured handing
/// the freed number straight back to the replacement socket, so a before-and-after inode comparison
/// passes whether or not the file was ever removed. A hard link planted beside the socket keeps the
/// dead socket alive under a second name, which makes the removal directly observable: two paths naming
/// one file become two paths naming two, the witness drops to a single link, and the witness still
/// refuses a connection while the socket path accepts one.
///
/// Gated to Linux because `link(2)` on a socket is implementation-defined for non-regular files. It
/// works here; it is not claimed for macOS, and no macOS run should be read as having made this proof.
#[cfg(target_os = "linux")]
#[test]
fn on_linux_the_stale_socket_unlink_is_witnessed_by_a_hard_link() {
    let lifecycle = Lifecycle::new();
    let socket = lifecycle.socket();
    let witness = socket.with_extension("witness");
    drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind then abandon a socket"));
    std::fs::hard_link(&socket, &witness).expect("witness the planted socket");
    let planted = (same_file(&socket, &witness), link_count(&witness));

    let started = lifecycle.act("start");

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
        ),
        ((true, 2), Some(0), (false, 1, true, false, true),),
        "start said: {}{}",
        started.stdout,
        started.stderr
    );
}

/// KT-30: a second start reuses the running daemon instead of racing a duplicate.
///
/// Runs on every platform. A success code would not show reuse, so the outcome is pinned from three
/// portable facts. The socket path still names the same file, by device and inode. The daemon's served
/// count carries on from where the first daemon left it rather than restarting at zero, which a
/// replacement daemon could not do. And the reported outcome is "already running" rather than
/// "started", which is the fact that distinguishes a start the CLI recognised as unnecessary from one
/// it attempted anyway: a duplicate spawn is also refused by the daemon's own bind, so without this the
/// wasted spawn would leave no trace in the state the other two facts observe.
///
/// `on_linux_a_reused_start_adds_no_process_and_a_stop_empties_the_table` adds the process census that
/// rules out a duplicate which lingers.
#[test]
fn a_second_start_reuses_the_running_daemon_rather_than_adding_one() {
    let lifecycle = Lifecycle::new();
    let started = lifecycle.act("start");
    let socket = lifecycle.socket();
    let first_served = lifecycle.serve_one_request();

    let again = lifecycle.act("start");
    let second_served = lifecycle.serve_one_request();

    assert_eq!(
        (
            started.code,
            first_served,
            (
                again.code,
                again.stdout.contains("already running"),
                again.stdout.contains("daemon started for"),
                again.stderr.clone(),
            ),
            same_file(&socket, &lifecycle.socket()),
            second_served,
        ),
        (
            Some(0),
            (Some(0), Some(1)),
            (Some(0), true, false, String::new()),
            true,
            (Some(0), Some(2)),
        ),
        "start said: {}{}\nsecond start said: {}{}",
        started.stdout,
        started.stderr,
        again.stdout,
        again.stderr
    );
}

/// KT-30: stopping twice is quiet, and the first stop leaves no socket behind.
///
/// Runs on every platform. The name says socket rather than process deliberately: proving no process
/// remains needs the process table, which is
/// `on_linux_a_reused_start_adds_no_process_and_a_stop_empties_the_table`, so a green run of this test
/// on a platform without that one has not made that proof. The negative is asserted with the positive:
/// after the second stop the socket file is gone and status reports absence rather than the staleness a
/// leaked socket would show. Both stops must print nothing on stderr, which is what "quiet" means for a
/// command an agent parses. Socket absence is polled to a deadline rather than asserted outright,
/// because `stop` bounds its own wait and gives up silently, so a loaded host can outlast it.
#[test]
fn stopping_twice_is_quiet_and_leaves_no_socket_behind() {
    let lifecycle = Lifecycle::new();
    let started = lifecycle.act("start");
    let socket = lifecycle.socket();
    let running = (started.code, socket.exists());

    let stopped = lifecycle.act("stop");
    let again = lifecycle.act("stop");
    let gone = settle(|| !socket.exists(), |absent| *absent);
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
            gone,
            (
                after.stdout.contains("does not exist"),
                after.stdout.contains("is a stale leftover"),
                after.stdout.contains("ktsense: daemon running for"),
            ),
        ),
        (
            (Some(0), true),
            (Some(0), true, String::new()),
            (Some(0), true, String::new()),
            true,
            (true, false, false),
        ),
        "start said: {}{}\nfirst stop said: {}\nsecond stop said: {}",
        started.stdout,
        started.stderr,
        stopped.stdout,
        again.stdout
    );
}

/// KT-30, Linux only: the process table, which is the only place "no second daemon" and "no lingering
/// child" can actually be read.
///
/// Both halves live in one test because both need the same census. A second start must add neither a
/// daemon nor an engine child, sampled straight after it returns and again after the next request; a
/// stop must empty the table of both. The census is polled to a deadline rather than slept on, because
/// a loaded host takes longer to reap a child than an idle one.
///
/// Gated to Linux because the census shells out to `ps -e -ww -o args=` and the behaviour of those
/// flags is not verified on macOS. No macOS run should be read as having made this proof.
#[cfg(target_os = "linux")]
#[test]
fn on_linux_a_reused_start_adds_no_process_and_a_stop_empties_the_table() {
    let lifecycle = Lifecycle::new();
    let started = lifecycle.act("start");
    let before = lifecycle.population();

    let again = lifecycle.act("start");
    let immediately_after = lifecycle.population();
    let served = lifecycle.serve_one_request();
    let after_request = lifecycle.population();

    let stopped = lifecycle.act("stop");
    let remaining = settle(|| lifecycle.population(), |census| census == &(0, 0));

    assert_eq!(
        (
            started.code,
            before,
            (again.code, immediately_after),
            (served.0, after_request),
            stopped.code,
            remaining,
        ),
        (
            Some(0),
            (1, 1),
            (Some(0), (1, 1)),
            (Some(0), (1, 1)),
            Some(0),
            (0, 0),
        ),
        "start said: {}{}\nsecond start said: {}",
        started.stdout,
        started.stderr,
        again.stdout
    );
}

/// How many times the concurrent-start race runs inside one test. A race is settled by a mechanism,
/// not by luck, but one pass still shows less than a handful; the count stays small so the default
/// suite remains quick on a host shared with other builds. Heavier repetition belongs in a load run.
const RACES: usize = 3;

/// What one race is judged on. Named fields rather than a row of anonymous values, because the
/// expectation below is a claim about behaviour and has to read as one.
#[derive(Debug, PartialEq, Eq)]
struct Race {
    arrived_together: usize,
    said_it_started_the_daemon: usize,
    said_one_was_already_running: usize,
    exits: Vec<Option<i32>>,
    quiet: bool,
    socket_answered: bool,
    beside_the_socket: Vec<String>,
    stopped: Option<i32>,
    left_behind: Vec<String>,
}

/// KT-61: two starts released together settle to one daemon, and each is told the truth about which
/// of them produced it.
///
/// The overlap is forced rather than hoped for. Both processes record their arrival at a barrier after
/// their own liveness check and before either arbitrates, so neither can leave that point until both
/// have reached it and both are provably inside the window the arbitration must settle. The arrival
/// count is asserted because it is the only thing that distinguishes this from two launches that
/// missed each other: with the seam inert it is zero.
///
/// Outcomes are counted from anchored messages across both results, so exactly one start claims the
/// daemon and exactly one reports the winner's. A loser that watched the winner's socket come up and
/// called that its own work fails here, and so does a loser that reports a bind failure instead of a
/// success. The socket directory holding nothing but the daemon socket is what proves the losing
/// attempt left no claim behind, and holding nothing at all after the stop is what proves the winner
/// released its own.
#[test]
fn concurrent_starts_settle_to_one_daemon_and_exactly_one_start_owns_it() {
    let (observed, transcripts): (Vec<Race>, Vec<String>) = (0..RACES)
        .map(|race| {
            let lifecycle = Lifecycle::new();
            let barrier = lifecycle.home.path().join(format!("barrier{race}"));

            let (runs, arrived_together) = lifecycle.race_to_start(2, &barrier);
            let socket = lifecycle.socket();
            let while_running = (
                socket.exists() && !refused(&socket),
                socket_dir_contents(&socket),
            );

            let stopped = lifecycle.act("stop");
            let emptied = settle(
                || socket_dir_contents(&socket),
                |contents| contents.is_empty(),
            );
            let transcript = transcript(race, &runs);
            (
                Race {
                    arrived_together,
                    said_it_started_the_daemon: counted(&runs, "ktsense: daemon started for"),
                    said_one_was_already_running: counted(&runs, "a daemon is already running for"),
                    exits: runs.iter().map(|run| run.code).collect(),
                    quiet: runs.iter().all(|run| run.stderr.is_empty()),
                    socket_answered: while_running.0,
                    beside_the_socket: while_running.1,
                    stopped: stopped.code,
                    left_behind: emptied,
                },
                transcript,
            )
        })
        .unzip();

    assert_eq!(
        observed,
        (0..RACES)
            .map(|_| Race {
                arrived_together: 2,
                said_it_started_the_daemon: 1,
                said_one_was_already_running: 1,
                exits: vec![Some(0), Some(0)],
                quiet: true,
                socket_answered: true,
                beside_the_socket: vec!["the daemon socket".to_string()],
                stopped: Some(0),
                left_behind: Vec::new(),
            })
            .collect::<Vec<_>>(),
        "what the starts said:\n{}",
        transcripts.join("\n")
    );
}

/// Everything both starts of one race wrote, which is the only place a start that failed names the
/// reason. Without it the record above says a start exited 1 and stops there, and a platform that
/// refuses a contended claim with an unexpected errno cannot be told from one that could not spawn.
fn transcript(race: usize, runs: &[Run]) -> String {
    runs.iter()
        .enumerate()
        .map(|(start, run)| {
            format!(
                "  race {race} start {start}: exit {:?}\n    stdout: {}\n    stderr: {}",
                run.code,
                run.stdout.trim(),
                run.stderr.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn counted(runs: &[Run], message: &str) -> usize {
    runs.iter()
        .filter(|run| run.stdout.contains(message))
        .count()
}

/// KT-61: a start that crossed its own liveness check before any daemon existed still reports the
/// daemon it finds rather than claiming it, when it only gets to arbitrate after another start has
/// already finished.
///
/// This interleaving is neither of the two the other tests cover. KT-30's second start sees a live
/// socket at its own liveness check and returns there, and a racing start contends for a claim the
/// winner is holding. Here the held start passes its liveness check with nothing running, and by the
/// time it is released the winner has bound the socket and released the claim, so nothing stands in
/// the way of taking a claim and spawning a second engine. What stops it is that arbitration re-reads
/// liveness rather than trusting the check the start made before waiting.
///
/// The barrier is filled by the test itself: the held start is one arrival of two, and the test
/// supplies the second once the winner is up. The arrival counted before the winner ran is asserted,
/// because it is what proves the held start really was past its liveness check by then, which is the
/// whole point of the ordering.
#[test]
fn a_start_that_arbitrates_after_the_winner_finished_reports_the_winner_rather_than_itself() {
    let lifecycle = Lifecycle::new();
    let barrier = lifecycle.home.path().join("barrier");

    let held = lifecycle.start_held_at(&barrier, 2);
    let waiting_before_any_daemon = settle(|| arrivals(&barrier), |arrived| *arrived >= 1);
    let winner = lifecycle.act("start");
    std::fs::write(barrier.join("released-by-the-test"), b"").expect("release the held start");
    let late = finished(held);
    let socket = lifecycle.socket();

    assert_eq!(
        (
            waiting_before_any_daemon,
            (
                winner.code,
                winner.stdout.contains("ktsense: daemon started for")
            ),
            (
                late.code,
                late.stdout.contains("a daemon is already running for"),
                late.stdout.contains("ktsense: daemon started for"),
                late.stderr.clone(),
            ),
            socket.exists() && !refused(&socket),
            socket_dir_contents(&socket),
        ),
        (
            1,
            (Some(0), true),
            (Some(0), true, false, String::new()),
            true,
            vec!["the daemon socket".to_string()],
        ),
        "the winner said: {}{}\nthe late start said: {}{}",
        winner.stdout,
        winner.stderr,
        late.stdout,
        late.stderr
    );
}

/// KT-61, Linux only: the race costs one daemon and one engine child, not two of either.
///
/// This is the half that cannot be read from messages. A losing start that spawned its own engine
/// before discovering it had lost would show up here as a second child, whether or not it was ever
/// reaped, and the census is taken as soon as both starts return rather than settled towards the
/// expected answer, so a duplicate that dies quickly is still caught. The stop then has to empty the
/// table of both, which is polled, because a loaded host takes longer to reap a child.
///
/// Gated to Linux for the same reason as the other census test: `ps -e -ww -o args=` is not verified
/// on macOS, and no macOS run should be read as having made this proof.
#[cfg(target_os = "linux")]
#[test]
fn on_linux_concurrent_starts_leave_one_daemon_and_one_engine_child() {
    let lifecycle = Lifecycle::new();
    let barrier = lifecycle.home.path().join("barrier");

    let (runs, arrived_together) = lifecycle.race_to_start(2, &barrier);
    let population = lifecycle.population();
    let served = lifecycle.serve_one_request();
    let after_a_request = lifecycle.population();

    let stopped = lifecycle.act("stop");
    let remaining = settle(|| lifecycle.population(), |census| census == &(0, 0));

    assert_eq!(
        (
            arrived_together,
            counted(&runs, "ktsense: daemon started for"),
            counted(&runs, "a daemon is already running for"),
            population,
            served,
            after_a_request,
            stopped.code,
            remaining,
        ),
        (2, 1, 1, (1, 1), (Some(0), Some(1)), (1, 1), Some(0), (0, 0)),
        "the racing starts said: {}",
        runs.iter()
            .map(|run| format!("[{}{}]", run.stdout, run.stderr))
            .collect::<Vec<_>>()
            .join(" ")
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
