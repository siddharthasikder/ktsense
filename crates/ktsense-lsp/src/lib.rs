//! Client for the embedded `kmp-lsp` engine.
//!
//! ktsense wraps upstream by process rather than by linking: `kmp-lsp` publishes only a binary
//! target, so there is no library to depend on. The client and the request wrappers land in KT-13
//! and KT-14. This module currently owns the version pin and the binary lookup order, which every
//! later piece needs.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// Upstream version this build of ktsense was developed and tested against.
pub const PINNED_UPSTREAM_VERSION: &str = "0.26.0";

/// Environment variable that overrides binary discovery.
pub const LSP_PATH_ENV: &str = "KTSENSE_LSP_PATH";

/// Name of the upstream engine binary.
pub const LSP_BINARY: &str = "kmp-lsp";

/// How a reported upstream version compares to the pinned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// Same major and minor: the tested combination.
    Supported,
    /// Same major, different minor: usable, worth warning about.
    Untested,
    /// Different major: refuse rather than produce wrong answers.
    Unsupported,
}

/// Classifies a reported upstream version against [`PINNED_UPSTREAM_VERSION`].
pub fn classify(reported: &str) -> Compatibility {
    let pinned = major_minor(PINNED_UPSTREAM_VERSION);
    match major_minor(reported) {
        Some(found) if Some(found) == pinned => Compatibility::Supported,
        Some((major, _)) if pinned.map(|(pinned_major, _)| pinned_major) == Some(major) => {
            Compatibility::Untested
        }
        _ => Compatibility::Unsupported,
    }
}

fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_version_is_supported() {
        assert_eq!(classify(PINNED_UPSTREAM_VERSION), Compatibility::Supported);
    }

    #[test]
    fn version_classification_covers_minor_drift_and_major_breaks() {
        let observed = [
            classify("0.26.0"),
            classify("v0.26.0"),
            classify("0.27.1"),
            classify("1.0.0"),
            classify("not-a-version"),
        ];
        assert_eq!(
            observed,
            [
                Compatibility::Supported,
                Compatibility::Supported,
                Compatibility::Untested,
                Compatibility::Unsupported,
                Compatibility::Unsupported,
            ]
        );
    }

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
