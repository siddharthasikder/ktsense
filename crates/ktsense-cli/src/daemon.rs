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
//! settled by one atomic claim taken before either spawns an engine, so exactly one of them owns the
//! attempt and each is told which one it was.

use std::fs::File;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{flock, FlockOperation};

use ktsense_daemon::{
    resolve_socket_path, run, short_socket_base, status, stop, DaemonConfig, Liveness, StopOutcome,
    StopReason, WarmEngine, DEFAULT_IDLE_TIMEOUT,
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

/// Suffix of the one file a start locks to claim a root, appended to that root's socket path. One
/// file and not a series: the lock on it is what decides the winner, and a second path to try is
/// exactly what let two starts believe they had both won (KT-71). Six bytes, inside the
/// `COMPANION_RESERVE` the socket budget keeps free for it.
const CLAIM_SUFFIX: &str = ".start";

/// Reports whether a daemon is running for `root`, and where its socket is.
pub(crate) fn report_status(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root)?;
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
/// One atomic operation grants that claim, so what a start reports and what a start spawns come from
/// the same decision and cannot disagree (KT-71).
pub(crate) fn start(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root)?;
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
/// claim, because the kernel drops the lock behind it, so a crashed start cannot lock a root out of
/// ever starting again.
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

/// The right to start one root's daemon, held for as long as the lock is held. Nothing ever reads or
/// writes the file: the lock on it is the claim, and the kernel dropping that lock is how another
/// start learns the holder is gone.
struct StartClaim {
    _locked: File,
    path: PathBuf,
}

/// Releases the claim, removing the file before closing the descriptor that locks it. That order is
/// deliberate: closing first would free the lock, and the file this then removed could already be the
/// next start's claim. Removing first means the only claim a release can remove is its own, and an
/// acquirer that locked the removed file sees it is no longer linked.
impl Drop for StartClaim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Locks this root's claim, or reports that another start holds it.
///
/// One atomic operation decides both facts a start needs: `flock` either grants the claim or reports
/// that it is held, and the kernel releases it when the holder's descriptor closes, which a death
/// does. Nothing has to *observe* whether the holder is still alive, and that is the whole point. The
/// previous mechanism bound a listening socket per claim and probed the occupant with a connect, then
/// moved to the next of 64 generations when the probe did not answer. A probe cannot tell a dead
/// holder from a live one between its own `bind(2)` and `listen(2)`, from one that released mid-probe,
/// or, on a BSD kernel, from one whose never-accepted backlog has filled: each of those minted a
/// second claim beside the first, after which both starts spawned a daemon and both reported that they
/// had started it. That is KT-71, seen on macOS CI where the gap between two syscalls is wide enough
/// to land in.
///
/// A file that exists but is unlocked is takeable in place, so there is no abandoned claim to step
/// over and no second path to create. A file whose link count has reached zero was removed by the
/// holder it belonged to while this call had it open, which means the lock just acquired is not on the
/// claim this root's path names; that is reported as held, and the next attempt creates the file
/// afresh. The link count answers this rather than a comparison of inode numbers, because a number
/// can be reused and a positive link count cannot be: only this one path is ever linked.
fn take_claim(socket: &Path) -> io::Result<Option<StartClaim>> {
    prepare_claim_dir(socket)?;
    let path = claim_path(socket);
    let locked = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(CLAIM_MODE)
        .open(&path)?;
    if let Err(errno) = flock(&locked, FlockOperation::NonBlockingLockExclusive) {
        let error = io::Error::from(errno);
        return if held_by_another_start(&error) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if !still_names_the_claim(&locked)? {
        return Ok(None);
    }
    Ok(Some(StartClaim {
        _locked: locked,
        path,
    }))
}

/// Whether the file this start locked is still the one its root's claim path names. A holder removes
/// its claim before closing the descriptor that locks it, so a lock granted on a file with no names
/// left is a lock on a claim that has already been released and replaced.
fn still_names_the_claim(locked: &File) -> io::Result<bool> {
    Ok(locked.metadata()?.nlink() > 0)
}

/// Whether a failed lock means another start holds the claim, as opposed to a filesystem refusal the
/// start has to report.
///
/// A claim that is merely held must never end a start that would otherwise have been told about the
/// winner's daemon: on macOS CI one of two starts released together exited 1 where every Linux run
/// conceded, because the platform reported contention with an errno the code did not recognise
/// (KT-65). `EAGAIN` and `EWOULDBLOCK` are the same value on both platforms and std maps both to one
/// kind, so this rests on the kind rather than on which name a kernel header chose.
fn held_by_another_start(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
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

/// Where a root's claim lives: beside that root's daemon socket, so it inherits both the owner-only
/// directory and the per-root identity the socket name already carries.
fn claim_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_os_string();
    name.push(CLAIM_SUFFIX);
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
    let socket = socket_for(&root)?;
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
    let socket = socket_for(&root)?;
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
///
/// Fallible because a socket path is a fixed-size address rather than a pathname: a runtime directory
/// deep enough to overflow one is answered from a short base instead, and when even that cannot yield
/// a bindable path the command says so rather than leaving a `bind` to fail where the caller has
/// nothing to explain it with.
pub(crate) fn socket_for(root: &Path) -> Result<PathBuf, CommandError> {
    let root = canonical_root(root);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    resolve_socket_path(runtime.as_deref(), &home, &short_socket_base(), &root)
        .map_err(|error| failure(format!("ktsense: {error}")))
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

    use ktsense_daemon::{COMPANION_RESERVE, MAX_SOCKET_PATH, SOCKET_BUDGET};

    /// The socket budget reserves room for the companion file a start puts beside a socket, and this
    /// is the file it means. A socket placed exactly at the budget must still leave room for its own
    /// claim, because a socket that fits while its claim does not is what failed on macOS CI: a
    /// 101-byte socket bound, and its 108-byte claim did not (KT-66).
    #[test]
    fn the_claim_name_still_fits_a_socket_placed_at_the_budget() {
        let at_budget = PathBuf::from("x".repeat(SOCKET_BUDGET));

        let claim = claim_path(&at_budget);
        let suffix = claim.as_os_str().len() - at_budget.as_os_str().len();

        assert_eq!(
            (
                suffix,
                suffix <= COMPANION_RESERVE,
                claim.as_os_str().len() <= MAX_SOCKET_PATH,
            ),
            (CLAIM_SUFFIX.len(), true, true)
        );
    }

    /// Contention must never end a start that would otherwise have been told about the winner's
    /// daemon: on macOS CI one of two starts released together exited 1 where every Linux run
    /// conceded, because the platform reported a held claim with an errno the code did not recognise
    /// (KT-65). A refusal that is not contention has to end the start rather than be mistaken for it.
    #[test]
    fn a_held_claim_is_recognized_as_contention_and_a_refusal_is_not() {
        let reads_as_held = |kind: io::ErrorKind| held_by_another_start(&io::Error::from(kind));

        assert_eq!(
            (
                reads_as_held(io::ErrorKind::WouldBlock),
                reads_as_held(io::ErrorKind::PermissionDenied),
                reads_as_held(io::ErrorKind::NotFound),
                reads_as_held(io::ErrorKind::AddrInUse),
            ),
            (true, false, false, false)
        );
    }

    /// KT-71: one root has one claim, and locking it is the only thing that decides who owns the
    /// start. Every state an arriving start can find is covered here, because the defect this replaces
    /// was a fourth state - a claim whose holder could not be observed - that the old mechanism
    /// answered by creating a second claim beside the first, after which both starts reported that
    /// they had started the daemon.
    ///
    /// Nothing is present, so the first start is granted. The claim is then held, so the second is
    /// refused rather than offered anything else, and the directory still holds exactly one claim.
    /// Releasing removes the file, and a start after the release is granted that same path.
    #[test]
    fn one_root_has_one_claim_and_locking_it_is_what_decides_the_winner() {
        let home = tempfile::tempdir().expect("temp dir");
        let socket = home.path().join("root.sock");
        let claim = claim_path(&socket);

        let held = take_claim(&socket).expect("a first claim");
        let blocked = take_claim(&socket).expect("a second attempt");
        let while_held = (
            held.as_ref().map(|claim| claim.path.clone()),
            blocked.is_none(),
            names_beside(&socket),
        );
        drop(held);
        let after_release = (claim.exists(), names_beside(&socket));
        let regranted = take_claim(&socket).expect("a claim after the release");

        assert_eq!(
            (
                while_held,
                after_release,
                regranted.as_ref().map(|claim| claim.path.clone()),
            ),
            (
                (Some(claim.clone()), true, vec![file_name(&claim)]),
                (false, Vec::new()),
                Some(claim),
            )
        );
    }

    /// The recovery story KT-61 wrote the claim for, exercised. A start that was killed leaves its
    /// claim file on disk with no lock behind it, because the kernel drops the lock as the process
    /// dies, and the next start must be granted rather than locked out of the root for good. A file
    /// written without ever being locked is that state; nothing here kills a process, as nothing did
    /// when the claim was a listener. It is granted in place, so no second claim can exist.
    #[test]
    fn a_claim_abandoned_by_a_killed_start_is_reclaimed_in_place() {
        let home = tempfile::tempdir().expect("temp dir");
        let socket = home.path().join("root.sock");
        let claim = claim_path(&socket);
        std::fs::write(&claim, b"").expect("a claim file with no lock behind it");

        let granted = take_claim(&socket).expect("a claim beside the abandoned one");

        assert_eq!(
            (
                granted.as_ref().map(|claim| claim.path.clone()),
                names_beside(&socket),
                take_claim(&socket).expect("a third attempt").is_none(),
            ),
            (Some(claim.clone()), vec![file_name(&claim)], true)
        );
    }

    /// The guard on the release window. A holder removes its claim before closing the descriptor that
    /// locks it, so a start that had the file open across that removal can be granted a lock on a file
    /// the claim path no longer names. A link count of zero is what says so, and it cannot be fooled
    /// by inode reuse the way a comparison of inode numbers can, because only the claim path is ever
    /// linked.
    #[test]
    fn a_lock_on_a_file_the_claim_path_no_longer_names_is_not_the_claim() {
        let home = tempfile::tempdir().expect("temp dir");
        let path = home.path().join("root.sock.start");
        let opened = File::create(&path).expect("a claim file");

        let while_linked = still_names_the_claim(&opened).expect("a link count");
        std::fs::remove_file(&path).expect("remove it behind the open descriptor");
        let after_removal = still_names_the_claim(&opened).expect("a link count");

        assert_eq!((while_linked, after_removal), (true, false));
    }

    /// Everything sharing the claim's directory, so a test can say that a start created one file and
    /// not a second one under another name.
    fn names_beside(socket: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(socket.parent().expect("a parent"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn file_name(path: &Path) -> String {
        path.file_name()
            .expect("the claim has a name")
            .to_string_lossy()
            .into_owned()
    }
}
