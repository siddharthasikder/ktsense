//! Upstream version pin and startup compatibility check.
//!
//! ktsense is developed against a single pinned `kmp-lsp` version. At launch the engine binary is
//! run once with `--version`, its report is classified against the pin, and the result becomes a
//! [`VersionCheck`] the launcher acts on: proceed, proceed with a warning, or refuse. Only a
//! differing major version is a hard refusal; every uncertain case proceeds with a visible warning
//! rather than blocking a possibly-working engine.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Upstream version this build of ktsense was developed and tested against.
pub const PINNED_UPSTREAM_VERSION: &str = "0.26.0";

/// The ktsense version reported by `ktsense --version` and by the MCP server's `serverInfo`, held
/// here so both front-ends read one string. Distinct from [`PINNED_UPSTREAM_VERSION`], which is the
/// engine's version, not ktsense's.
///
/// A development build falls back to the workspace manifest version (`0.0.0`), which is never edited
/// to cut a release. A release build exports `KTSENSE_BUILD_VERSION` at compile time and this reports
/// that value instead, so no source is mutated per tag. Cargo records environment variables read
/// through `option_env!` in its dep-info, so changing `KTSENSE_BUILD_VERSION` between builds forces a
/// recompile rather than serving a stale string from cache.
pub const KTSENSE_VERSION: &str = match option_env!("KTSENSE_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How a reported upstream version compares to [`PINNED_UPSTREAM_VERSION`], on major.minor alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// Same major and minor: the tested combination.
    Supported,
    /// Same major, different minor: usable, worth warning about.
    Untested,
    /// Different major, or unparseable: cannot be trusted.
    Unsupported,
}

/// The startup decision derived from probing `kmp-lsp --version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionCheck {
    /// Reported version matches the pin; start silently.
    Compatible,
    /// Start the engine, but surface this diagnostic to the user first.
    Warn(String),
    /// Do not start the engine; surface this diagnostic as the failure.
    Refuse(String),
}

/// Classifies a reported version string against [`PINNED_UPSTREAM_VERSION`] on major.minor.
pub fn classify(reported: &str) -> Compatibility {
    parse(reported).map_or(Compatibility::Unsupported, |found| compare(&found))
}

/// Probes `binary --version` with the default timeout and turns the result into a [`VersionCheck`].
pub async fn check_version(binary: &Path) -> VersionCheck {
    check_version_within(binary, DEFAULT_PROBE_TIMEOUT).await
}

/// Probes `binary --version` under an explicit whole-probe timeout.
pub async fn check_version_within(binary: &Path, bound: Duration) -> VersionCheck {
    assess(&probe(binary, bound).await)
}

/// The version string `binary --version` reports, or `None` when the binary is missing, fails, or
/// prints nothing. For reporting, not for gating: [`check_version`] decides whether to start.
pub async fn reported_version(binary: &Path) -> Option<String> {
    match probe(binary, DEFAULT_PROBE_TIMEOUT).await {
        VersionProbe::Reported(version) => Some(version),
        VersionProbe::NoOutput | VersionProbe::Failed(_) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionProbe {
    Reported(String),
    NoOutput,
    Failed(String),
}

struct Semver {
    major: u32,
    minor: u32,
    prerelease: bool,
}

async fn probe(binary: &Path, bound: Duration) -> VersionProbe {
    let mut command = Command::new(binary);
    command
        // kmp-lsp writes env_logger lines unless RUST_LOG=error; keep them out of the probe output
        // so the version line is not buried. (see AGENTS.md)
        .env("RUST_LOG", "error")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return VersionProbe::Failed(err.to_string()),
    };
    match timeout(bound, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => reported_or_empty(&output.stdout),
        Ok(Ok(output)) => {
            VersionProbe::Failed(format!("`--version` exited with {}", output.status))
        }
        Ok(Err(err)) => VersionProbe::Failed(err.to_string()),
        Err(_elapsed) => {
            VersionProbe::Failed(format!("`--version` did not respond within {bound:?}"))
        }
    }
}

fn reported_or_empty(stdout: &[u8]) -> VersionProbe {
    let text = String::from_utf8_lossy(stdout).trim().to_string();
    if text.is_empty() {
        VersionProbe::NoOutput
    } else {
        VersionProbe::Reported(text)
    }
}

fn assess(probe: &VersionProbe) -> VersionCheck {
    match probe {
        VersionProbe::Reported(line) => assess_reported(line),
        VersionProbe::NoOutput => VersionCheck::Warn(no_output_message()),
        VersionProbe::Failed(reason) => VersionCheck::Warn(probe_failed_message(reason)),
    }
}

fn assess_reported(reported: &str) -> VersionCheck {
    match parse(reported) {
        None => VersionCheck::Warn(unparsed_message(reported)),
        Some(found) => match compare(&found) {
            Compatibility::Unsupported => VersionCheck::Refuse(major_mismatch_message(reported)),
            Compatibility::Untested => VersionCheck::Warn(minor_drift_message(reported)),
            Compatibility::Supported if found.prerelease => {
                VersionCheck::Warn(prerelease_message(reported))
            }
            Compatibility::Supported => VersionCheck::Compatible,
        },
    }
}

fn compare(found: &Semver) -> Compatibility {
    let pin = parse(PINNED_UPSTREAM_VERSION).expect("pinned version parses");
    if found.major != pin.major {
        Compatibility::Unsupported
    } else if found.minor != pin.minor {
        Compatibility::Untested
    } else {
        Compatibility::Supported
    }
}

fn parse(reported: &str) -> Option<Semver> {
    // The real `--version` line is `kmp-lsp 0.26.0`, name then version, so take the first token
    // that looks like a version rather than assuming the whole string is one. (see KT-26 notes)
    let token = reported
        .split_whitespace()
        .find(|piece| looks_like_version(piece))?;
    let core = token.trim_start_matches('v');
    let core_end = core.find(['-', '+']).unwrap_or(core.len());
    let mut numbers = core[..core_end].split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers.next()?.parse().ok()?;
    Some(Semver {
        major,
        minor,
        prerelease: core_end < core.len(),
    })
}

fn looks_like_version(piece: &str) -> bool {
    let core = piece.trim_start_matches('v');
    piece.contains('.') && core.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

fn major_mismatch_message(reported: &str) -> String {
    format!(
        "kmp-lsp reports {reported:?}, but ktsense {PINNED_UPSTREAM_VERSION} needs a matching major \
         version. Refusing to start to avoid wrong answers; install kmp-lsp \
         {PINNED_UPSTREAM_VERSION} or point KTSENSE_LSP_PATH at a compatible build."
    )
}

fn minor_drift_message(reported: &str) -> String {
    format!(
        "kmp-lsp reports {reported:?}; ktsense was tested against {PINNED_UPSTREAM_VERSION}. \
         Continuing, but results may differ where the protocol has changed."
    )
}

fn prerelease_message(reported: &str) -> String {
    format!(
        "kmp-lsp reports prerelease {reported:?}; ktsense was tested against the released \
         {PINNED_UPSTREAM_VERSION}. Continuing, but prerelease builds are untested."
    )
}

fn unparsed_message(reported: &str) -> String {
    format!(
        "could not read a version from kmp-lsp --version output {reported:?}; expected something \
         like {PINNED_UPSTREAM_VERSION:?}. Continuing without a compatibility check."
    )
}

fn no_output_message() -> String {
    format!(
        "kmp-lsp --version produced no output, so its version could not be checked against \
         {PINNED_UPSTREAM_VERSION}. Continuing without a compatibility check."
    )
}

fn probe_failed_message(reason: &str) -> String {
    format!(
        "could not run kmp-lsp --version ({reason}); skipping the compatibility check against \
         {PINNED_UPSTREAM_VERSION}. Continuing."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(probe: VersionProbe) -> VersionCheck {
        assess(&probe)
    }

    #[test]
    fn ktsense_version_falls_back_to_the_cargo_version_without_a_build_override() {
        assert_eq!(
            (KTSENSE_VERSION, option_env!("KTSENSE_BUILD_VERSION")),
            (env!("CARGO_PKG_VERSION"), None)
        );
    }

    #[test]
    fn classifies_major_minor_drift_and_ignores_name_prefix_patch_and_case() {
        let observed = [
            classify(PINNED_UPSTREAM_VERSION),
            classify("kmp-lsp 0.26.0"),
            classify("v0.26.0"),
            classify("0.26.99"),
            classify("0.27.1"),
            classify("1.0.0"),
            classify("not-a-version"),
        ];
        assert_eq!(
            observed,
            [
                Compatibility::Supported,
                Compatibility::Supported,
                Compatibility::Supported,
                Compatibility::Supported,
                Compatibility::Untested,
                Compatibility::Unsupported,
                Compatibility::Unsupported,
            ]
        );
    }

    #[test]
    fn every_probe_outcome_maps_to_its_documented_verdict_and_message() {
        let observed = [
            verdict(VersionProbe::Reported("kmp-lsp 0.26.0".to_string())),
            verdict(VersionProbe::Reported("kmp-lsp 0.26.7".to_string())),
            verdict(VersionProbe::Reported("kmp-lsp 0.27.0".to_string())),
            verdict(VersionProbe::Reported("kmp-lsp 2.0.0".to_string())),
            verdict(VersionProbe::Reported("kmp-lsp 0.26.0-rc1".to_string())),
            verdict(VersionProbe::Reported("kmp-lsp 3.0.0-rc1".to_string())),
            verdict(VersionProbe::Reported("garbage".to_string())),
            verdict(VersionProbe::NoOutput),
            verdict(VersionProbe::Failed("No such file".to_string())),
        ];
        let expected = [
            VersionCheck::Compatible,
            VersionCheck::Compatible,
            VersionCheck::Warn(minor_drift_message("kmp-lsp 0.27.0")),
            VersionCheck::Refuse(major_mismatch_message("kmp-lsp 2.0.0")),
            VersionCheck::Warn(prerelease_message("kmp-lsp 0.26.0-rc1")),
            VersionCheck::Refuse(major_mismatch_message("kmp-lsp 3.0.0-rc1")),
            VersionCheck::Warn(unparsed_message("garbage")),
            VersionCheck::Warn(no_output_message()),
            VersionCheck::Warn(probe_failed_message("No such file")),
        ];
        assert_eq!(observed, expected);
    }
}
