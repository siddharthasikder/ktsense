//! The `daemon start|stop|status` lifecycle surface over the daemon crate's library API.
//!
//! `start` does not fork. It re-executes this binary's hidden `daemon serve` subcommand as a
//! detached child, which keeps the CLI free of unsafe process manipulation and behaves identically on
//! Linux and macOS. The parent then waits for the socket to answer, so a successful `start` reports a
//! daemon that is actually reachable rather than merely a process that was spawned.
//!
//! Every outcome is reported in the text, not only in the exit code. An agent that asked for a daemon
//! and got one already running needs to be told that, because "already running" and "just started"
//! lead to different next steps even though both are successes.

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

/// Reports whether a daemon is running for `root`, and where its socket is.
pub(crate) fn report_status(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root);
    let text = match block_on(status(&socket)) {
        Liveness::Live => format!(
            "ktsense: daemon running for {}\nsocket: {}\nindex phase, symbol counts and uptime are \
             not reported yet (KT-36)\n",
            root.display(),
            socket.display()
        ),
        // A stale socket is not an error to report to a human: naming it explains why a file exists
        // where a live daemon is not, and says who will clean it up.
        Liveness::Stale => format!(
            "ktsense: no daemon running for {}\nsocket: {} is a stale leftover and will be \
             reclaimed by the next start\n",
            root.display(),
            socket.display()
        ),
        Liveness::Absent => format!(
            "ktsense: no daemon running for {}\nsocket: {} does not exist\n",
            root.display(),
            socket.display()
        ),
    };
    Ok(CommandOutcome::success(text))
}

/// Starts a daemon for `root`, or reports that one is already running. Idempotent by design: asking
/// for something that already exists is a success, and saying so is more useful than an error.
pub(crate) fn start(root: &Path) -> Result<CommandOutcome, CommandError> {
    let root = canonical_root(root);
    let socket = socket_for(&root);
    if block_on(status(&socket)) == Liveness::Live {
        return Ok(CommandOutcome::success(format!(
            "ktsense: a daemon is already running for {}\nsocket: {}\n",
            root.display(),
            socket.display()
        )));
    }

    spawn_detached(&root)?;
    await_liveness(&socket)?;
    Ok(CommandOutcome::success(format!(
        "ktsense: daemon started for {}\nsocket: {}\n",
        root.display(),
        socket.display()
    )))
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
        ignore_patterns: Vec::new(),
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
fn canonical_root(root: &Path) -> PathBuf {
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
        if block_on(status(socket)) == Liveness::Live {
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
