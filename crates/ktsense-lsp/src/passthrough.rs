//! Command-mode passthrough to the `kmp-lsp` engine for `check` and `diagnose`.
//!
//! These are one-shot child invocations, not LSP sessions: the engine runs the subcommand, writes
//! its answer to stdout, and exits. The adapter spawns the child bounded, closes its stdin, drains
//! both pipes without deadlock, and normalizes the answer into the stable values a front-end
//! renders. Binary discovery and the version pin are reused from the crate root rather than
//! duplicated.
//!
//! Two upstream behaviours in 0.26.0 diverge from the naive expectation and shape the contract:
//! `check --json` emits a JSON object and exits 1 when a file has errors, but `diagnose` ignores
//! `--json`, prints human `line:col [severity]: message` lines, and exits 0 even when it reports
//! findings, reserving a non-zero exit for a genuine failure such as an unreadable file. The
//! adapter therefore parses each command's real shape rather than assuming a shared one.

use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

/// Whole-invocation bound. Generous because `diagnose` may build the workspace index on a cold
/// cache, yet finite so a wedged engine cannot stall the CLI.
pub const DEFAULT_PASSTHROUGH_TIMEOUT: Duration = Duration::from_secs(60);

const NO_DIAGNOSTICS: &str = "No diagnostics.";

/// One syntax error reported by `check`, with its file normalized relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyntaxError {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub message: String,
}

/// The normalized result of `kmp-lsp check <path> --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    pub files_ok: u32,
    pub files_with_errors: u32,
    pub errors: Vec<SyntaxError>,
}

impl CheckReport {
    /// Whether the engine found at least one broken file, which the CLI turns into a failing exit.
    pub fn has_errors(&self) -> bool {
        self.files_with_errors > 0 || !self.errors.is_empty()
    }
}

/// Severity of a single `diagnose` finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One finding reported by `diagnose`, positioned within the analyzed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub line: u32,
    pub col: u32,
    pub severity: Severity,
    pub message: String,
}

/// The normalized result of `kmp-lsp diagnose <file> --root <root>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiagnoseReport {
    pub file: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// Everything that can go wrong invoking the engine in command mode. Every variant maps to the
/// CLI's failure exit; usage errors never reach here because the argv is fixed, not user-supplied.
#[derive(Debug, Error)]
pub enum PassthroughError {
    #[error("failed to spawn engine `{path}`: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("i/o error running engine `{command}`: {source}")]
    Io {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("engine `{command}` did not respond within {timeout:?}")]
    Timeout { command: String, timeout: Duration },
    #[error("engine `{command}` failed ({code}): {stderr}")]
    Failed {
        command: String,
        code: String,
        stderr: String,
    },
    #[error("could not parse engine `{command}` output: {detail}")]
    Unparseable { command: String, detail: String },
}

/// A located engine binary bound to a workspace root, from which `check` and `diagnose` run under
/// a shared timeout. Grouping the binary, root, and bound keeps every call to two arguments or
/// fewer and keeps a caller from transposing same-typed paths.
pub struct EngineCommand<'a> {
    binary: &'a Path,
    root: &'a Path,
    timeout: Duration,
}

impl<'a> EngineCommand<'a> {
    /// Binds the engine at `binary` to `root` with the default timeout.
    pub fn new(binary: &'a Path, root: &'a Path) -> Self {
        Self {
            binary,
            root,
            timeout: DEFAULT_PASSTHROUGH_TIMEOUT,
        }
    }

    /// Binds with an explicit timeout, used by tests and by callers tuning the bound.
    pub fn within(binary: &'a Path, root: &'a Path, timeout: Duration) -> Self {
        Self {
            binary,
            root,
            timeout,
        }
    }

    /// Runs `check <path> --json --root <root>` and normalizes its JSON report.
    pub async fn check(&self, path: &Path) -> Result<CheckReport, PassthroughError> {
        let path = path.to_string_lossy();
        let root = self.root.to_string_lossy();
        let captured = self
            .capture(
                "check",
                &["check", path.as_ref(), "--json", "--root", root.as_ref()],
            )
            .await?;
        parse_check(self.root, &captured)
    }

    /// Runs `diagnose <file> --root <root>` and normalizes its line-oriented report.
    pub async fn diagnose(&self, file: &Path) -> Result<DiagnoseReport, PassthroughError> {
        let file = file.to_string_lossy();
        let root = self.root.to_string_lossy();
        let captured = self
            .capture(
                "diagnose",
                &["diagnose", file.as_ref(), "--root", root.as_ref()],
            )
            .await?;
        parse_diagnose(normalize(self.root, &file), &captured)
    }

    async fn capture(&self, label: &str, args: &[&str]) -> Result<Captured, PassthroughError> {
        let mut command = Command::new(self.binary);
        command
            .args(args)
            // kmp-lsp writes env_logger INFO lines onto its output unless RUST_LOG=error; keep
            // them out so parsing sees only the command's answer. (see AGENTS.md)
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|source| PassthroughError::Spawn {
            path: self.binary.display().to_string(),
            source,
        })?;
        match timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(Captured::from(output)),
            Ok(Err(source)) => Err(PassthroughError::Io {
                command: label.to_string(),
                source,
            }),
            Err(_elapsed) => Err(PassthroughError::Timeout {
                command: label.to_string(),
                timeout: self.timeout,
            }),
        }
    }
}

/// Locates the engine via the crate's discovery order and runs `check` against `path`.
pub async fn run_check(root: &Path, path: &Path) -> Result<CheckReport, PassthroughError> {
    let binary = crate::locate_binary();
    EngineCommand::new(&binary, root).check(path).await
}

/// Locates the engine via the crate's discovery order and runs `diagnose` against `file`.
pub async fn run_diagnose(root: &Path, file: &Path) -> Result<DiagnoseReport, PassthroughError> {
    let binary = crate::locate_binary();
    EngineCommand::new(&binary, root).diagnose(file).await
}

struct Captured {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl From<std::process::Output> for Captured {
    fn from(output: std::process::Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

#[derive(Deserialize)]
struct RawCheck {
    errors: Vec<RawSyntaxError>,
    files_ok: u32,
    files_with_errors: u32,
}

#[derive(Deserialize)]
struct RawSyntaxError {
    file: String,
    line: u32,
    col: u32,
    message: String,
}

fn parse_check(root: &Path, captured: &Captured) -> Result<CheckReport, PassthroughError> {
    let raw: RawCheck =
        serde_json::from_slice(&captured.stdout).map_err(|err| PassthroughError::Unparseable {
            command: "check".to_string(),
            detail: unparse_detail(&err.to_string(), &captured.stderr),
        })?;
    let errors = raw
        .errors
        .into_iter()
        .map(|error| SyntaxError {
            file: normalize(root, &error.file),
            line: error.line,
            col: error.col,
            message: error.message,
        })
        .collect();
    Ok(CheckReport {
        files_ok: raw.files_ok,
        files_with_errors: raw.files_with_errors,
        errors,
    })
}

fn parse_diagnose(file: String, captured: &Captured) -> Result<DiagnoseReport, PassthroughError> {
    if captured.code != Some(0) {
        return Err(PassthroughError::Failed {
            command: "diagnose".to_string(),
            code: code_label(captured.code),
            stderr: stderr_snippet(&captured.stderr),
        });
    }
    let text = String::from_utf8_lossy(&captured.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == NO_DIAGNOSTICS {
        return Ok(DiagnoseReport {
            file,
            diagnostics: Vec::new(),
        });
    }
    let mut diagnostics = Vec::new();
    for line in trimmed.lines() {
        let diagnostic =
            parse_diagnostic_line(line).ok_or_else(|| PassthroughError::Unparseable {
                command: "diagnose".to_string(),
                detail: format!("unrecognized diagnostic line: {line:?}"),
            })?;
        diagnostics.push(diagnostic);
    }
    Ok(DiagnoseReport { file, diagnostics })
}

fn parse_diagnostic_line(line: &str) -> Option<Diagnostic> {
    let (position, rest) = line.split_once(" [")?;
    let (severity, message) = rest.split_once("]: ")?;
    let (line_text, col_text) = position.split_once(':')?;
    Some(Diagnostic {
        line: line_text.trim().parse().ok()?,
        col: col_text.trim().parse().ok()?,
        severity: parse_severity(severity)?,
        message: message.to_string(),
    })
}

fn parse_severity(text: &str) -> Option<Severity> {
    match text {
        "error" => Some(Severity::Error),
        "warning" => Some(Severity::Warning),
        "info" | "information" => Some(Severity::Info),
        _ => None,
    }
}

/// The path as it should appear in output: relative to the workspace root and always
/// `/`-separated, so the same file yields the same report on every filesystem.
fn normalize(root: &Path, file: &str) -> String {
    let path = Path::new(file);
    let relative = path.strip_prefix(root).unwrap_or(path);
    let joined = relative
        .components()
        .filter(|component| !matches!(component, Component::RootDir))
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if relative.has_root() {
        format!("/{joined}")
    } else {
        joined
    }
}

fn code_label(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit {code}"),
        None => "terminated by signal".to_string(),
    }
}

fn stderr_snippet(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "no stderr".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unparse_detail(error: &str, stderr: &[u8]) -> String {
    let noise = stderr_snippet(stderr);
    format!("{error}; engine stderr: {noise}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(code: i32, stdout: &str) -> Captured {
        Captured {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn a_path_outside_the_root_stays_absolute_without_a_doubled_separator() {
        let root = Path::new("/repo");
        let observed = (
            normalize(root, "/repo/src/When.kt"),
            normalize(root, "/elsewhere/Broken.kt"),
            normalize(root, "src/When.kt"),
        );

        assert_eq!(
            observed,
            (
                "src/When.kt".to_string(),
                "/elsewhere/Broken.kt".to_string(),
                "src/When.kt".to_string(),
            ),
            "observed={observed:?}"
        );
    }

    #[test]
    fn check_json_is_normalized_with_root_relative_paths() {
        let root = Path::new("/repo");
        let stdout = r#"{"errors":[{"col":1,"file":"/repo/src/A.kt","line":2,"message":"boom"}],"files_ok":0,"files_with_errors":1}"#;

        assert_eq!(
            parse_check(root, &captured(1, stdout)).unwrap(),
            CheckReport {
                files_ok: 0,
                files_with_errors: 1,
                errors: vec![SyntaxError {
                    file: "src/A.kt".to_string(),
                    line: 2,
                    col: 1,
                    message: "boom".to_string(),
                }],
            }
        );
    }

    #[test]
    fn noisy_or_malformed_check_output_is_a_parse_error_not_a_silent_empty_report() {
        let observed = parse_check(Path::new("/repo"), &captured(0, "INFO indexing\nnot json"));
        assert!(
            matches!(observed, Err(PassthroughError::Unparseable { .. })),
            "was {observed:?}"
        );
    }

    #[test]
    fn diagnose_lines_and_the_clean_sentinel_map_to_findings_or_none() {
        let root_file = "src/When.kt".to_string();
        let findings = parse_diagnose(
            root_file.clone(),
            &captured(
                0,
                "2:1 [error]: unexpected `fun`\n3:15 [warning]: missing branch: B\n",
            ),
        )
        .unwrap();
        let clean = parse_diagnose(root_file.clone(), &captured(0, "No diagnostics.\n")).unwrap();

        assert_eq!(
            (findings, clean),
            (
                DiagnoseReport {
                    file: root_file.clone(),
                    diagnostics: vec![
                        Diagnostic {
                            line: 2,
                            col: 1,
                            severity: Severity::Error,
                            message: "unexpected `fun`".to_string(),
                        },
                        Diagnostic {
                            line: 3,
                            col: 15,
                            severity: Severity::Warning,
                            message: "missing branch: B".to_string(),
                        },
                    ],
                },
                DiagnoseReport {
                    file: root_file,
                    diagnostics: Vec::new(),
                }
            )
        );
    }

    #[test]
    fn diagnose_nonzero_exit_is_a_failure_and_unreadable_lines_do_not_parse() {
        let failed = parse_diagnose("x.kt".to_string(), &captured(1, ""));
        let malformed =
            parse_diagnose("x.kt".to_string(), &captured(0, "garbage without position"));

        let observed = (
            matches!(failed, Err(PassthroughError::Failed { .. })),
            matches!(malformed, Err(PassthroughError::Unparseable { .. })),
        );
        assert_eq!(
            observed,
            (true, true),
            "failed={failed:?} malformed={malformed:?}"
        );
    }
}
