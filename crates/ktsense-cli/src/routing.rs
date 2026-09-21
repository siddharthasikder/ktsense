//! Transparent daemon routing for the commands a warm daemon can answer without the engine.
//!
//! `outline` and `deps` are pure tree-sitter work, so a running daemon answers them by calling the
//! very same functions the in-process path calls; identical output is then a property of the
//! construction, not a hope. The daemon side is [`CommandEngine`], which wraps the warm LSP engine
//! and intercepts one extra method, `ktsense/command`, whose params are the command and its
//! arguments; every other method still reaches the engine. The client side is [`route`], which
//! tries the root's socket first and falls back to running in-process when there is no live
//! daemon, when `KTSENSE_NO_DAEMON=1` is set, or when the transport fails, so a routing problem
//! degrades to a slower answer rather than no answer. A daemon that answers with a command error
//! is not a routing problem: that error is the answer and is reported as such. For debugging and
//! for tests that must prove a daemon answered, `KTSENSE_REQUIRE_DAEMON=1` turns the fallback into
//! a failure.
//!
//! `symbols` is deliberately not routed (Fork A, 2026-09-18): its backend is the engine's
//! command-mode `find`, and `workspace/symbol` is fuzzy and not root-scoped on this engine, so
//! there is nothing a warm session could answer more faithfully than the subprocess already does.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ktsense_core::{DepLevel, RenderOptions};
use ktsense_daemon::{Client, ClientError, Engine, EngineRequest, HandlerOutcome, WarmEngine};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::status::{DaemonSnapshot, STATUS_METHOD};
use crate::{block_on, CommandError, Format};

pub(crate) const NO_DAEMON_ENV: &str = "KTSENSE_NO_DAEMON";
pub(crate) const REQUIRE_DAEMON_ENV: &str = "KTSENSE_REQUIRE_DAEMON";
const COMMAND_METHOD: &str = "ktsense/command";

/// A command a daemon can answer in place of the in-process path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(crate) enum RoutedCommand {
    Outline {
        file: PathBuf,
        private: bool,
        kdoc: bool,
    },
    Deps {
        level: RoutedLevel,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RoutedLevel {
    Package,
    File,
}

impl From<DepLevel> for RoutedLevel {
    fn from(level: DepLevel) -> Self {
        match level {
            DepLevel::Package => RoutedLevel::Package,
            DepLevel::File => RoutedLevel::File,
        }
    }
}

impl From<RoutedLevel> for DepLevel {
    fn from(level: RoutedLevel) -> Self {
        match level {
            RoutedLevel::Package => DepLevel::Package,
            RoutedLevel::File => DepLevel::File,
        }
    }
}

/// The wire shape of a routed request: the command plus the output format, since the daemon
/// renders exactly what the CLI would have printed.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoutedParams {
    #[serde(flatten)]
    command: RoutedCommand,
    format: WireFormat,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireFormat {
    Md,
    Json,
    Dot,
}

impl From<Format> for WireFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Md => WireFormat::Md,
            Format::Json => WireFormat::Json,
            Format::Dot => WireFormat::Dot,
        }
    }
}

impl From<WireFormat> for Format {
    fn from(format: WireFormat) -> Self {
        match format {
            WireFormat::Md => Format::Md,
            WireFormat::Json => Format::Json,
            WireFormat::Dot => Format::Dot,
        }
    }
}

/// Runs a routed command in-process against `root`. Both the daemon fallback and the client
/// fallback call this, so the two paths cannot drift apart. A live daemon answers the same commands
/// through [`CommandEngine::run_cached`], which reuses parsed skeletons but renders identically.
pub(crate) fn run_in_process(
    root: &Path,
    command: &RoutedCommand,
    format: Format,
) -> Result<String, CommandError> {
    match command {
        RoutedCommand::Outline {
            file,
            private,
            kdoc,
        } => crate::outline(
            root,
            &crate::resolve_root(Some(root), file),
            format,
            &render_options(*private, *kdoc),
        ),
        RoutedCommand::Deps { level } => crate::deps(root, DepLevel::from(*level), format),
    }
}

/// The render options a routed `outline` carries, shared by the fresh and cached paths so a knob
/// added to one cannot be forgotten by the other.
fn render_options(private: bool, kdoc: bool) -> RenderOptions {
    let mut options = RenderOptions::default();
    if private {
        options = options.with_private();
    }
    if kdoc {
        options = options.with_doc();
    }
    options
}

/// Answers the command through the root's daemon when one is live and routing is enabled, and
/// in-process otherwise. A transport failure on the daemon path falls back rather than surfacing,
/// because the caller asked a question about the code, not about the daemon; an error the daemon
/// itself reports for the command is the answer to that question and is returned as such.
pub(crate) fn route(
    root: &Path,
    socket: &Path,
    command: RoutedCommand,
    format: Format,
) -> Result<String, CommandError> {
    if flag_set(NO_DAEMON_ENV) {
        return run_in_process(root, &command, format);
    }
    match block_on(ask_daemon(socket, &command, format)) {
        DaemonAnswer::Text(text) => Ok(text),
        DaemonAnswer::CommandFailed(message) => Err(CommandError::routed_failure(message)),
        DaemonAnswer::Unreachable if flag_set(REQUIRE_DAEMON_ENV) => {
            Err(CommandError::no_daemon(root))
        }
        DaemonAnswer::Unreachable => run_in_process(root, &command, format),
    }
}

fn flag_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value == "1")
}

enum DaemonAnswer {
    Text(String),
    CommandFailed(String),
    Unreachable,
}

async fn ask_daemon(socket: &Path, command: &RoutedCommand, format: Format) -> DaemonAnswer {
    let Ok(params) = serde_json::to_value(RoutedParams {
        command: command.clone(),
        format: format.into(),
    }) else {
        return DaemonAnswer::Unreachable;
    };
    let Ok(mut client) = Client::connect(socket).await else {
        return DaemonAnswer::Unreachable;
    };
    match client.request(COMMAND_METHOD, params).await {
        Ok(Value::String(text)) => DaemonAnswer::Text(text),
        Ok(_other_shape) => DaemonAnswer::Unreachable,
        Err(ClientError::Engine { message }) => DaemonAnswer::CommandFailed(message),
        Err(_transport) => DaemonAnswer::Unreachable,
    }
}

/// The daemon-side engine: the warm LSP session for engine methods, plus `ktsense/command` answered
/// by the same in-process functions the CLI uses. A command that fails is an ordinary error reply,
/// so the session lives on and the client falls back.
pub(crate) struct CommandEngine {
    root: PathBuf,
    engine: WarmEngine,
    cache: crate::cache::SkeletonCache,
    started: Instant,
    served: AtomicU64,
}

impl CommandEngine {
    pub(crate) fn new(root: PathBuf, engine: WarmEngine) -> Self {
        Self {
            root,
            engine,
            cache: crate::cache::SkeletonCache::default(),
            started: Instant::now(),
            served: AtomicU64::new(0),
        }
    }

    /// What `ktsense status` shows for this daemon. Status requests are not counted as served: the
    /// count answers "how much work has this daemon done", and looking at it is not work.
    fn snapshot(&self) -> DaemonSnapshot {
        DaemonSnapshot {
            uptime_secs: self.started.elapsed().as_secs(),
            index: self.engine.index_phase().into(),
            requests_served: self.served.load(Ordering::Relaxed),
        }
    }

    fn answer(&self, params: Value) -> HandlerOutcome {
        let routed: RoutedParams = match serde_json::from_value(params) {
            Ok(routed) => routed,
            Err(error) => {
                return HandlerOutcome::Error(format!("malformed command request: {error}"))
            }
        };
        match self.run_cached(&routed.command, routed.format.into()) {
            Ok(text) => HandlerOutcome::Reply(Value::String(text)),
            Err(error) => HandlerOutcome::Error(error.message),
        }
    }

    /// Answers a routed command from the warm skeleton cache. Byte-identical to
    /// [`run_in_process`] by construction: it renders through the same functions and differs only in
    /// reusing a parsed skeleton whose file has not changed since it was parsed.
    fn run_cached(&self, command: &RoutedCommand, format: Format) -> Result<String, CommandError> {
        match command {
            RoutedCommand::Outline {
                file,
                private,
                kdoc,
            } => self.cache.outline(
                &self.root,
                &crate::resolve_root(Some(&self.root), file),
                format,
                &render_options(*private, *kdoc),
            ),
            RoutedCommand::Deps { level } => {
                self.cache.deps(&self.root, DepLevel::from(*level), format)
            }
        }
    }
}

impl Engine for CommandEngine {
    async fn handle(&self, request: EngineRequest) -> HandlerOutcome {
        if request.method == STATUS_METHOD {
            return match serde_json::to_value(self.snapshot()) {
                Ok(value) => HandlerOutcome::Reply(value),
                Err(error) => HandlerOutcome::Error(error.to_string()),
            };
        }
        self.served.fetch_add(1, Ordering::Relaxed);
        if request.method == COMMAND_METHOD {
            return self.answer(request.params);
        }
        self.engine.handle(request).await
    }

    async fn shutdown(self) {
        self.engine.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_commands_round_trip_through_the_wire_shape() {
        let params = RoutedParams {
            command: RoutedCommand::Outline {
                file: PathBuf::from("src/A.kt"),
                private: true,
                kdoc: false,
            },
            format: WireFormat::Json,
        };

        let value = serde_json::to_value(&params).expect("serializes");
        let back: RoutedParams = serde_json::from_value(value.clone()).expect("deserializes");

        assert_eq!(
            (value, back.command, matches!(back.format, WireFormat::Json)),
            (
                serde_json::json!({
                    "command": "outline",
                    "file": "src/A.kt",
                    "private": true,
                    "kdoc": false,
                    "format": "json"
                }),
                params.command,
                true
            )
        );
    }
}
