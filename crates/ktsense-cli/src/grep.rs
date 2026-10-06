//! The `grep` command: a regex text search over the workspace's Kotlin sources, each hit attributed
//! to the declaration it falls inside (KT-102).
//!
//! `grep` needs no engine. It reuses the same file walk, path normalization and site classification
//! the KT-94 text listing uses ([`crate::collect_kotlin_files`], [`crate::normalized_path`],
//! [`ktsense_syntax::classify_reference_sites`]); only the matcher differs, a regex here against the
//! whole-word literal there. The scan is line oriented like `rg`: a hit is a line that matches, so
//! the hit count equals `rg -c` over the same files, and the Rust `regex` crate ripgrep is built on
//! gives the same pattern semantics. Each matching line is classified by its first match's position
//! so a comment or string mention is kept apart from code, and the grouping, capping and rendering
//! are `ktsense-core`'s, because the pure crate never reads a file or parses one.

use std::path::Path;

use ktsense_core::{build_text_search, render_text_search_markdown, TextSearch, TextSearchHit};
use regex::Regex;

use crate::{collect_kotlin_files, normalized_path, CommandError, CommandOutcome, Exit, Format};

/// Which source sets a search covers. Production and test are the KT-91 split; the default is both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestFilter {
    All,
    OnlyTests,
    OnlyProduction,
}

impl TestFilter {
    /// Reads the two mutually exclusive CLI flags into one filter. clap already refuses both at once,
    /// so a true-true pair cannot reach here.
    pub(crate) fn from_flags(tests: bool, no_tests: bool) -> Self {
        match (tests, no_tests) {
            (true, _) => TestFilter::OnlyTests,
            (_, true) => TestFilter::OnlyProduction,
            _ => TestFilter::All,
        }
    }

    fn includes(self, is_test: bool) -> bool {
        match self {
            TestFilter::All => true,
            TestFilter::OnlyTests => is_test,
            TestFilter::OnlyProduction => !is_test,
        }
    }
}

/// Runs a `grep`: compiles the pattern, scans the matching files, and renders the grouped answer.
/// A text search that matches nothing is an answer, not a failure, so it still exits successfully.
pub(crate) fn run(
    root: &Path,
    pattern: &str,
    path_prefix: Option<&str>,
    tests: TestFilter,
    limit: Option<usize>,
    format: Format,
) -> Result<CommandOutcome, CommandError> {
    let regex = Regex::new(pattern).map_err(|error| CommandError::bad_pattern(pattern, &error))?;
    let scan = scan(root, &regex, path_prefix, tests)?;
    let search = build_text_search(pattern, &scan.hits, &scan.skeletons, limit);
    Ok(CommandOutcome {
        text: present(&search, format)?,
        exit: Exit::Success,
        stderr: None,
    })
}

/// Every classified hit the regex found, with the file skeletons the core needs to name each hit's
/// enclosing declaration.
struct Scan {
    hits: Vec<TextSearchHit>,
    skeletons: Vec<ktsense_core::FileSkeleton>,
}

/// Scans the workspace's Kotlin files for the pattern, honouring `--root`, the `--path` prefix and
/// the test-source filter. A file that cannot be read is skipped rather than failing the scan, the
/// rule the map and deps walks follow; a file that carries a hit is also parsed, so the core can
/// attribute each hit to a declaration.
fn scan(
    root: &Path,
    regex: &Regex,
    path_prefix: Option<&str>,
    tests: TestFilter,
) -> Result<Scan, CommandError> {
    let mut hits = Vec::new();
    let mut skeletons = Vec::new();
    for file in collect_kotlin_files(root)? {
        let display = normalized_path(root, &file);
        if path_prefix.is_some_and(|prefix| !display.starts_with(prefix)) {
            continue;
        }
        if !tests.includes(ktsense_core::is_test_source(&display)) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        let matches = matching_lines(&source, regex);
        if matches.is_empty() {
            continue;
        }
        let coordinates: Vec<(u32, u32)> = matches
            .iter()
            .map(|matched| (matched.line, matched.column))
            .collect();
        let kinds = ktsense_syntax::classify_reference_sites(&source, &coordinates);
        for (matched, kind) in matches.into_iter().zip(kinds) {
            hits.push(TextSearchHit {
                path: display.clone(),
                line: matched.line,
                source_line: matched.source_line,
                kind,
            });
        }
        if let Ok(skeleton) = ktsense_syntax::extract(display, &source) {
            skeletons.push(skeleton);
        }
    }
    Ok(Scan { hits, skeletons })
}

/// One matching line: the 1-based line, the 1-based UTF-16 column of the first match on it (the
/// coordinate [`ktsense_syntax::classify_reference_sites`] expects), and the whole source line.
struct MatchedLine {
    line: u32,
    column: u32,
    source_line: String,
}

/// Every line of `source` the pattern matches, one hit per line as `rg -c` counts them. The column
/// is the first match's start as a UTF-16 offset, which is where the line is classified: a line
/// with several matches is still one hit, classified by its first.
fn matching_lines(source: &str, regex: &Regex) -> Vec<MatchedLine> {
    source
        .lines()
        .enumerate()
        .filter_map(|(row, line)| {
            regex.find(line).map(|matched| MatchedLine {
                line: row as u32 + 1,
                column: line[..matched.start()].encode_utf16().count() as u32 + 1,
                source_line: line.to_string(),
            })
        })
        .collect()
}

fn present(search: &TextSearch, format: Format) -> Result<String, CommandError> {
    match format {
        Format::Md => Ok(render_text_search_markdown(search)),
        Format::Json => crate::as_json(search),
        Format::Dot => Err(CommandError::unsupported_format("grep")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_resolve_to_the_three_filters_and_each_admits_the_right_sources() {
        let observed = (
            TestFilter::from_flags(false, false).includes(true),
            TestFilter::from_flags(false, false).includes(false),
            TestFilter::from_flags(true, false).includes(true),
            TestFilter::from_flags(true, false).includes(false),
            TestFilter::from_flags(false, true).includes(true),
            TestFilter::from_flags(false, true).includes(false),
        );

        assert_eq!(observed, (true, true, true, false, false, true));
    }

    /// One hit per matching line as `rg -c` counts them, with the first match's UTF-16 column for
    /// classification: a line with two matches is one hit, an embedded match on a later line still
    /// matches (a regex is not whole-word), and a multi-byte character before the match shifts the
    /// column. Asserted as one table of (line, column, source).
    #[test]
    fn each_matching_line_is_one_hit_located_at_its_first_match_in_utf16_columns() {
        let source = concat!(
            "val a = save(save(x))\n",
            "// é save here\n",
            "val saved = 1\n",
            "no match on this line\n",
        );
        let regex = Regex::new("save").expect("valid");

        let observed: Vec<(u32, u32, String)> = matching_lines(source, &regex)
            .into_iter()
            .map(|matched| (matched.line, matched.column, matched.source_line))
            .collect();

        assert_eq!(
            observed,
            vec![
                (1, 9, "val a = save(save(x))".to_string()),
                (2, 6, "// é save here".to_string()),
                (3, 5, "val saved = 1".to_string()),
            ]
        );
    }
}
