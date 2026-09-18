//! Client for the embedded `kmp-lsp` engine.
//!
//! ktsense wraps upstream by process rather than by linking: `kmp-lsp` publishes only a binary
//! target, so there is no library to depend on. This crate owns the version pin, the binary lookup
//! order, the stdio framing codec, and the async [`LspClient`] that drives the child process. Typed
//! request wrappers and `$/progress` index tracking layer on top.

#![forbid(unsafe_code)]

mod client;
mod framing;
mod progress;
mod requests;
mod version;

pub use client::{InitializeConfig, LspClient, LspError, Notification, Teardown};
pub use framing::FramingError;
pub use progress::IndexPhase;
pub use requests::{DeclarationScope, FilePosition};
pub use version::{
    check_version, check_version_within, classify, Compatibility, VersionCheck,
    PINNED_UPSTREAM_VERSION,
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
/// supplies the environment override and the executable path, keeping this decision pure.
pub fn discovery_order(
    env_override: Option<PathBuf>,
    current_exe: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(from_env) = env_override {
        candidates.push(from_env);
    }
    if let Some(exe) = current_exe {
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

fn locate_binary() -> PathBuf {
    let candidates = discovery_order(
        std::env::var_os(LSP_PATH_ENV).map(PathBuf::from),
        std::env::current_exe().ok(),
    );
    let fallback = candidates
        .last()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(LSP_BINARY));
    candidates
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or(fallback)
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
