//! The `daemon start|stop|status` lifecycle surface over the daemon crate's library API.
//!
//! `start` does not fork. It re-executes this binary's hidden `daemon serve` subcommand as a
//! detached child, which keeps the CLI free of unsafe process manipulation and behaves identically on
//! Linux and macOS. The parent then waits for the socket to answer, so a successful `start` reports a
//! daemon that is actually reachable rather than merely a process that was spawned.
//!
//! Every outcome is reported in the text, not only in the exit code. An agent that asked for a daemon
//! and got one already running needs to be told that, because "already running" and "just started"
//! lead to different next steps even though both are successes. Two starts released together are
//! settled by a claim taken before either spawns an engine, so exactly one of them owns the attempt
//! and each is told which one it was.

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ktsense_daemon::{
    run, socket_dir, socket_path, status, stop, DaemonConfig, Liveness, StopOutcome, StopReason,
    WarmEngine, DEFAULT_IDLE_TIMEOUT,
};
use ktsense_lsp::InitializeConfig;

use crate::{block_on, CommandError, CommandOutcome, Exit};

/// How long `start` waits for the spawned daemon to answer before reporting failure. The engine
/// handshake happens before the socket is bound, so this has to tolerate a cold engine launch.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const START_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Overrides the idle window, in seconds. A test needs a daemon that expires in moments rather than
/// an hour, so that a daemon left behind by a failed assertion cannot outlive the run.
const IDLE_SECS_ENV: &str = "KTSENSE_DAEMON_IDLE_SECS";

/// Owner-only, the modes the daemon crate sets on this same directory and its own socket. A start
/// claim sits beside that socket and must not widen what reaches it.
const CLAIM_DIR_MODE: u32 = 0o700;
const CLAIM_MODE: u32 = 0o600;

/// How many abandoned claims one root tolerates before a start refuses rather than walking on. One
/// is left behind per claimant that died without releasing, so reaching this means something is
/// killing starts repeatedly and a truthful failure beats an unbounded search.
const MAX_CLAIM_GENERATIONS: u32 = 64;

/// Reports whether a daemon is running for `root`, and where its socket is.
pub(crate) fn report_status(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root);
    let daemon = block_on(crate::status::observe_daemon(&socket));
    let headline = match daemon.state {
        crate::status::DaemonState::Running => "daemon running for",
        _ => "no daemon running for",
    };
    Ok(CommandOutcome::success(format!(
        "ktsense: {headline} {}\n{}",
        root.display(),
        crate::status::daemon_lines(&daemon)
    )))
}

/// Starts a daemon for `root`, or reports that one is already running. Idempotent by design: asking
/// for something that already exists is a success, and saying so is more useful than an error.
///
/// The liveness check below cannot decide a race, because two starts released together both pass it.
/// A claim decides, and it is taken before the engine child is spawned rather than after: the loser
/// reports the winner's daemon without having paid for an engine of its own, so there is no losing
/// child to orphan and no second bind to lose. Attribution follows from the claim rather than from
/// the socket, since a socket that came up while another start held the claim is that start's work.
pub(crate) fn start(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root);
    if live(&socket) {
        return Ok(already_running(&root, &socket));
    }
    rendezvous();
    match claim_the_start(&socket)? {
        Arbitration::Conceded => Ok(already_running(&root, &socket)),
        Arbitration::Granted(_claim) => {
            spawn_detached(&root)?;
            await_liveness(&socket)?;
            Ok(CommandOutcome::success(format!(
                "ktsense: daemon started for {}\nsocket: {}\n",
                root.display(),
                socket.display()
            )))
        }
    }
}

fn already_running(root: &Path, socket: &Path) -> CommandOutcome {
    CommandOutcome::success(format!(
        "ktsense: a daemon is already running for {}\nsocket: {}\n",
        root.display(),
        socket.display()
    ))
}

fn live(socket: &Path) -> bool {
    block_on(status(socket)) == Liveness::Live
}

/// Which side of one start race this process is on: it owns the attempt, or another start owns it and
/// has already produced the daemon this one was asked for.
enum Arbitration {
    Granted(StartClaim),
    Conceded,
}

/// Arbitrates this start against every other start for the same root, returning once this process
/// holds the claim or once a daemon is live, whichever comes first. A claimant that dies releases its
/// claim, because the kernel closes the listener behind it, so a crashed start cannot lock a root out
/// of ever starting again.
///
/// Liveness is read twice, at the top of the loop and again once a claim is granted. The first lets a
/// waiting start return as soon as the daemon it asked for exists, rather than waiting for the winner
/// to release the claim; the second catches a start that was granted a free claim because the winner
/// had already finished and released it. Either alone stops a late start from spawning a duplicate,
/// which is why removing one and not the other changes nothing an observer can see, and both are kept
/// because they bound different waits.
fn claim_the_start(socket: &Path) -> Result<Arbitration, CommandError> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if live(socket) {
            return Ok(Arbitration::Conceded);
        }
        match take_claim(socket) {
            Ok(Some(claim)) => {
                return Ok(if live(socket) {
                    Arbitration::Conceded
                } else {
                    Arbitration::Granted(claim)
                })
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(START_POLL_INTERVAL),
            Ok(None) => {
                return Err(failure(format!(
                    "ktsense: another start holds {} and produced no daemon within {}s",
                    socket.display(),
                    START_TIMEOUT.as_secs()
                )))
            }
            Err(error) => {
                return Err(failure(format!(
                    "ktsense: cannot claim the start for {}: {error}",
                    socket.display()
                )))
            }
        }
    }
}

/// The right to start one root's daemon, held for as long as the listener is open. Nothing ever
/// accepts on it: its existence is the claim, and connecting to it is how another start learns
/// whether the holder is still alive.
struct StartClaim {
    _listener: UnixListener,
    path: PathBuf,
}

/// Releases the claim. The file is removed as well as the listener closed, so the next start binds
/// this same generation instead of walking past a file nobody owns.
impl Drop for StartClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Binds the first free generation of this root's claim, or reports that a live claimant holds one.
///
/// A listening socket rather than a lock file, because its owner's death releases it: the kernel
/// closes the listener, and the file left behind then refuses connections, which is exactly what
/// tells the next start that the claim was abandoned rather than held.
///
/// Generations exist because nothing here may unlink another process's claim. Two starts that both
/// found the same abandoned claim would each remove what they found, and the second removal would
/// take the first's fresh claim, leaving two owners and the double spawn this arbitration exists to
/// prevent. Moving to the next generation instead leaves the decision to `bind` alone, which is
/// atomic. The cost is one dead socket file per claimant that died, in a directory the system clears
/// between logins.
fn take_claim(socket: &Path) -> io::Result<Option<StartClaim>> {
    prepare_claim_dir(socket)?;
    for generation in 0..MAX_CLAIM_GENERATIONS {
        let path = claim_path(socket, generation);
        match UnixListener::bind(&path) {
            Ok(listener) => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(CLAIM_MODE))?;
                return Ok(Some(StartClaim {
                    _listener: listener,
                    path,
                }));
            }
            Err(error) if names_a_path_already_there(&error) => {
                if UnixStream::connect(&path).is_ok() {
                    return Ok(None);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{MAX_CLAIM_GENERATIONS} abandoned start claims already sit beside this socket"),
    ))
}

/// Whether a failed `bind` means this generation's file is already there, which is a contended or
/// abandoned claim, as opposed to a filesystem refusal the start has to report.
///
/// Linux answers `EADDRINUSE`. A BSD-derived kernel may answer `EEXIST` for the same condition, and
/// the two are the same fact about the same path, so both are read as occupancy rather than the
/// arbitration resting on which errno a platform chose. Reading only one of them turns the other
/// platform's contended claim into a failed start: on macOS CI one of two starts released together
/// exited 1 where every Linux run conceded (KT-65), and a claim that is merely held must never end a
/// start that would otherwise have been told about the winner's daemon.
fn names_a_path_already_there(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::AlreadyExists
    )
}

/// Creates the socket directory when a start is the first thing to need it, owner-only as it is
/// created so a claim is never briefly reachable by anyone else. An existing directory is left
/// exactly as it was, including one owned by another user, which the daemon still refuses.
fn prepare_claim_dir(socket: &Path) -> io::Result<()> {
    let dir = socket.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", socket.display()),
        )
    })?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(CLAIM_DIR_MODE)
        .create(dir)
}

/// Where one generation of a root's claim lives: beside that root's daemon socket, so it inherits
/// both the owner-only directory and the per-root identity the socket name already carries.
fn claim_path(socket: &Path, generation: u32) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(format!(".start{generation}"));
    PathBuf::from(name)
}

/// Where a test tells `start` to wait, and for how many participants.
const BARRIER_DIR_ENV: &str = "KTSENSE_START_BARRIER_DIR";
const BARRIER_PARTIES_ENV: &str = "KTSENSE_START_BARRIER_PARTIES";
const BARRIER_TIMEOUT: Duration = Duration::from_secs(10);

/// A test seam, inert unless both barrier variables are set. It holds every participating start here,
/// after the liveness check and before arbitration, until all of them have arrived, so a test can
/// prove the arbitration settled a real overlap rather than two launches that happened to miss each
/// other. Arrivals are files, so the test can also count them. Bounded: a participant that never
/// arrives delays a start rather than hanging it.
fn rendezvous() {
    let (Some(dir), Some(parties)) = (
        std::env::var_os(BARRIER_DIR_ENV).map(PathBuf::from),
        std::env::var(BARRIER_PARTIES_ENV)
            .ok()
            .and_then(|parties| parties.parse::<usize>().ok()),
    ) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join(std::process::id().to_string()), b"");
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    while arrivals(&dir) < parties && Instant::now() < deadline {
        std::thread::sleep(START_POLL_INTERVAL);
    }
}

fn arrivals(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| entries.count())
        .unwrap_or_default()
}

/// Stops the daemon for `root`, or reports that none was running.
pub(crate) fn shutdown(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root);
    match block_on(stop(&socket)) {
        Ok(StopOutcome::Stopped) => Ok(CommandOutcome::success(format!(
            "ktsense: daemon stopped for {}\n",
            root.display()
        ))),
        Ok(StopOutcome::NotRunning) => Ok(CommandOutcome::success(format!(
            "ktsense: no daemon was running for {}\n",
            root.display()
        ))),
        Err(error) => Err(failure(format!("ktsense: cannot stop the daemon: {error}"))),
    }
}

/// Serves until stopped, idle, or faulted. This is the body the detached child runs; it is hidden
/// from help because it is an implementation detail of `start`, not a command to invoke by hand.
pub(crate) fn serve(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root);
    let config = DaemonConfig::new(socket, idle_timeout());
    let initialize = InitializeConfig {
        root_uri: file_uri(&root),
        ignore_patterns: vec![crate::trace::IGNORED_BUILD_OUTPUT.to_string()],
    };

    let reason = block_on(async move {
        let engine = WarmEngine::warm_up(initialize)
            .await
            .map_err(|error| failure(format!("ktsense: cannot start the engine: {error}")))?;
        let engine = crate::routing::CommandEngine::new(root, engine);
        run(config, engine)
            .await
            .map_err(|error| failure(format!("ktsense: daemon failed: {error}")))
    })?;

    Ok(CommandOutcome::success(format!(
        "ktsense: daemon exited because {}\n",
        reason_label(reason)
    )))
}

fn reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stopped => "a client asked it to stop",
        StopReason::Idle => "it was idle",
        StopReason::EngineFaulted => "its engine session faulted",
    }
}

/// The socket a root's daemon listens on, resolved exactly as the daemon crate resolves it so the two
/// halves cannot disagree about where to meet. The environment is read here, at the edge.
pub(crate) fn socket_for(root: &Path) -> PathBuf {
    let root = canonical_root(root);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    socket_path(&socket_dir(runtime.as_deref(), &home), &root)
}

/// Resolves the root before it is used as a daemon's identity, so `.`, a relative path and an
/// absolute path all address the same daemon instead of silently starting three.
pub(crate) fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn file_uri(root: &Path) -> String {
    format!("file://{}", root.display())
}

fn idle_timeout() -> Duration {
    std::env::var(IDLE_SECS_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT)
}

/// Re-executes this binary as a detached `daemon serve` child with its streams closed, so the daemon
/// neither holds the terminal nor writes into the caller's output.
fn spawn_detached(root: &Path) -> Result<(), CommandError> {
    let executable = std::env::current_exe().map_err(|error| {
        failure(format!(
            "ktsense: cannot locate the ktsense binary: {error}"
        ))
    })?;
    Command::new(executable)
        .arg("--root")
        .arg(root)
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_detached| ())
        .map_err(|error| failure(format!("ktsense: cannot start the daemon: {error}")))
}

/// Waits for the freshly spawned daemon to accept a connection. Polling the socket rather than
/// trusting the spawn is what makes a reported start mean a reachable daemon.
fn await_liveness(socket: &Path) -> Result<(), CommandError> {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if live(socket) {
            return Ok(());
        }
        std::thread::sleep(START_POLL_INTERVAL);
    }
    Err(failure(format!(
        "ktsense: the daemon did not answer on {} within {}s",
        socket.display(),
        START_TIMEOUT.as_secs()
    )))
}

fn failure(message: String) -> CommandError {
    CommandError {
        exit: Exit::Failure,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The occupancy decision holds on every platform, because the errno a kernel picks for "that
    /// path is already there" is not the same everywhere. Both readings mean a claim is held or was
    /// abandoned, and both must be stepped over; a refusal that is neither has to end the start
    /// rather than be mistaken for contention.
    #[test]
    fn a_taken_claim_path_is_recognized_whichever_errno_the_platform_reports() {
        let reads_as_taken =
            |kind: io::ErrorKind| names_a_path_already_there(&io::Error::from(kind));

        assert_eq!(
            (
                reads_as_taken(io::ErrorKind::AddrInUse),
                reads_as_taken(io::ErrorKind::AlreadyExists),
                reads_as_taken(io::ErrorKind::PermissionDenied),
                reads_as_taken(io::ErrorKind::NotFound),
            ),
            (true, true, false, false)
        );
    }

    /// A claim blocks every other start for as long as it is held, and only for as long. The socket it
    /// binds is the claim itself, so releasing it removes the file and frees the same generation for
    /// the next start rather than pushing it onwards.
    #[test]
    fn a_held_claim_blocks_another_start_and_releasing_it_frees_the_same_generation() {
        let home = tempfile::tempdir().expect("temp dir");
        let socket = home.path().join("root.sock");
        let first_generation = claim_path(&socket, 0);

        let held = take_claim(&socket).expect("a first claim");
        let blocked = take_claim(&socket).expect("a second attempt");
        let while_held = (held.is_some(), blocked.is_none(), first_generation.exists());
        drop(held);
        let after_release = first_generation.exists();
        let regranted = take_claim(&socket).expect("a claim after the release");

        assert_eq!(
            (
                while_held,
                after_release,
                regranted.as_ref().map(|claim| claim.path.clone()),
                claim_path(&socket, 1).exists(),
            ),
            ((true, true, true), false, Some(first_generation), false)
        );
    }

    /// The owner-recovery story, exercised. A start that was killed leaves its claim socket on disk
    /// with nothing listening behind it, which is what dropping a listener without removing its file
    /// reproduces, and the next start must be granted a claim rather than locked out of the root for
    /// good. The abandoned file is stepped over rather than unlinked: two starts that both found it
    /// would otherwise each remove what they found, and the second removal would take the first's
    /// fresh claim.
    #[test]
    fn a_claim_abandoned_by_a_killed_start_is_stepped_over_rather_than_unlinked() {
        let home = tempfile::tempdir().expect("temp dir");
        let socket = home.path().join("root.sock");
        let abandoned = claim_path(&socket, 0);
        drop(UnixListener::bind(&abandoned).expect("bind then abandon a claim"));

        let granted = take_claim(&socket).expect("a claim beside the abandoned one");

        assert_eq!(
            (
                granted.as_ref().map(|claim| claim.path.clone()),
                abandoned.exists(),
                UnixStream::connect(&abandoned)
                    .err()
                    .map(|error| error.kind()),
                take_claim(&socket).expect("a third attempt").is_none(),
            ),
            (
                Some(claim_path(&socket, 1)),
                true,
                Some(io::ErrorKind::ConnectionRefused),
                true
            )
        );
    }
}
