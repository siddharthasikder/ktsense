//! Client for the embedded `kmp-lsp` engine.
//!
//! ktsense wraps upstream by process rather than by linking: `kmp-lsp` publishes only a binary
//! target, so there is no library to depend on. This crate owns the version pin, the binary lookup
//! order, the stdio framing codec, and the async [`LspClient`] that drives the child process. Typed
//! request wrappers and `$/progress` index tracking layer on top.

#![forbid(unsafe_code)]

mod client;
mod framing;
mod passthrough;
mod progress;
mod requests;
mod symbols;
mod version;

pub use client::{InitializeConfig, LspClient, LspError, Notification, Teardown};
pub use framing::FramingError;
pub use passthrough::{
    run_check, run_diagnose, CheckReport, DiagnoseReport, Diagnostic, EngineCommand,
    PassthroughError, Severity, SyntaxError, DEFAULT_PASSTHROUGH_TIMEOUT,
};
pub use progress::{wait_for_index, IndexPhase, IndexWait};
pub use requests::{uri_to_path, DeclarationScope, FilePosition, SiteLocation};
pub use symbols::{
    name_column, name_column_near, resolve_symbol, run_symbols, ReportedPosition, Resolution,
    SymbolCandidate, SymbolResolver,
};
pub use version::{
    check_version, check_version_within, classify, reported_version, Compatibility, VersionCheck,
    KTSENSE_VERSION, PINNED_UPSTREAM_VERSION,
};

use std::path::PathBuf;

/// Environment variable that overrides binary discovery.
pub const LSP_PATH_ENV: &str = "KTSENSE_LSP_PATH";

/// Name of the upstream engine binary.
pub const LSP_BINARY: &str = "kmp-lsp";

/// Candidate paths for the engine binary, in the order ktsense tries them.
///
/// The `libexec` entry is what a Homebrew install resolves to, so a brewed ktsense finds the
/// upstream binary shipped alongside it before falling back to whatever is on `PATH`. The caller
/// supplies the environment override and the executable path, keeping this decision pure; pass the
/// real executable rather than a link to it, as [`locate_binary_from`] does.
pub fn discovery_order(env_override: Option<PathBuf>, real_exe: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(from_env) = env_override {
        candidates.push(from_env);
    }
    if let Some(exe) = real_exe {
        if let Some(bin_dir) = exe.parent() {
            candidates.push(bin_dir.join("../libexec").join(LSP_BINARY));
        }
    }
    candidates.push(PathBuf::from(LSP_BINARY));
    candidates
}

/// Locates the engine binary via [`discovery_order`] and spawns a client against it.
///
/// Before starting an LSP session, the located binary is probed with `--version` and classified
/// against [`PINNED_UPSTREAM_VERSION`]: a differing major version is refused so ktsense never
/// produces answers from an incompatible protocol, while every other divergence proceeds with a
/// warning. The refusal happens before any session child is spawned, so a rejected engine leaves
/// no process behind.
pub async fn launch() -> Result<LspClient, LspError> {
    let binary = locate_binary();
    match check_version(&binary).await {
        VersionCheck::Refuse(message) => Err(LspError::Incompatible { message }),
        VersionCheck::Warn(message) => {
            tracing::warn!("{message}");
            LspClient::spawn(&binary).await
        }
        VersionCheck::Compatible => LspClient::spawn(&binary).await,
    }
}

/// The engine binary ktsense will use: the first existing candidate of [`discovery_order`], or the
/// bare `PATH` name when none exists so the eventual spawn error names what was looked for.
pub fn locate_binary() -> PathBuf {
    locate_binary_from(
        std::env::var_os(LSP_PATH_ENV).map(PathBuf::from),
        std::env::current_exe().ok(),
    )
}

/// [`locate_binary`] as a function of its two inputs, so discovery can be driven through a given
/// install layout without touching the environment of the process asking.
///
/// `current_exe` is resolved to the real file before the `libexec` candidate is derived from it.
/// Homebrew keeps the real binary in the Cellar and links it into the prefix `bin`, while the
/// release archive keeps `libexec/kmp-lsp` beside the real binary, not beside the link. On macOS
/// `current_exe` reports the link it was invoked through, so deriving from the link would look for a
/// prefix-level `libexec` that Homebrew never creates: the Cellar-path invocation the formula test
/// uses would find its engine while a normal `PATH` invocation would not.
pub fn locate_binary_from(env_override: Option<PathBuf>, current_exe: Option<PathBuf>) -> PathBuf {
    let candidates = discovery_order(env_override, current_exe.map(real_executable));
    let fallback = candidates
        .last()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(LSP_BINARY));
    candidates
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or(fallback)
}

/// The real file behind an executable path, keeping the path as given when it cannot be resolved so
/// an unresolvable executable still contributes the candidate it always did and `PATH` stays the
/// honest last resort.
fn real_executable(exe: PathBuf) -> PathBuf {
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_prefers_an_override_then_libexec_then_path() {
        let order = discovery_order(
            Some(PathBuf::from("/custom/kmp-lsp")),
            Some(PathBuf::from("/opt/brew/bin/ktsense")),
        );
        assert_eq!(
            order,
            vec![
                PathBuf::from("/custom/kmp-lsp"),
                PathBuf::from("/opt/brew/bin/../libexec/kmp-lsp"),
                PathBuf::from("kmp-lsp"),
            ]
        );
    }
}
