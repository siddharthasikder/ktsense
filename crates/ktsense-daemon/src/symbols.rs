//! Exact-name declaration lookup answered from the daemon's warm session.
//!
//! `trace` needs a name resolved to declarations before it can ask the engine anything positional.
//! The command-mode `find` subprocess that answers this on the in-process path rebuilds the engine's
//! whole index per invocation, which costs more than the rest of a warm trace put together, so a
//! daemon that already holds an indexed session should not be paying it. This module is that lookup:
//! one `workspace/symbol` request on the warm session, narrowed to the declarations a `find --root`
//! would have reported.
//!
//! Two measured properties of `kmp-lsp` 0.26.0 shape the narrowing, and neither is visible from the
//! LSP protocol:
//!
//! 1. `workspace/symbol` matching is a case-insensitive **substring** test, so a query answers with
//!    every symbol whose name merely contains it (`save` also returns `saveAll`). Only an exact name
//!    match is a candidate.
//! 2. The engine's index reaches past the session's `rootUri` to the enclosing repository, so a
//!    session rooted at one module answers with symbols from its siblings too. Measured on
//!    `fixtures/multi-module`, whose session reports `tiny-app` symbols. Every candidate is therefore
//!    filtered to the root, which is the same widening `--root` prevents in command mode.
//!
//! A third property bounds what the request can prove: the engine truncates the response at
//! [`WORKSPACE_SYMBOL_CAP`] entries while it scans, in index order, so a name whose substring matches
//! exceed the cap loses exact matches that the cap happened to cut, and loses different ones on each
//! run. That cannot be filtered after the fact, so a response at the cap is reported as
//! [`Inconclusive::Truncated`] and the caller must ask command-mode `find` instead. Likewise a name
//! the warm index holds no declaration for is [`Inconclusive::Unindexed`] rather than "no such
//! symbol": `find` answers such a name from its own text-search fallback, and preserving what
//! ktsense reports today means asking it.

use std::path::{Path, PathBuf};

use ktsense_lsp::{uri_to_path, LspClient, SymbolCandidate};
use serde::Deserialize;
use serde_json::{json, Value};

/// The LSP request that reads the engine's indexed symbol table.
const WORKSPACE_SYMBOL: &str = "workspace/symbol";

/// How many entries `kmp-lsp` 0.26.0 returns before it stops scanning its index
/// (`WORKSPACE_SYMBOL_CAP` in upstream's `features/workspace_symbols.rs`). A response of this length
/// is a truncated view of the matches, not all of them.
const WORKSPACE_SYMBOL_CAP: usize = 512;

/// What the warm session could say about a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmResolution {
    /// Every declaration of the name the warm index holds under the root, in the order the engine
    /// reported them.
    Declarations(Vec<SymbolCandidate>),
    /// The warm session cannot answer the name completely; the caller must resolve it the slow way.
    Inconclusive(Inconclusive),
}

/// Why a warm lookup cannot stand on its own. Each variant names something the caller can report,
/// so a fallback is never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inconclusive {
    /// The response reached the engine's result cap, so completeness cannot be shown.
    Truncated { returned: usize },
    /// The warm index holds no declaration of the name under the root.
    Unindexed,
    /// The request itself failed, leaving the warm session's view unknown.
    Failed(String),
}

/// Every declaration of `name` the warm session holds under `root`.
///
/// The returned candidates carry the same fields command-mode `find` produces for the same
/// declaration: the engine's absolute path, its 1-based line, and the 1-based **character** column of
/// the name located on that line. The column is relocated rather than taken from the response
/// because an LSP position counts UTF-16 code units, which is not the character column ktsense
/// presents and requests references at; the engine's own column survives only when the name cannot be
/// found on its line, which keeps an unverifiable location honest instead of inventing one.
pub async fn resolve_from_warm_index(
    client: &LspClient,
    root: &Path,
    name: &str,
) -> WarmResolution {
    let response = match client
        .request(WORKSPACE_SYMBOL, json!({ "query": name }))
        .await
    {
        Ok(value) => value,
        Err(error) => return WarmResolution::Inconclusive(Inconclusive::Failed(error.to_string())),
    };
    match parse_reported(response) {
        Ok(reported) => classify(name, root, &reported),
        Err(error) => WarmResolution::Inconclusive(Inconclusive::Failed(error)),
    }
}

/// Narrows a whole response to the declarations that answer the name, or says why it cannot.
fn classify(name: &str, root: &Path, reported: &[ReportedSymbol]) -> WarmResolution {
    if reported.len() >= WORKSPACE_SYMBOL_CAP {
        return WarmResolution::Inconclusive(Inconclusive::Truncated {
            returned: reported.len(),
        });
    }
    let declarations = declarations_in_root(name, root, reported);
    if declarations.is_empty() {
        return WarmResolution::Inconclusive(Inconclusive::Unindexed);
    }
    WarmResolution::Declarations(declarations)
}

fn declarations_in_root(
    name: &str,
    root: &Path,
    reported: &[ReportedSymbol],
) -> Vec<SymbolCandidate> {
    let root = canonical(root);
    reported
        .iter()
        .filter(|entry| entry.name == name)
        .map(|entry| (entry, uri_to_path(&entry.location.uri)))
        .filter(|(_, path)| is_under(&root, path))
        .map(|(entry, path)| candidate_at(name, entry, &path))
        .collect()
}

fn candidate_at(name: &str, entry: &ReportedSymbol, path: &Path) -> SymbolCandidate {
    let line = entry.location.range.start.line + 1;
    let reported_column = entry.location.range.start.character + 1;
    SymbolCandidate {
        name: name.to_string(),
        file: path.to_string_lossy().into_owned(),
        line,
        col: ktsense_lsp::name_column(path, line, name).unwrap_or(reported_column),
    }
}

/// Whether `path` names a file inside `root`. Compared component-wise, so a sibling root sharing a
/// textual prefix is not mistaken for a child, and canonicalized on a miss so a root reached through
/// a symlink still matches.
fn is_under(root: &Path, path: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved.starts_with(root),
        Err(_unreadable) => false,
    }
}

fn canonical(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn parse_reported(response: Value) -> Result<Vec<ReportedSymbol>, String> {
    // A name the engine knows nothing about answers `null` rather than an empty array (KT-49), so a
    // missing list is an empty result, not a malformed response.
    serde_json::from_value::<Option<Vec<ReportedSymbol>>>(response)
        .map(Option::unwrap_or_default)
        .map_err(|error| format!("unexpected {WORKSPACE_SYMBOL} response: {error}"))
}

/// One entry of a `workspace/symbol` response, narrowed to the fields this lookup reads.
#[derive(Debug, Clone, Deserialize)]
struct ReportedSymbol {
    name: String,
    location: ReportedLocation,
}

#[derive(Debug, Clone, Deserialize)]
struct ReportedLocation {
    uri: String,
    range: ReportedRange,
}

#[derive(Debug, Clone, Deserialize)]
struct ReportedRange {
    start: ReportedPosition,
}

/// A zero-based LSP position. `character` counts UTF-16 code units, which is why it is only ever a
/// fallback for a column ktsense presents.
#[derive(Debug, Clone, Deserialize)]
struct ReportedPosition {
    line: u32,
    character: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _root: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temp root");
            let path = std::fs::canonicalize(root.path()).expect("canonical temp root");
            Self {
                _root: root,
                root: path,
            }
        }

        fn write(&self, relative: &str, source: &str) -> PathBuf {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().expect("file has a parent"))
                .expect("create dirs");
            std::fs::write(&path, source).expect("write source");
            path
        }
    }

    /// One `workspace/symbol` entry, built the way the engine reports one: a zero-based position
    /// whose `character` points at the name.
    fn reported(name: &str, path: &Path, line: u32, character: u32) -> ReportedSymbol {
        ReportedSymbol {
            name: name.to_string(),
            location: ReportedLocation {
                uri: format!("file://{}", path.display()),
                range: ReportedRange {
                    start: ReportedPosition {
                        line: line - 1,
                        character,
                    },
                },
            },
        }
    }

    fn observed(resolution: &WarmResolution, root: &Path) -> Vec<(String, u32, u32)> {
        match resolution {
            WarmResolution::Declarations(candidates) => candidates
                .iter()
                .map(|found| {
                    let relative = Path::new(&found.file)
                        .strip_prefix(root)
                        .unwrap_or(Path::new(&found.file))
                        .to_string_lossy()
                        .into_owned();
                    (relative, found.line, found.col)
                })
                .collect(),
            WarmResolution::Inconclusive(_) => Vec::new(),
        }
    }

    #[test]
    fn only_exact_in_root_declarations_are_kept_and_the_engine_order_survives() {
        let fixture = Fixture::new();
        let repository = fixture.write(
            "core/OrderRepository.kt",
            "package shop.order\n\ninterface OrderRepository {\n    fun save(order: Order): OrderId\n}\n",
        );
        let jdbc = fixture.write(
            "db/JdbcOrderRepository.kt",
            "package shop.db\n\nclass JdbcOrderRepository {\n    override fun save(order: Order): OrderId = TODO()\n}\n",
        );
        let sibling_root = tempfile::tempdir().expect("sibling root");
        let outside = sibling_root.path().join("Other.kt");
        std::fs::write(&outside, "package other\n\nfun save(): Unit = Unit\n").expect("write");

        let resolution = classify(
            "save",
            &fixture.root,
            &[
                // The engine answers substring matches and neighbouring roots as well.
                reported("saveAll", &repository, 4, 8),
                reported("save", &jdbc, 4, 17),
                reported("save", &outside, 3, 4),
                reported("save", &repository, 4, 8),
            ],
        );

        assert_eq!(
            observed(&resolution, &fixture.root),
            vec![
                ("db/JdbcOrderRepository.kt".to_string(), 4, 18),
                ("core/OrderRepository.kt".to_string(), 4, 9),
            ],
        );
    }

    #[test]
    fn a_capped_response_and_a_name_the_index_lacks_are_both_inconclusive() {
        let fixture = Fixture::new();
        let file = fixture.write(
            "core/Order.kt",
            "package shop.order\n\ndata class Order(val id: OrderId)\n",
        );
        let at_cap: Vec<ReportedSymbol> = (0..WORKSPACE_SYMBOL_CAP)
            .map(|_| reported("Order", &file, 3, 11))
            .collect();

        let truncated = classify("Order", &fixture.root, &at_cap);
        let nothing_matched =
            classify("Order", &fixture.root, &[reported("OrderId", &file, 3, 25)]);
        let empty_response = classify("Order", &fixture.root, &[]);

        assert_eq!(
            (truncated, nothing_matched, empty_response),
            (
                WarmResolution::Inconclusive(Inconclusive::Truncated {
                    returned: WORKSPACE_SYMBOL_CAP
                }),
                WarmResolution::Inconclusive(Inconclusive::Unindexed),
                WarmResolution::Inconclusive(Inconclusive::Unindexed),
            )
        );
    }

    /// The column must be the character column an editor shows: not the UTF-16 offset an LSP
    /// position carries, not a byte offset, and not the keyword column the engine reports when it
    /// answers from its text-search path (KT-24). A file ktsense cannot read must keep the engine's
    /// column rather than acquire an invented one.
    #[test]
    fn the_name_is_relocated_to_its_character_column_unless_the_file_cannot_be_read() {
        let fixture = Fixture::new();
        // `save` sits at character 9, byte 12, and UTF-16 unit 10 of line 3: the emoji is one
        // character, four bytes and two UTF-16 code units, so the three columns disagree.
        let astral = fixture.write("core/Totals.kt", "package shop\n\nval \u{1f600} = save()\n");
        let keyword = fixture.write("core/Ledger.kt", "package shop\n\nval balance = 0\n");
        // Under the root but never written: the name cannot be located, so nothing may be invented.
        let missing = fixture.root.join("core/Absent.kt");

        let relocated = classify("save", &fixture.root, &[reported("save", &astral, 3, 10)]);
        let off_the_keyword = classify(
            "balance",
            &fixture.root,
            &[reported("balance", &keyword, 3, 0)],
        );
        let unreadable = classify("save", &fixture.root, &[reported("save", &missing, 7, 12)]);

        assert_eq!(
            (
                observed(&relocated, &fixture.root),
                observed(&off_the_keyword, &fixture.root),
                observed(&unreadable, &fixture.root),
            ),
            (
                vec![("core/Totals.kt".to_string(), 3, 9)],
                vec![("core/Ledger.kt".to_string(), 3, 5)],
                vec![("core/Absent.kt".to_string(), 7, 13)],
            )
        );
    }

    #[test]
    fn a_null_response_is_an_empty_result_and_a_malformed_one_is_a_failure() {
        let empty = parse_reported(Value::Null);
        let malformed = parse_reported(json!({ "symbols": [] }));

        assert_eq!(
            (empty.map(|found| found.len()), malformed.is_err()),
            (Ok(0), true)
        );
    }
}
