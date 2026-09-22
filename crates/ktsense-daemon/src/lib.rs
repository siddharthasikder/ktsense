//! Warm-session daemon.
//!
//! The daemon owns one initialized engine session plus the parsed-skeleton cache so that repeated
//! agent queries do not pay startup twice. The server itself lands in KT-27; this module currently
//! owns socket naming, which the CLI needs in order to find a running daemon at all.

#![forbid(unsafe_code)]

mod client;
mod engine;
mod server;
mod symbols;
mod wire;

pub use client::{verify_protocol, Client, ClientError, ProtocolMismatch};
pub use engine::{Engine, EngineRequest, HandlerOutcome, IndexTracker, WarmEngine};
pub use server::{
    probe, run, status, stop, DaemonConfig, DaemonError, Liveness, StopOutcome, StopReason,
    DEFAULT_IDLE_TIMEOUT,
};
pub use symbols::{resolve_from_warm_index, Inconclusive, WarmResolution};
pub use wire::{read_frame, write_frame, ClientFrame, ServerFrame, WireError, MAX_FRAME};

use std::path::{Path, PathBuf};

/// Protocol version carried in the hello frame, so a stale daemon is detected rather than trusted.
pub const PROTOCOL_VERSION: u32 = 1;

/// Directory holding daemon sockets, honouring `XDG_RUNTIME_DIR` when the platform sets it.
pub fn socket_dir(xdg_runtime_dir: Option<&Path>, home: &Path) -> PathBuf {
    match xdg_runtime_dir {
        Some(runtime) => runtime.join("ktsense"),
        None => home.join(".cache").join("ktsense").join("run"),
    }
}

/// Socket path for one workspace root. Distinct roots get distinct daemons.
pub fn socket_path(dir: &Path, workspace_root: &Path) -> PathBuf {
    dir.join(format!("{}.sock", root_key(workspace_root)))
}

/// Stable short key for a workspace root path.
///
/// FNV-1a keeps this dependency-free and is sufficient: the key only has to distinguish roots on one
/// machine, and a collision costs a wrong-daemon rejection at the hello frame, not silent bad data.
fn root_key(workspace_root: &Path) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in workspace_root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_is_preferred_and_home_is_the_fallback() {
        let with_runtime = socket_dir(Some(Path::new("/run/user/1000")), Path::new("/home/dev"));
        let without_runtime = socket_dir(None, Path::new("/home/dev"));
        assert_eq!(
            (with_runtime, without_runtime),
            (
                PathBuf::from("/run/user/1000/ktsense"),
                PathBuf::from("/home/dev/.cache/ktsense/run"),
            )
        );
    }

    #[test]
    fn distinct_roots_get_distinct_sockets_and_one_root_is_stable() {
        let dir = Path::new("/run/user/1000/ktsense");
        let first = socket_path(dir, Path::new("/work/alpha"));
        let second = socket_path(dir, Path::new("/work/beta"));
        let repeat = socket_path(dir, Path::new("/work/alpha"));
        assert_eq!((first == second, first == repeat), (false, true));
    }
}
