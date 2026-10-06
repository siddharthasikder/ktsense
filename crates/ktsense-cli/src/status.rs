//! `ktsense status`: what this installation would answer with, and what state it is in.
//!
//! Everything reported is observed, not assumed: the engine binary is the one discovery would use
//! and its version is what `--version` printed just now; the daemon's state comes from probing its
//! socket, and its uptime, index phase and request count are read from the daemon itself over the
//! `ktsense/status` method. The command never fails because something is missing: an absent engine
//! or daemon is exactly the state a caller runs `status` to learn about, so it is reported and the
//! command exits 0. Symbol counts are not reported: the engine keeps a single global `status.json`
//! whose last writer wins across workspaces, so no per-root count can be trusted from it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ktsense_daemon::{status as probe_daemon, AnyClient, Client, Liveness, PROTOCOL_VERSION};
use ktsense_lsp::{classify, Compatibility, IndexPhase, LSP_BINARY, PINNED_UPSTREAM_VERSION};
use serde::{Deserialize, Serialize};

use crate::{block_on, collect_kotlin_files, CommandError, Format};

pub(crate) const STATUS_METHOD: &str = "ktsense/status";

/// What a live daemon reports about itself. Produced by the daemon's `CommandEngine`, consumed by
/// the CLI; the same type on both ends keeps the wire honest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DaemonSnapshot {
    pub(crate) uptime_secs: u64,
    pub(crate) index: IndexLabel,
    pub(crate) requests_served: u64,
    /// The idle window this daemon will exit after, in seconds, so `status` reports the limit the
    /// daemon actually holds rather than a fixed default: an autostarted daemon idles out sooner
    /// than one started by hand (KT-125).
    pub(crate) idle_limit_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IndexLabel {
    Pending,
    Indexing,
    Complete,
}

impl From<IndexPhase> for IndexLabel {
    fn from(phase: IndexPhase) -> Self {
        match phase {
            IndexPhase::Pending => IndexLabel::Pending,
            IndexPhase::Indexing => IndexLabel::Indexing,
            IndexPhase::Ready => IndexLabel::Complete,
        }
    }
}

impl IndexLabel {
    fn text(self) -> &'static str {
        match self {
            IndexLabel::Pending => "pending",
            IndexLabel::Indexing => "indexing",
            IndexLabel::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusReport {
    pub(crate) root: PathBuf,
    pub(crate) kotlin_files: usize,
    pub(crate) engine: EngineStatus,
    /// The `rg` binary this installation would exec, or `None` when it is not on `PATH`. The engine
    /// execs ripgrep for `find` and `references` and answers nothing without it, so its absence is
    /// state a caller runs `status` to learn about, reported and never a reason to fail.
    pub(crate) ripgrep: Option<PathBuf>,
    pub(crate) daemon: DaemonStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EngineStatus {
    pub(crate) binary: PathBuf,
    pub(crate) pinned_version: &'static str,
    pub(crate) version: Option<String>,
    pub(crate) compatibility: CompatibilityLabel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompatibilityLabel {
    Supported,
    Untested,
    Unsupported,
    /// The binary could not be run, so there is nothing to compare.
    Unavailable,
}

impl CompatibilityLabel {
    fn text(self) -> &'static str {
        match self {
            CompatibilityLabel::Supported => "supported",
            CompatibilityLabel::Untested => "untested",
            CompatibilityLabel::Unsupported => "unsupported",
            CompatibilityLabel::Unavailable => "unavailable",
        }
    }
}

impl From<Compatibility> for CompatibilityLabel {
    fn from(compatibility: Compatibility) -> Self {
        match compatibility {
            Compatibility::Supported => CompatibilityLabel::Supported,
            Compatibility::Untested => CompatibilityLabel::Untested,
            Compatibility::Unsupported => CompatibilityLabel::Unsupported,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DaemonStatus {
    pub(crate) socket: PathBuf,
    pub(crate) state: DaemonState,
    /// Set only when a live daemon announced a protocol version this build does not match; naming
    /// both versions is what a caller runs `status` to learn before an upgrade replaces it. Absent
    /// from the JSON of a matched daemon, so an ordinary status is byte-identical (KT-113).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) protocol_mismatch: Option<ProtocolMismatchReport>,
    #[serde(flatten)]
    pub(crate) snapshot: Option<DaemonSnapshot>,
}

/// The two protocol versions on a mismatch: the live daemon's, and this build's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ProtocolMismatchReport {
    pub(crate) daemon: u32,
    pub(crate) build: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DaemonState {
    Running,
    /// A socket file exists but nothing listens on it; the next start reclaims it.
    Stale,
    Absent,
}

pub(crate) fn run(root: &Path, format: Format) -> Result<String, CommandError> {
    let report = collect(root)?;
    match format {
        Format::Md => Ok(render_markdown(&report)),
        Format::Json => crate::as_json(&report),
        Format::Dot => Err(CommandError::unsupported_format("status")),
    }
}

pub(crate) fn collect(root: &Path) -> Result<StatusReport, CommandError> {
    let root = crate::daemon::canonical_root(root);
    let kotlin_files = collect_kotlin_files(&root)?.len();
    let socket = crate::daemon::socket_for(&root)?;
    let (engine, daemon) =
        block_on(async { tokio::join!(observe_engine(), observe_daemon(&socket)) });
    Ok(StatusReport {
        root,
        kotlin_files,
        engine,
        ripgrep: locate_ripgrep(std::env::var_os("PATH")),
        daemon,
    })
}

async fn observe_engine() -> EngineStatus {
    let located = ktsense_lsp::locate_binary();
    let reported = ktsense_lsp::reported_version(&located).await;
    let compatibility = reported
        .as_deref()
        .map(|reported| CompatibilityLabel::from(classify(reported)))
        .unwrap_or(CompatibilityLabel::Unavailable);
    EngineStatus {
        binary: resolve_on_path(located, std::env::var_os("PATH")),
        pinned_version: PINNED_UPSTREAM_VERSION,
        version: reported.map(|text| bare_version(&text)),
        compatibility,
    }
}

/// `kmp-lsp --version` prints `kmp-lsp 0.26.0`; the report wants the number, and the classifier
/// accepts either form.
fn bare_version(reported: &str) -> String {
    reported
        .strip_prefix(LSP_BINARY)
        .map(str::trim)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(reported)
        .to_string()
}

/// Discovery falls back to the bare `PATH` name, which spawns fine but tells a reader nothing; the
/// report names the file that name resolves to, when one does.
fn resolve_on_path(binary: PathBuf, path: Option<std::ffi::OsString>) -> PathBuf {
    if binary.components().count() != 1 {
        return binary;
    }
    path.map(|entries| std::env::split_paths(&entries).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(&binary))
        .find(|candidate| candidate.is_file())
        .unwrap_or(binary)
}

/// A plain `PATH` scan for an executable named `rg`, resolved in the CLI rather than the engine so
/// the report can name the file the engine would exec, or say it is missing.
fn locate_ripgrep(path: Option<std::ffi::OsString>) -> Option<PathBuf> {
    path.map(|entries| std::env::split_paths(&entries).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("rg"))
        .find(|candidate| candidate.is_file())
}

/// Probes the socket, then asks a live daemon for its snapshot. A daemon that accepts the
/// connection but does not answer the status method is still reported as running, with no
/// snapshot, rather than as absent: the connection is the fact, the snapshot is a courtesy. A daemon
/// whose announced protocol this build does not match is reported running with that mismatch and no
/// snapshot, since the snapshot request is a shape the versions would disagree about (KT-113).
pub(crate) async fn observe_daemon(socket: &Path) -> DaemonStatus {
    let state = match probe_daemon(socket).await {
        Liveness::Live => DaemonState::Running,
        Liveness::Stale => DaemonState::Stale,
        Liveness::Absent => DaemonState::Absent,
    };
    let (snapshot, protocol_mismatch) = match state {
        DaemonState::Running => observe_running(socket).await,
        DaemonState::Stale | DaemonState::Absent => (None, None),
    };
    DaemonStatus {
        socket: socket.to_path_buf(),
        state,
        protocol_mismatch,
        snapshot,
    }
}

/// Learns a live daemon's protocol version first, then asks for its snapshot only when that version
/// matches this build. A version that does not match is reported rather than queried. The daemon
/// serves one connection at a time, so the version connection is closed before the snapshot
/// connection opens; holding it open would leave the snapshot waiting on a daemon still serving it.
async fn observe_running(
    socket: &Path,
) -> (Option<DaemonSnapshot>, Option<ProtocolMismatchReport>) {
    let announced = AnyClient::connect(socket)
        .await
        .ok()
        .map(|client| client.protocol_version);
    match announced {
        Some(version) if version == PROTOCOL_VERSION => (fetch_snapshot(socket).await, None),
        Some(version) => (
            None,
            Some(ProtocolMismatchReport {
                daemon: version,
                build: PROTOCOL_VERSION,
            }),
        ),
        None => (None, None),
    }
}

async fn fetch_snapshot(socket: &Path) -> Option<DaemonSnapshot> {
    let mut client = Client::connect(socket).await.ok()?;
    let reply = client
        .request(STATUS_METHOD, serde_json::Value::Null)
        .await
        .ok()?;
    serde_json::from_value(reply).ok()
}

pub(crate) fn render_markdown(report: &StatusReport) -> String {
    let mut out = format!("# Status: {}\n\n", report.root.display());
    out.push_str(&format!("kotlin files: {}\n", report.kotlin_files));
    out.push_str(&format!("engine: {}\n", engine_line(&report.engine)));
    out.push_str(&format!("ripgrep: {}\n", ripgrep_line(&report.ripgrep)));
    out.push_str(&daemon_lines(&report.daemon));
    out
}

fn engine_line(engine: &EngineStatus) -> String {
    match &engine.version {
        Some(version) => format!(
            "kmp-lsp {version} at {} (pinned {}, {})",
            engine.binary.display(),
            engine.pinned_version,
            engine.compatibility.text()
        ),
        None => format!(
            "kmp-lsp unavailable at {} (pinned {}); install it or point KTSENSE_LSP_PATH at a build",
            engine.binary.display(),
            engine.pinned_version
        ),
    }
}

/// The engine execs `rg` for `find` and `references`, so a reader who sees those answer nothing
/// needs to know whether ripgrep is even reachable; the line names it or says it is missing.
fn ripgrep_line(ripgrep: &Option<PathBuf>) -> String {
    match ripgrep {
        Some(path) => format!("rg at {}", path.display()),
        None => {
            "not found on PATH; engine find and references return nothing without it".to_string()
        }
    }
}

/// The daemon section, shared with `ktsense daemon status` so the two commands cannot describe
/// one daemon differently.
pub(crate) fn daemon_lines(daemon: &DaemonStatus) -> String {
    let socket = daemon.socket.display();
    match daemon.state {
        DaemonState::Running => {
            let mut lines = format!("daemon: running\nsocket: {socket}\n");
            match &daemon.protocol_mismatch {
                Some(mismatch) => lines.push_str(&format!(
                    "protocol: daemon speaks protocol {}, this build speaks {}\n",
                    mismatch.daemon, mismatch.build
                )),
                None => match &daemon.snapshot {
                    Some(snapshot) => lines.push_str(&format!(
                        "uptime: {}\nindex: {}\nrequests served: {}\nidle limit: {} min\n",
                        humanize(Duration::from_secs(snapshot.uptime_secs)),
                        snapshot.index.text(),
                        snapshot.requests_served,
                        snapshot.idle_limit_secs / 60
                    )),
                    None => lines.push_str("uptime, index and request count: not answered\n"),
                },
            }
            lines
        }
        DaemonState::Stale => format!(
            "daemon: not running\nsocket: {socket} is a stale leftover and will be reclaimed by \
             the next start\n"
        ),
        DaemonState::Absent => format!("daemon: not running\nsocket: {socket} does not exist\n"),
    }
}

fn humanize(uptime: Duration) -> String {
    let secs = uptime.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(daemon: DaemonStatus) -> StatusReport {
        StatusReport {
            root: PathBuf::from("/work/app"),
            kotlin_files: 3,
            engine: EngineStatus {
                binary: PathBuf::from("/opt/kmp-lsp"),
                pinned_version: "0.26.0",
                version: Some("0.26.0".to_string()),
                compatibility: CompatibilityLabel::Supported,
            },
            ripgrep: Some(PathBuf::from("/usr/bin/rg")),
            daemon,
        }
    }

    #[test]
    fn a_running_daemon_renders_its_snapshot_and_json_flattens_it() {
        let running = report(DaemonStatus {
            socket: PathBuf::from("/run/ktsense/abc.sock"),
            state: DaemonState::Running,
            protocol_mismatch: None,
            snapshot: Some(DaemonSnapshot {
                uptime_secs: 3725,
                index: IndexLabel::Complete,
                requests_served: 7,
                idle_limit_secs: 3600,
            }),
        });

        let json: serde_json::Value = serde_json::to_value(&running).expect("serializes");

        assert_eq!(
            (
                render_markdown(&running),
                json["daemon"].clone(),
                json["ripgrep"].clone()
            ),
            (
                "# Status: /work/app\n\nkotlin files: 3\nengine: kmp-lsp 0.26.0 at /opt/kmp-lsp \
                 (pinned 0.26.0, supported)\nripgrep: rg at /usr/bin/rg\ndaemon: running\nsocket: \
                 /run/ktsense/abc.sock\nuptime: 1h 2m\nindex: complete\nrequests served: 7\nidle \
                 limit: 60 min\n"
                    .to_string(),
                serde_json::json!({
                    "socket": "/run/ktsense/abc.sock",
                    "state": "running",
                    "uptime_secs": 3725,
                    "index": "complete",
                    "requests_served": 7,
                    "idle_limit_secs": 3600
                }),
                serde_json::json!("/usr/bin/rg")
            )
        );
    }

    /// A daemon this build does not match is reported running and names both protocol versions, in
    /// Markdown and JSON, in place of the snapshot it cannot ask for. This is what a caller runs
    /// `status` to learn before `daemon stop` or an autostart replaces it (KT-113).
    #[test]
    fn a_mismatched_daemon_names_both_protocol_versions() {
        let mismatched = report(DaemonStatus {
            socket: PathBuf::from("/run/ktsense/abc.sock"),
            state: DaemonState::Running,
            protocol_mismatch: Some(ProtocolMismatchReport {
                daemon: 7,
                build: 8,
            }),
            snapshot: None,
        });

        let json: serde_json::Value = serde_json::to_value(&mismatched).expect("serializes");

        assert_eq!(
            (daemon_lines(&mismatched.daemon), json["daemon"].clone()),
            (
                "daemon: running\nsocket: /run/ktsense/abc.sock\nprotocol: daemon speaks protocol \
                 7, this build speaks 8\n"
                    .to_string(),
                serde_json::json!({
                    "socket": "/run/ktsense/abc.sock",
                    "state": "running",
                    "protocol_mismatch": { "daemon": 7, "build": 8 }
                })
            )
        );
    }

    #[test]
    fn a_missing_engine_and_a_stale_socket_are_reported_rather_than_failing() {
        let mut degraded = report(DaemonStatus {
            socket: PathBuf::from("/run/ktsense/abc.sock"),
            state: DaemonState::Stale,
            protocol_mismatch: None,
            snapshot: None,
        });
        degraded.engine.version = None;
        degraded.engine.compatibility = CompatibilityLabel::Unavailable;
        degraded.ripgrep = None;
        let json: serde_json::Value = serde_json::to_value(&degraded).expect("serializes");

        assert_eq!(
            (render_markdown(&degraded), json["ripgrep"].clone()),
            (
                "# Status: /work/app\n\nkotlin files: 3\nengine: kmp-lsp unavailable at \
                 /opt/kmp-lsp (pinned 0.26.0); install it or point KTSENSE_LSP_PATH at a \
                 build\nripgrep: not found on PATH; engine find and references return nothing \
                 without it\ndaemon: not running\nsocket: /run/ktsense/abc.sock is a stale \
                 leftover and will be reclaimed by the next start\n"
                    .to_string(),
                serde_json::Value::Null
            )
        );
    }

    #[test]
    fn the_reported_version_loses_its_program_name_and_a_bare_binary_resolves_through_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let on_path = dir.path().join("kmp-lsp");
        std::fs::write(&on_path, b"").expect("writes");
        let rg = dir.path().join("rg");
        std::fs::write(&rg, b"").expect("writes");
        let path = std::env::join_paths([dir.path().to_path_buf(), PathBuf::from("/nowhere")])
            .expect("joins");

        assert_eq!(
            (
                bare_version("kmp-lsp 0.26.0"),
                bare_version("0.27.1"),
                bare_version("kmp-lsp"),
                resolve_on_path(PathBuf::from("kmp-lsp"), Some(path.clone())),
                resolve_on_path(PathBuf::from("/opt/kmp-lsp"), Some(path.clone())),
                resolve_on_path(PathBuf::from("kmp-lsp"), None),
                locate_ripgrep(Some(path)),
                locate_ripgrep(None),
            ),
            (
                "0.26.0".to_string(),
                "0.27.1".to_string(),
                "kmp-lsp".to_string(),
                on_path,
                PathBuf::from("/opt/kmp-lsp"),
                PathBuf::from("kmp-lsp"),
                Some(rg),
                None,
            )
        );
    }

    #[test]
    fn uptime_is_humanized_at_three_scales() {
        let rendered: Vec<String> = [59, 61, 3600 * 5 + 60 * 7 + 9]
            .into_iter()
            .map(|secs| humanize(Duration::from_secs(secs)))
            .collect();
        assert_eq!(rendered, ["59s", "1m 1s", "5h 7m"]);
    }
}
