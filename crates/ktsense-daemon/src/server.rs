//! The daemon server core: bind a restricted socket, serve one warm engine, and exit on stop, idle
//! or a faulted session.
//!
//! Everything here is a callable function that returns a value. Nothing prints or exits the process,
//! so a later CLI stage can wrap [`run`], [`stop`] and [`status`] onto `daemon start|stop|status`
//! without this crate deciding how results reach a terminal.

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use crate::client::{Client, ClientError};
use crate::engine::{Engine, EngineRequest, HandlerOutcome};
use crate::wire::{self, ClientFrame, ServerFrame};
use crate::PROTOCOL_VERSION;

/// Owner-only directory: an unauthenticated local endpoint that exposes source stays reachable only
/// by the user who started it.
const DIR_MODE: u32 = 0o700;
/// Owner-only socket, for the same reason.
const SOCKET_MODE: u32 = 0o600;

/// Default idle window before an untouched daemon exits.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How the daemon should run: where to listen and how long to stay up while idle.
///
/// `idle_timeout` is injected rather than hard-coded so a test can prove expiry in milliseconds
/// without waiting an hour.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub idle_timeout: Duration,
}

impl DaemonConfig {
    pub fn new(socket_path: PathBuf, idle_timeout: Duration) -> Self {
        Self {
            socket_path,
            idle_timeout,
        }
    }
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A client sent `Stop`.
    Stopped,
    /// No connection arrived within the idle window.
    Idle,
    /// The warm session faulted and could no longer be trusted.
    EngineFaulted,
}

/// Whether a socket path is backed by a live daemon, a dead one, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// A daemon accepted a connection.
    Live,
    /// The socket file exists but no daemon is listening; it is safe to unlink and rebind.
    Stale,
    /// No socket file is present.
    Absent,
}

/// What [`stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped,
    NotRunning,
}

/// A failure that prevents the daemon from serving.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("socket path {0} has no parent directory")]
    NoParent(PathBuf),
    #[error("refusing a socket path owned by uid {uid} rather than reusing it")]
    ForeignSocket { uid: u32 },
    #[error("a daemon is already running on this socket")]
    AlreadyRunning,
    #[error("could not bind the daemon socket: {0}")]
    Bind(#[source] io::Error),
    #[error("could not accept a daemon connection: {0}")]
    Accept(#[source] io::Error),
    #[error("daemon filesystem error: {0}")]
    Io(#[from] io::Error),
}

/// Serves `engine` on `config.socket_path` until a stop, the idle window, or a session fault, then
/// tears the engine down and removes the socket. This is the body a daemonized `daemon start` runs.
pub async fn run<E: Engine>(config: DaemonConfig, engine: E) -> Result<StopReason, DaemonError> {
    let uid = current_uid();
    let dir = config
        .socket_path
        .parent()
        .ok_or_else(|| DaemonError::NoParent(config.socket_path.clone()))?;
    prepare_directory(dir, uid)?;
    prepare_socket_path(&config.socket_path, uid).await?;

    let listener = UnixListener::bind(&config.socket_path).map_err(DaemonError::Bind)?;
    set_mode(&config.socket_path, SOCKET_MODE)?;
    let _socket = SocketGuard(config.socket_path.clone());

    let reason = serve_loop(&listener, config.idle_timeout, &engine).await;
    engine.shutdown().await;
    reason
}

/// Reports whether a daemon is live, stale, or absent at `socket_path`, distinguishing a dead
/// leftover from a genuinely running one so `start` can be idempotent rather than racing.
pub async fn probe(socket_path: &Path) -> Liveness {
    match UnixStream::connect(socket_path).await {
        Ok(_stream) => Liveness::Live,
        Err(err) if err.kind() == io::ErrorKind::NotFound => Liveness::Absent,
        Err(_refused_or_unusable) => Liveness::Stale,
    }
}

/// Alias for [`probe`], named for the CLI surface a later stage exposes.
pub async fn status(socket_path: &Path) -> Liveness {
    probe(socket_path).await
}

/// Stops a running daemon, or reports that none was running. Idempotent: stopping an absent or dead
/// daemon is a no-op, not an error.
pub async fn stop(socket_path: &Path) -> Result<StopOutcome, ClientError> {
    if probe(socket_path).await != Liveness::Live {
        return Ok(StopOutcome::NotRunning);
    }
    let client = Client::connect(socket_path).await?;
    client.stop().await?;
    wait_until_gone(socket_path, STOP_TIMEOUT).await;
    Ok(StopOutcome::Stopped)
}

const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);

async fn serve_loop<E: Engine>(
    listener: &UnixListener,
    idle: Duration,
    engine: &E,
) -> Result<StopReason, DaemonError> {
    loop {
        match timeout(idle, listener.accept()).await {
            Err(_elapsed) => return Ok(StopReason::Idle),
            Ok(Err(err)) => return Err(DaemonError::Accept(err)),
            Ok(Ok((stream, _addr))) => match serve_connection(stream, engine).await {
                ConnectionEnd::Closed => continue,
                ConnectionEnd::Stop => return Ok(StopReason::Stopped),
                ConnectionEnd::Faulted => return Ok(StopReason::EngineFaulted),
            },
        }
    }
}

enum ConnectionEnd {
    Closed,
    Stop,
    Faulted,
}

/// Serves one connection. A transport error on a single connection ends only that connection; only a
/// stop request or a session fault ends the daemon, so a client that hangs up cannot take it down.
async fn serve_connection<E: Engine>(mut stream: UnixStream, engine: &E) -> ConnectionEnd {
    let hello = ServerFrame::Hello {
        protocol_version: PROTOCOL_VERSION,
    };
    if wire::write_frame(&mut stream, &hello).await.is_err() {
        return ConnectionEnd::Closed;
    }
    loop {
        let frame = match wire::read_frame::<_, ClientFrame>(&mut stream).await {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(_) => return ConnectionEnd::Closed,
        };
        let request = match frame {
            ClientFrame::Stop => return ConnectionEnd::Stop,
            ClientFrame::Request { method, params } => EngineRequest { method, params },
        };
        let response = match engine.handle(request).await {
            HandlerOutcome::Reply(value) => ServerFrame::Result { value },
            HandlerOutcome::Error(message) => ServerFrame::Error { message },
            HandlerOutcome::Faulted(message) => {
                let _ = wire::write_frame(&mut stream, &ServerFrame::Error { message }).await;
                return ConnectionEnd::Faulted;
            }
        };
        if wire::write_frame(&mut stream, &response).await.is_err() {
            return ConnectionEnd::Closed;
        }
    }
}

fn prepare_directory(dir: &Path, current: u32) -> Result<(), DaemonError> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => refuse_foreign(meta.uid(), current)?,
        Err(err) if err.kind() == io::ErrorKind::NotFound => create_private_dir(dir)?,
        Err(err) => return Err(err.into()),
    }
    set_mode(dir, DIR_MODE)?;
    Ok(())
}

/// Creates the socket directory and every parent it needs, owner-only as each is created rather than
/// widened afterwards. The mode covers the parents because a fallback socket directory sits under a
/// per-uid base this call may be the first to create, and a base left at the default mode would be a
/// directory anyone could write into holding a socket that serves source.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
}

async fn prepare_socket_path(socket_path: &Path, current: u32) -> Result<(), DaemonError> {
    match std::fs::symlink_metadata(socket_path) {
        Ok(meta) => {
            refuse_foreign(meta.uid(), current)?;
            match probe(socket_path).await {
                Liveness::Live => Err(DaemonError::AlreadyRunning),
                Liveness::Stale | Liveness::Absent => {
                    std::fs::remove_file(socket_path)?;
                    Ok(())
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn refuse_foreign(file_uid: u32, current: u32) -> Result<(), DaemonError> {
    if owned_by_current_user(file_uid, current) {
        Ok(())
    } else {
        Err(DaemonError::ForeignSocket { uid: file_uid })
    }
}

fn owned_by_current_user(file_uid: u32, current: u32) -> bool {
    file_uid == current
}

pub(crate) fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

async fn wait_until_gone(socket_path: &Path, within: Duration) {
    let _ = timeout(within, async {
        while socket_path.exists() {
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }
    })
    .await;
}

/// Removes the socket file when the daemon stops, so a clean shutdown never leaves a stale socket
/// behind for the next start to trip over.
struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_owned_by_another_user_is_refused() {
        let current = current_uid();
        let observed = (
            refuse_foreign(current, current).is_ok(),
            matches!(
                refuse_foreign(current.wrapping_add(1), current),
                Err(DaemonError::ForeignSocket { uid }) if uid == current.wrapping_add(1)
            ),
        );
        assert_eq!(observed, (true, true));
    }
}
