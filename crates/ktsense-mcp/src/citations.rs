//! The `structuredContent` an answer carries: the files and lines it cites, indexed out of the
//! Markdown the command already produced.
//!
//! This is an index over an answer, not a second analysis of the code. The commands render one
//! document per question and each one is the authority on its own shape; re-deriving the same
//! facts from a `--format json` run would mean invoking the engine twice per tool call and would
//! let the text and the structure disagree. So the text is the answer and this is its citation
//! index, and it never asserts anything the text does not already say.
//!
//! What counts as a citation, stated once so it can be argued with:
//!
//! 1. `## <path>` heading whose whole text is a Kotlin source path: the file the section is about
//!    (`outline`, and one per mapped file in `map`).
//! 2. A line that is nothing but a Kotlin source path: opens a usage group, as `trace` renders it,
//!    and the `- <line>` items under it cite that file at that line.
//! 3. A `<path>:<line>` or `<path>:<line>:<column>` token anywhere on a line (`symbols` rows,
//!    `trace` definitions and related declarations, `check` errors).
//! 4. A line reading `index: <word>`: the completeness marker `trace` puts before any list, which
//!    an agent has to see because `partial` is a lower bound rather than the answer.
//!
//! A path is recognised only when it ends in `.kt` or `.kts` and carries no character that cannot
//! appear in one, which is what keeps prose and Kotlin signatures out of the index. The renderers
//! neutralize source-derived text into visible `<U+XXXX>` markers before it reaches a line, so a
//! path carrying a newline arrives as `ok.kt<U+000A>## Injected`, fails to be a path at all, and is
//! left out rather than being indexed as something it is not.

use serde::Serialize;

/// A cap on how many citations travel in one answer, so a symbol with thousands of reference sites
/// cannot make the structured half of a result larger than the answer it indexes. The text is never
/// truncated; only the index is, and it says by how much.
pub const MAX_CITATIONS: usize = 500;

/// One place an answer points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Citation {
    /// Path as the answer printed it: relative to the workspace root, `/`-separated.
    pub path: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, present only where the answer stated one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// What the caller already knows about the call it made, as opposed to what the answer says. Grouped
/// so indexing takes the call and its text, rather than four loose arguments two of which are strings.
pub struct Call<'a> {
    pub tool: &'a crate::Tool,
    pub root: &'a str,
    pub exit: Option<i32>,
    /// The label of what the server did about keeping an engine warm for this root, when it did
    /// anything at all.
    pub warmth: Option<&'static str>,
}

/// The structured half of a tool result: which tool answered, about what, and every file and line
/// its text cites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Answer {
    /// The MCP tool name that produced this.
    pub tool: String,
    /// The `ktsense` subcommand it delegated to, so a human can reproduce the call.
    pub command: String,
    /// What had to be installed for this call: `nothing`, `kmp-lsp`, or both engine and index.
    pub requires: String,
    /// The workspace root the answer is about, which is what the cited paths are relative to.
    pub root: String,
    /// The command's exit status, which is a contract to branch on: `0` an answer, `3` a name that
    /// resolved to several candidates and which one to use is yours to pick, `1` a failure on the
    /// input, `2` a malformed invocation. Absent when the child was killed by a signal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// `complete` or `partial`, when the answer carried an index marker. `partial` means the list
    /// is a lower bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// What the server did about keeping an engine warm for this root: `opened`, `left_to_daemon`,
    /// `already_decided` or `failed`. Absent when the tool's answer does not depend on the index, or
    /// when warming is switched off.
    ///
    /// This is a fact about the server's own action and never a claim about the engine's index, which
    /// the commands that carry an `index:` marker state for themselves. It is reported because
    /// `find_kotlin_symbol` carries no such marker and its answer is shaped by the index anyway:
    /// against a cold one the engine text-searches, and one declaration can arrive as several
    /// candidates that look like a genuine ambiguity. A `failed` here is the reason to distrust a
    /// duplicated-looking result and to call `ktsense_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warmth: Option<String>,
    /// Whether the tool ran and answered or failed to run, as a machine-readable category: `clean`
    /// or `findings` for a `check` that ran, `execution_failure` for one that could not reach the
    /// engine. Set only where the exit status alone cannot separate an answer from a failure, which
    /// today is `check_kotlin_syntax`: its exit 1 means both "found a syntax error" and "the engine
    /// is missing", and an agent branching on `isError` has to be able to tell them apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// How many distinct error sites a `check` answer cites: `0` when the file is clean, the count
    /// of cited positions when it is not, and absent when the check failed to run. It indexes the
    /// answer the agent already reads rather than re-invoking the engine to count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<usize>,
    /// Files the answer is about, in the order it introduced them.
    pub files: Vec<String>,
    /// Every path and line the answer cites, deduplicated, in the order they appear.
    pub citations: Vec<Citation>,
    /// How many further citations the text carries that this index does not, once the cap of 500
    /// is reached. The text is never truncated; only this index is.
    pub citations_omitted: usize,
}

/// Indexes one rendered answer. `text` is whatever the command wrote; `call` is what the caller
/// already knows about the call it made.
pub fn index_answer(call: Call<'_>, text: &str) -> Answer {
    let scan = Scan::of(text);
    Answer {
        tool: call.tool.name.to_string(),
        command: call.tool.cli_command.to_string(),
        requires: call.tool.requires.label().to_string(),
        root: call.root.to_string(),
        exit: call.exit,
        index: scan.index,
        warmth: call.warmth.map(str::to_string),
        outcome: None,
        findings: None,
        files: scan.files,
        citations: scan.citations,
        citations_omitted: scan.omitted,
    }
}

const INDEX_MARKER: &str = "index: ";
const HEADING: &str = "## ";
const BULLET: &str = "- ";
const KOTLIN_SUFFIXES: [&str; 2] = [".kt", ".kts"];

/// What one pass over the text found.
#[derive(Default)]
struct Scan {
    index: Option<String>,
    files: Vec<String>,
    citations: Vec<Citation>,
    omitted: usize,
}

impl Scan {
    fn of(text: &str) -> Self {
        let mut scan = Scan::default();
        let mut group: Option<String> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(marker) = trimmed.strip_prefix(INDEX_MARKER) {
                scan.index.get_or_insert_with(|| marker.trim().to_string());
            }
            if let Some(path) = heading_path(trimmed) {
                scan.note_file(path);
                group = Some(path.to_string());
            } else if is_source_path(trimmed) {
                scan.note_file(trimmed);
                group = Some(trimmed.to_string());
            } else if let Some(line_number) = grouped_line(trimmed) {
                if let Some(path) = &group {
                    scan.cite(path.clone(), line_number, None);
                }
            }
            for (path, line_number, column) in positions(line) {
                scan.cite(path, line_number, column);
            }
        }
        scan
    }

    fn note_file(&mut self, path: &str) {
        if !self.files.iter().any(|known| known == path) {
            self.files.push(path.to_string());
        }
    }

    fn cite(&mut self, path: String, line: u32, column: Option<u32>) {
        let citation = Citation { path, line, column };
        if self.citations.contains(&citation) {
            return;
        }
        if self.citations.len() == MAX_CITATIONS {
            self.omitted += 1;
            return;
        }
        self.note_file(&citation.path);
        self.citations.push(citation);
    }
}

/// The path a `## <path>` section heading names, when its whole text is one.
fn heading_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(HEADING)?.trim();
    is_source_path(rest).then_some(rest)
}

/// The line number a `- <digits>` usage item cites within the group above it. `- ... 3 more` and
/// `- none` are not items, and neither is a related-declaration row, which carries its own path.
fn grouped_line(line: &str) -> Option<u32> {
    let rest = line.strip_prefix(BULLET)?;
    let (line_number, after_digits) = number_at(rest, 0)?;
    let tail = rest[after_digits..].trim_start();
    (tail.is_empty() || tail.starts_with("in ")).then_some(line_number)
}

/// Every `<path>:<line>[:<column>]` the line carries.
fn positions(line: &str) -> Vec<(String, u32, Option<u32>)> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = line[at..].find(':') {
        let colon = at + offset;
        at = colon + 1;
        let Some((line_number, after_line)) = number_at(line, at) else {
            continue;
        };
        let path = &line[..colon];
        let path = &path[path.len() - path_run_length(path)..];
        if !is_source_path(path) {
            continue;
        }
        let (column, end) = match bytes.get(after_line) {
            Some(b':') => match number_at(line, after_line + 1) {
                Some((column, end)) => (Some(column), end),
                None => (None, after_line),
            },
            _ => (None, after_line),
        };
        found.push((path.to_string(), line_number, column));
        at = end;
    }
    found
}

/// The number starting at `from`, with the offset just past it, or `None` when no digit is there.
fn number_at(line: &str, from: usize) -> Option<(u32, usize)> {
    let digits: String = line[from..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    Some((digits.parse().ok()?, from + digits.len()))
}

/// How many bytes of path-shaped characters `text` ends with.
fn path_run_length(text: &str) -> usize {
    text.len() - text.trim_end_matches(is_path_character).len()
}

/// Characters a path may be made of. Deliberately narrow: a space, a bracket, a quote or a
/// `<U+XXXX>` marker ends the run, so prose and neutralized source text cannot be read as a path.
fn is_path_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | '-' | '+' | '~')
}

fn is_source_path(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(is_path_character)
        && KOTLIN_SUFFIXES
            .iter()
            .any(|suffix| text.len() > suffix.len() && text.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CHECK, OUTLINE, TRACE};

    fn call(tool: &crate::Tool) -> Call<'_> {
        Call {
            tool,
            root: "/repo",
            exit: Some(0),
            warmth: None,
        }
    }

    fn indexed(tool: &crate::Tool, text: &str) -> (Option<String>, Vec<String>, Vec<Citation>) {
        let answer = index_answer(call(tool), text);
        (answer.index, answer.files, answer.citations)
    }

    fn cite(path: &str, line: u32) -> Citation {
        Citation {
            path: path.to_string(),
            line,
            column: None,
        }
    }

    #[test]
    fn an_outline_answer_cites_the_file_its_heading_names_and_nothing_from_the_fence() {
        let text = concat!(
            "## core/src/main/kotlin/shop/order/OrderRepository.kt\n",
            "\n",
            "package shop.order\n",
            "\n",
            "```kotlin\n",
            "interface OrderRepository {\n",
            "    fun save(order: Order): OrderId\n",
            "    fun findAll(page: Int = 0): List<Order>\n",
            "}\n",
            "```\n",
        );

        assert_eq!(
            indexed(&OUTLINE, text),
            (
                None,
                vec!["core/src/main/kotlin/shop/order/OrderRepository.kt".to_string()],
                Vec::new(),
            )
        );
    }

    #[test]
    fn a_trace_answer_yields_its_marker_its_definition_and_every_grouped_usage_line() {
        let text = concat!(
            "# Trace: save\n",
            "\n",
            "index: complete\n",
            "\n",
            "## Definition\n",
            "\n",
            "core/src/main/kotlin/shop/order/OrderRepository.kt:4\n",
            "\n",
            "```kotlin\n",
            "fun save(order: Order): OrderId\n",
            "```\n",
            "\n",
            "## Implementors (1)\n",
            "- shop.db.JdbcOrderRepository.save  db/src/main/kotlin/shop/db/JdbcOrderRepository.kt:8\n",
            "\n",
            "## Callers (0)\n",
            "- none\n",
            "\n",
            "## Usages (3 sites in 1 file)\n",
            "\n",
            "app/src/main/kotlin/shop/app/Main.kt\n",
            "- 7 in shop.app.main\n",
            "- 12\n",
            "- ... 4 more\n",
        );

        assert_eq!(
            indexed(&TRACE, text),
            (
                Some("complete".to_string()),
                vec![
                    "core/src/main/kotlin/shop/order/OrderRepository.kt".to_string(),
                    "db/src/main/kotlin/shop/db/JdbcOrderRepository.kt".to_string(),
                    "app/src/main/kotlin/shop/app/Main.kt".to_string(),
                ],
                vec![
                    cite("core/src/main/kotlin/shop/order/OrderRepository.kt", 4),
                    cite("db/src/main/kotlin/shop/db/JdbcOrderRepository.kt", 8),
                    cite("app/src/main/kotlin/shop/app/Main.kt", 7),
                    cite("app/src/main/kotlin/shop/app/Main.kt", 12),
                ],
            )
        );
    }

    #[test]
    fn a_check_error_keeps_its_column_and_the_trailing_message_is_not_another_position() {
        let text = concat!(
            "## Syntax check\n",
            "\n",
            "0 OK, 1 with errors.\n",
            "\n",
            "```text\n",
            "src/Broken.kt:12:5: unexpected `fun`\n",
            "```\n",
        );

        assert_eq!(
            indexed(&CHECK, text),
            (
                None,
                vec!["src/Broken.kt".to_string()],
                vec![Citation {
                    path: "src/Broken.kt".to_string(),
                    line: 12,
                    column: Some(5),
                }],
            )
        );
    }

    #[test]
    fn prose_signatures_and_a_neutralized_path_contribute_nothing_rather_than_something_wrong() {
        let text = concat!(
            "## ok.kt<U+000A>## Injected heading\n",
            "\n",
            "Budget 4000 tokens, content bound 3925. 12 files shown, 3 omitted.\n",
            "- shop.order -> shop.db\n",
            "val timeout: Duration = 30\n",
            "fun at(position: Int = 12): String\n",
            "see notes.txt:9 and Config.kts.bak:4\n",
        );

        assert_eq!(indexed(&OUTLINE, text), (None, Vec::new(), Vec::new()));
    }

    #[test]
    fn past_the_cap_the_index_stops_and_says_how_many_it_left_out() {
        let mut text = String::from("app/Main.kt\n");
        for line in 1..=MAX_CITATIONS + 7 {
            text.push_str(&format!("- {line}\n"));
        }

        let answer = index_answer(call(&TRACE), &text);

        assert_eq!(
            (answer.citations.len(), answer.citations_omitted),
            (MAX_CITATIONS, 7)
        );
    }
}
