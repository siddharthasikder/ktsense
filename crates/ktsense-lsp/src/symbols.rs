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

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::passthrough::{
    code_label, stderr_snippet, Captured, EngineCommand, PassthroughError,
    DEFAULT_PASSTHROUGH_TIMEOUT,
};
use crate::requests::FilePosition;

/// One declaration location from the engine's `find --json`. The fields are the engine's own,
/// except `col`, which [`SymbolResolver::find`] corrects to the declaration name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolCandidate {
    pub name: String,
    /// The file exactly as the engine gave it, an absolute path. Display normalization relative to
    /// a workspace root is the caller's concern, so a downstream LSP request keeps a real path.
    pub file: String,
    /// 1-based line, as the engine reports it.
    pub line: u32,
    /// 1-based column of the declaration name. [`SymbolResolver::find`] corrects the column
    /// `find --json` reports on a cold cache, where it can point at the keyword before the name
    /// (`fun save` at the `fun`); the engine's own column is kept when the source cannot be read.
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
        Ok(correct_columns(parse_find(&captured)?, |path| {
            std::fs::read_to_string(path).ok()
        }))
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

/// Corrects every candidate's column to its declaration name, reading each unique file at most once
/// per call. A file is read through `read` the first time a candidate names it and the result, text
/// or failure, is reused for every later candidate sharing that path, so an ambiguous match across
/// three overrides of one file reads it once, not three times. The engine's reported column is kept
/// when the source cannot be read or the name is absent, so a location ktsense cannot verify stays
/// honest. The cache lives only for this call, so no read outlives the request that made it.
fn correct_columns<F>(candidates: Vec<SymbolCandidate>, mut read: F) -> Vec<SymbolCandidate>
where
    F: FnMut(&Path) -> Option<String>,
{
    let mut sources: HashMap<String, Option<String>> = HashMap::new();
    candidates
        .into_iter()
        .map(|mut candidate| {
            let source = sources
                .entry(candidate.file.clone())
                .or_insert_with(|| read(Path::new(&candidate.file)));
            if let Some(column) = source
                .as_deref()
                .and_then(|text| name_column_in_source(text, candidate.line, &candidate.name))
            {
                candidate.col = column;
            }
            candidate
        })
        .collect()
}

/// The 1-based column of the first whole-word occurrence of `name` on 1-based `line` of the file at
/// `path`, or `None` when the file cannot be read or `name` is not present on that line.
///
/// This corrects the column `find --json` reports: on a cold cache it takes a text-search path and
/// has been observed to report the column of the keyword before the name (`fun save` at the `fun`,
/// `val CallLogging` at the `v`), and a position request built from that column reads the keyword
/// rather than the declaration.
pub fn name_column(path: &Path, line: u32, name: &str) -> Option<u32> {
    let source = std::fs::read_to_string(path).ok()?;
    name_column_in_source(&source, line, name)
}

/// The 1-based character column of the first whole-word occurrence of `name` on 1-based `line` of
/// already-read `source`, or `None` when the line or the name is absent. The column counts
/// characters, not bytes, so a name preceded by multibyte text reports the column an editor shows
/// rather than a byte offset.
fn name_column_in_source(source: &str, line: u32, name: &str) -> Option<u32> {
    let text = source.lines().nth(line.checked_sub(1)? as usize)?;
    let offset = whole_word_offset(text, name)?;
    u32::try_from(text[..offset].chars().count() + 1).ok()
}

/// Byte offset of `name` in `text` where it is not part of a longer identifier, so `save` is not
/// found inside `saveAll`.
fn whole_word_offset(text: &str, name: &str) -> Option<usize> {
    let is_identifier = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(name)
        .map(|(offset, _)| offset)
        .find(|&offset| {
            let before = text[..offset].chars().next_back();
            let after = text[offset + name.len()..].chars().next();
            !before.is_some_and(is_identifier) && !after.is_some_and(is_identifier)
        })
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

    #[test]
    fn the_name_is_located_as_a_whole_word_not_inside_a_longer_identifier() {
        let observed = (
            whole_word_offset("    fun save(order: Order): OrderId", "save"),
            whole_word_offset(
                "    fun saveAll(all: List<Order>): Int = all.map(::save).size",
                "save",
            ),
            whole_word_offset(
                "public val CallLogging: ApplicationPlugin<CallLoggingConfig>",
                "CallLogging",
            ),
            whole_word_offset("    fun saveAll(): Int", "save"),
        );
        assert_eq!(observed, (Some(8), Some(51), Some(11), None));
    }

    #[test]
    fn the_name_column_is_none_when_the_source_cannot_be_read() {
        assert_eq!(
            name_column(Path::new("/no/such/OrderRepository.kt"), 4, "save"),
            None
        );
    }

    #[test]
    fn a_multibyte_prefix_yields_a_character_column_not_a_byte_column() {
        assert_eq!(
            (
                name_column_in_source("val 日本 = save()", 1, "save"),
                whole_word_offset("val 日本 = save()", "save"),
            ),
            (Some(10), Some(13)),
        );
    }

    #[test]
    fn each_unique_candidate_file_is_read_once_and_columns_corrected_or_kept() {
        use std::cell::RefCell;
        use std::collections::BTreeMap;

        let sources = BTreeMap::from([
            (
                "/r/A.kt",
                "package shop\n    fun save(order: Order): OrderId",
            ),
            ("/r/B.kt", "\n\n\nval 日本 = save()"),
        ]);
        let reads = RefCell::new(BTreeMap::<String, usize>::new());
        let read = |path: &Path| {
            let key = path.to_string_lossy().into_owned();
            *reads.borrow_mut().entry(key.clone()).or_default() += 1;
            sources.get(key.as_str()).map(|text| text.to_string())
        };

        let corrected = correct_columns(
            vec![
                candidate("save", "/r/A.kt", 2, 5),
                candidate("save", "/r/A.kt", 2, 5),
                candidate("save", "/r/missing.kt", 1, 7),
                candidate("save", "/r/missing.kt", 1, 7),
                candidate("save", "/r/B.kt", 4, 1),
            ],
            read,
        );

        let observed = (
            corrected
                .iter()
                .map(|found| (found.file.clone(), found.line, found.col))
                .collect::<Vec<_>>(),
            reads.into_inner(),
        );
        assert_eq!(
            observed,
            (
                vec![
                    ("/r/A.kt".to_string(), 2, 9),
                    ("/r/A.kt".to_string(), 2, 9),
                    ("/r/missing.kt".to_string(), 1, 7),
                    ("/r/missing.kt".to_string(), 1, 7),
                    ("/r/B.kt".to_string(), 4, 10),
                ],
                BTreeMap::from([
                    ("/r/A.kt".to_string(), 1),
                    ("/r/B.kt".to_string(), 1),
                    ("/r/missing.kt".to_string(), 1),
                ]),
            )
        );
    }
}
