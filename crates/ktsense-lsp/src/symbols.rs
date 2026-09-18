//! Name-to-declaration resolution over the engine's `find --json` command mode.
//!
//! Agents name symbols; LSP position requests want a file and a point. This module bridges the two
//! by driving `kmp-lsp find <name> --json`, which is positionless and builds or reuses the index
//! cache. It was chosen over `workspace/symbol` deliberately: that request returns null on a cold
//! session (KT-49), whereas `find` answers from the CLI index path.
//!
//! The resolver is value-returning and shared with `trace` (KT-18): it never prints and never
//! exits. Ambiguity is an outcome it hands back as [`Resolution::Ambiguous`]; the caller decides
//! whether that is an error, a prompt, or a list. Each candidate can produce the [`FilePosition`]
//! a downstream LSP request needs, which is the piece `trace` consumes.
//!
//! The observed 0.26.0 `find --json` shape is an array of objects each carrying exactly `file`
//! (absolute), `line` and `col` (both 1-based), and `name`. There is no kind, fully-qualified name
//! or signature in this stream; a front-end that wants those enriches each location from the file
//! itself. A name that matches nothing prints nothing and exits non-zero, which is an empty result
//! rather than a failure; a genuine failure carries a message on stderr.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::passthrough::{
    code_label, stderr_snippet, Captured, EngineCommand, PassthroughError,
    DEFAULT_PASSTHROUGH_TIMEOUT,
};
use crate::requests::FilePosition;

/// One declaration location as the engine reported it: the raw `find --json` fields, unenriched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolCandidate {
    pub name: String,
    /// The file exactly as the engine gave it, an absolute path. Display normalization relative to
    /// a workspace root is the caller's concern, so a downstream LSP request keeps a real path.
    pub file: String,
    /// 1-based line, as the engine reports it.
    pub line: u32,
    /// 1-based column, as the engine reports it.
    pub col: u32,
}

impl SymbolCandidate {
    /// The position a downstream LSP request (definition, references, implementation) addresses,
    /// with the engine's 1-based line and column mapped to LSP's 0-based pair. This is the handoff
    /// `trace` builds on so name resolution lives in one place.
    pub fn to_file_position(&self) -> FilePosition {
        FilePosition {
            uri: format!("file://{}", self.file),
            line: self.line.saturating_sub(1),
            character: self.col.saturating_sub(1),
        }
    }
}

/// The outcome of resolving a name, so a caller branches on the shape rather than re-counting a
/// vector. `trace` treats [`Resolution::Ambiguous`] by presenting the candidates; `symbols` exits
/// with its ambiguity code. Neither decision belongs to the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    None,
    Unique(SymbolCandidate),
    Ambiguous(Vec<SymbolCandidate>),
}

impl Resolution {
    /// Classifies engine candidates by count without imposing any policy on the caller.
    pub fn classify(mut candidates: Vec<SymbolCandidate>) -> Self {
        match candidates.len() {
            0 => Resolution::None,
            1 => Resolution::Unique(candidates.pop().expect("length checked as one")),
            _ => Resolution::Ambiguous(candidates),
        }
    }
}

/// Resolves symbol names through the engine's `find` command, bound to a workspace root and a
/// whole-invocation timeout. Grouping the binary, root and bound keeps every call to a single
/// argument and stops a caller transposing the two same-typed paths.
pub struct SymbolResolver<'a> {
    binary: &'a Path,
    root: &'a Path,
    timeout: Duration,
}

impl<'a> SymbolResolver<'a> {
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

    /// Every declaration the engine reports for `query`, in the order it reported them. An empty
    /// vector means the name matched nothing; malformed output is an error, never a silent empty.
    pub async fn find(&self, query: &str) -> Result<Vec<SymbolCandidate>, PassthroughError> {
        let root = self.root.to_string_lossy();
        let captured = EngineCommand::within(self.binary, self.root, self.timeout)
            .capture("find", &["find", query, "--json", "--root", root.as_ref()])
            .await?;
        parse_find(&captured)
    }

    /// Resolves `query` to a single outcome the caller can branch on.
    pub async fn resolve(&self, query: &str) -> Result<Resolution, PassthroughError> {
        Ok(Resolution::classify(self.find(query).await?))
    }
}

/// Locates the engine via the crate's discovery order and lists every declaration matching `query`.
pub async fn run_symbols(
    root: &Path,
    query: &str,
) -> Result<Vec<SymbolCandidate>, PassthroughError> {
    let binary = crate::locate_binary();
    SymbolResolver::new(&binary, root).find(query).await
}

/// Locates the engine via the crate's discovery order and resolves `query` to one outcome.
pub async fn resolve_symbol(root: &Path, query: &str) -> Result<Resolution, PassthroughError> {
    let binary = crate::locate_binary();
    SymbolResolver::new(&binary, root).resolve(query).await
}

#[derive(Deserialize)]
struct RawFind {
    file: String,
    line: u32,
    col: u32,
    name: String,
}

fn parse_find(captured: &Captured) -> Result<Vec<SymbolCandidate>, PassthroughError> {
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // A name that matches no declaration prints nothing and exits non-zero. That is an empty
        // result, not a failure, so long as the engine also stayed silent on stderr; a real
        // failure (an unreadable root, say) announces itself there and must not read as "no match".
        if stderr_snippet(&captured.stderr) == "no stderr" {
            return Ok(Vec::new());
        }
        return Err(PassthroughError::Failed {
            command: "find".to_string(),
            code: code_label(captured.code),
            stderr: stderr_snippet(&captured.stderr),
        });
    }
    let raw: Vec<RawFind> =
        serde_json::from_str(trimmed).map_err(|err| PassthroughError::Unparseable {
            command: "find".to_string(),
            detail: format!("{err}; engine stderr: {}", stderr_snippet(&captured.stderr)),
        })?;
    Ok(raw
        .into_iter()
        .map(|entry| SymbolCandidate {
            name: entry.name,
            file: entry.file,
            line: entry.line,
            col: entry.col,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn candidate(name: &str, file: &str, line: u32, col: u32) -> SymbolCandidate {
        SymbolCandidate {
            name: name.to_string(),
            file: file.to_string(),
            line,
            col,
        }
    }

    #[test]
    fn a_json_array_becomes_candidates_and_classifies_by_count() {
        let one = r#"[{"file":"/r/A.kt","line":3,"col":1,"name":"A"}]"#;
        let many = r#"[{"file":"/r/A.kt","line":4,"col":5,"name":"save"},
                       {"file":"/r/B.kt","line":8,"col":14,"name":"save"}]"#;

        let observed = (
            parse_find(&captured(0, one, "")).unwrap(),
            Resolution::classify(parse_find(&captured(0, one, "")).unwrap()),
            Resolution::classify(parse_find(&captured(0, many, "")).unwrap()),
        );

        assert_eq!(
            observed,
            (
                vec![candidate("A", "/r/A.kt", 3, 1)],
                Resolution::Unique(candidate("A", "/r/A.kt", 3, 1)),
                Resolution::Ambiguous(vec![
                    candidate("save", "/r/A.kt", 4, 5),
                    candidate("save", "/r/B.kt", 8, 14),
                ]),
            )
        );
    }

    #[test]
    fn silence_on_both_streams_is_an_empty_result_not_a_failure() {
        let observed = parse_find(&captured(1, "", ""));
        assert_eq!(observed.unwrap(), Vec::new());
    }

    #[test]
    fn noise_or_a_stderr_message_is_diagnosed_rather_than_read_as_empty() {
        let noisy = parse_find(&captured(0, "[INFO kmp_lsp] indexing\nnot json", ""));
        let errored = parse_find(&captured(2, "", "error: root does not exist"));

        let observed = (
            matches!(noisy, Err(PassthroughError::Unparseable { .. })),
            matches!(errored, Err(PassthroughError::Failed { .. })),
        );
        assert_eq!(
            observed,
            (true, true),
            "noisy={noisy:?} errored={errored:?}"
        );
    }

    #[test]
    fn a_candidate_maps_to_a_zero_based_lsp_position_with_a_file_uri() {
        assert_eq!(
            candidate("save", "/r/A.kt", 4, 5).to_file_position(),
            FilePosition {
                uri: "file:///r/A.kt".to_string(),
                line: 3,
                character: 4,
            }
        );
    }
}
