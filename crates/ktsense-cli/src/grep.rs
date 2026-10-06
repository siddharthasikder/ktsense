//! The `grep` command: a regex text search over the workspace's Kotlin and Java sources, each hit
//! attributed to the declaration it falls inside (KT-102, KT-122).
//!
//! `grep` needs no engine. It reuses the same file walk, path normalization and site classification
//! the KT-94 text listing uses ([`crate::collect_kotlin_files`], [`crate::collect_java_files`],
//! [`crate::normalized_path`], [`ktsense_syntax::classify_reference_sites`]); only the matcher
//! differs, a regex here against the whole-word literal there. The scan is line oriented like `rg`:
//! a hit is a line that matches, so the hit count equals `rg -c` over the same files, and the Rust
//! `regex` crate ripgrep is built on gives the same pattern semantics. The `-w`, `-i` and `-F`
//! flags map onto that crate the way `rg`'s do: `-w` bounds the pattern with ripgrep's half word
//! boundaries `\b{start-half}(?:...)\b{end-half}`, `-i` prepends `(?i)`, and `-F` runs it through
//! [`regex::escape`], so a count still equals `rg -c` with the same flags (KT-122).
//!
//! Each matching line is classified by its first match's position so a comment or string mention is
//! kept apart from code, and the grouping, capping and rendering are `ktsense-core`'s, because the
//! pure crate never reads a file or parses one. A Kotlin hit's enclosing declaration comes from the
//! file skeleton tree-sitter produces; a Java hit's comes from the pure KT-114 enclosing scan and
//! its node kind from the KT-112 Java lexer, since tree-sitter Kotlin cannot parse Java. A Java
//! file's header is labelled `(..., java)`. `--kotlin-only` restores the KT-102 Kotlin-only scope,
//! leaving an all-Kotlin workspace byte-identical with or without the flag.

use std::path::Path;

use ktsense_core::{
    build_text_search, classify_java_sites, java_enclosing_declarations,
    render_text_search_markdown, TextSearch, TextSearchHit,
};
use regex::Regex;

use crate::{
    collect_java_files, collect_kotlin_files, normalized_path, CommandError, CommandOutcome, Exit,
    Format,
};

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

/// How the pattern is matched, the three rg flags that compose onto the regex: `-w` whole word,
/// `-i` case-insensitive, `-F` fixed (literal) string. Each maps onto the `regex` crate the way
/// ripgrep maps it, so a count with any combination still equals `rg -c` with the same flags.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MatchOptions {
    pub word: bool,
    pub ignore_case: bool,
    pub fixed_strings: bool,
}

/// One `grep` invocation: the user's pattern and the knobs that shape which files it covers and how
/// it matches. A request object rather than a long parameter list, so the matcher flags that belong
/// together travel together.
pub(crate) struct GrepRequest<'a> {
    pub pattern: &'a str,
    pub path_prefix: Option<&'a str>,
    pub tests: TestFilter,
    pub limit: Option<usize>,
    pub kotlin_only: bool,
    pub matching: MatchOptions,
}

/// Builds the `regex` crate pattern for `pattern` under `matching`, mapping each flag the way
/// ripgrep does: `-F` escapes the pattern to a literal, `-w` bounds it with ripgrep's half word
/// boundaries `\b{start-half}(?:...)\b{end-half}` (what `rg -w` uses since rg 14, so a pattern with
/// a non-word edge such as `->` counts the lines `rg -w` counts, and the `(?:...)` keeps an
/// alternation bounded as a whole), and `-i` prepends the `(?i)` case-insensitive flag. Composed in
/// that order so the flags combine as rg combines them.
fn compile_pattern(pattern: &str, matching: MatchOptions) -> Result<Regex, CommandError> {
    let mut expr = if matching.fixed_strings {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    if matching.word {
        expr = format!(r"\b{{start-half}}(?:{expr})\b{{end-half}}");
    }
    if matching.ignore_case {
        expr = format!("(?i){expr}");
    }
    Regex::new(&expr).map_err(|error| CommandError::bad_pattern(pattern, &error))
}

/// Runs a `grep`: compiles the pattern under its flags, scans the matching files, and renders the
/// grouped answer. A text search that matches nothing is an answer, not a failure, so it still exits
/// successfully. The rendered pattern is the user's own, not the flag-expanded regex.
pub(crate) fn run(
    root: &Path,
    request: GrepRequest,
    format: Format,
) -> Result<CommandOutcome, CommandError> {
    let regex = compile_pattern(request.pattern, request.matching)?;
    let scan = scan(root, &regex, &request)?;
    let search = build_text_search(request.pattern, &scan.hits, &scan.skeletons, request.limit);
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

/// Scans the workspace's Kotlin files, and (unless `--kotlin-only`) its Java files, for the pattern,
/// honouring `--root`, the `--path` prefix and the test-source filter. A file that cannot be read is
/// skipped rather than failing the scan, the rule the map and deps walks follow. A Kotlin file that
/// carries a hit is parsed so the core can attribute each hit to a declaration; a Java file's hits
/// are attributed here by the pure enclosing scan, since tree-sitter cannot parse Java.
fn scan(root: &Path, regex: &Regex, request: &GrepRequest) -> Result<Scan, CommandError> {
    let mut hits = Vec::new();
    let mut skeletons = Vec::new();
    for file in collect_kotlin_files(root)? {
        let Some((display, source, matches)) =
            file_matches(root, &file, regex, request.path_prefix, request.tests)
        else {
            continue;
        };
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
                enclosing: None,
                java: false,
            });
        }
        if let Ok(skeleton) = ktsense_syntax::extract(display, &source) {
            skeletons.push(skeleton);
        }
    }
    if !request.kotlin_only {
        for file in collect_java_files(root)? {
            let Some((display, source, matches)) =
                file_matches(root, &file, regex, request.path_prefix, request.tests)
            else {
                continue;
            };
            let coordinates: Vec<(u32, u32)> = matches
                .iter()
                .map(|matched| (matched.line, matched.column))
                .collect();
            let kinds = classify_java_sites(&source, &coordinates);
            let lines: Vec<u32> = matches.iter().map(|matched| matched.line).collect();
            let enclosings = java_enclosing_declarations(&source, &lines);
            for ((matched, kind), enclosing) in matches.into_iter().zip(kinds).zip(enclosings) {
                hits.push(TextSearchHit {
                    path: display.clone(),
                    line: matched.line,
                    source_line: matched.source_line,
                    kind,
                    enclosing,
                    java: true,
                });
            }
        }
    }
    Ok(Scan { hits, skeletons })
}

/// The workspace-relative path, source text and matching lines of one file, or `None` when the file
/// is outside the `--path` prefix, the wrong source set, unreadable, or carries no hit. Shared by
/// the Kotlin and Java scans so both filter one tree the same way.
fn file_matches(
    root: &Path,
    file: &Path,
    regex: &Regex,
    path_prefix: Option<&str>,
    tests: TestFilter,
) -> Option<(String, String, Vec<MatchedLine>)> {
    let display = normalized_path(root, file);
    if path_prefix.is_some_and(|prefix| !display.starts_with(prefix)) {
        return None;
    }
    if !tests.includes(ktsense_core::is_test_source(&display)) {
        return None;
    }
    let source = std::fs::read_to_string(file).ok()?;
    let matches = matching_lines(&source, regex);
    (!matches.is_empty()).then_some((display, source, matches))
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

    /// Each rg flag maps onto the regex the way ripgrep maps it, and they compose: `-F` escapes the
    /// pattern to a literal, `-w` bounds the whole pattern with ripgrep's half word boundaries, `-i`
    /// matches either case, and all three together find a bounded literal case-insensitively while a
    /// plain pattern is matched verbatim. Asserted as one table of the lines each matches in one
    /// source, so the matcher semantics are pinned in one place.
    #[test]
    fn flags_shape_the_regex_the_way_ripgrep_does_and_compose() {
        let source = "val save = x\nval saver = y\nval a_b = call(a.b)\nval SAVE = z\n";
        let lines_matching = |pattern: &str, matching: MatchOptions| -> Vec<u32> {
            let regex = compile_pattern(pattern, matching).expect("valid pattern");
            matching_lines(source, &regex)
                .into_iter()
                .map(|matched| matched.line)
                .collect::<Vec<_>>()
        };
        let word = MatchOptions {
            word: true,
            ..MatchOptions::default()
        };
        let fixed = MatchOptions {
            fixed_strings: true,
            ..MatchOptions::default()
        };
        let word_fixed_ignore = MatchOptions {
            word: true,
            ignore_case: true,
            fixed_strings: true,
        };

        let observed = (
            lines_matching("save", MatchOptions::default()),
            lines_matching("save", word),
            lines_matching("a.b", fixed),
            lines_matching("save", word_fixed_ignore),
        );

        assert_eq!(observed, (vec![1, 2], vec![1], vec![3], vec![1, 4]));
    }

    /// `-w` uses ripgrep's half word boundaries (`\b{start-half}(?:...)\b{end-half}`, as rg 15.2.0
    /// does), not a full `\b(?:...)\b`, so a pattern whose own edge is a non-word character counts
    /// the lines rg counts: `-w -- '->'` matches a spaced arrow but not one glued between
    /// identifiers, and `-w 'foo-'` matches a trailing hyphen followed by a non-word character. A
    /// full `\b` would invert both, matching `a->b` and missing `foo- `. Pinned against rg 15.2.0
    /// (verified by `rg -n -w`) because the word-identifier cases above cannot reach a non-word edge.
    #[test]
    fn word_flag_uses_half_word_boundaries_so_non_word_edges_match_rg() {
        let source = "val f: (String) -> Boolean\nval g = a->b\ntrailing foo- here\nglued afoo-b\n";
        let word = MatchOptions {
            word: true,
            ..MatchOptions::default()
        };
        let lines = |pattern: &str| -> Vec<u32> {
            let regex = compile_pattern(pattern, word).expect("valid pattern");
            matching_lines(source, &regex)
                .into_iter()
                .map(|matched| matched.line)
                .collect::<Vec<_>>()
        };

        let observed = (lines("->"), lines("foo-"));

        assert_eq!(observed, (vec![1], vec![3]));
    }
}
