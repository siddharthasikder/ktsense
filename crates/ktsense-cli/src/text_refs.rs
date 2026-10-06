//! The workspace text scan behind a `trace` or `context` whose name nothing declares (KT-94).
//!
//! When resolution reports no declaration named X, this scans the same Kotlin files the rest of the
//! product walks and lists where X is written anyway: a whole-word match, respecting `--root`,
//! skipping the same build and vendor directories [`crate::collect_kotlin_files`] skips. Each match
//! is classified by the syntax node it falls in (KT-83) so comment and string mentions are kept but
//! counted apart from code. The grouping, capping and rendering are `ktsense-core`'s; this module
//! only reads the files and hands over classified [`Location`] values, because the pure crate never
//! touches a filesystem or a parser.
//!
//! The scan parses only the files that carry a raw match, not the whole tree, so confirming absence
//! stays cheap on a large repository where the answer is almost always "a handful of files".

use std::path::Path;

use ktsense_core::{
    build_annotated, build_text_references, build_text_references_attributed, classify_java_sites,
    fully_qualified_enclosing, render_annotated_markdown, render_text_references_markdown,
    AnnotatedDeclaration, AnnotatedDeclarations, FileSkeleton, ForeignReference, Location,
    TextReferences,
};

use crate::{collect_kotlin_files, normalized_path, CommandError, CommandOutcome, Exit, Format};

/// The exit a name nothing declares ends with, unchanged from before this listing existed: the
/// answer on stdout, the status still a failure the caller can branch on.
const NOT_FOUND_EXIT: Exit = Exit::Failure;

/// Builds the answer for a name the workspace does not declare: the KT-87 not-found wording, then
/// the text-reference evidence, on stdout with a failing exit and no stderr, so a routed daemon and
/// the in-process path render the same bytes and the MCP layer carries the same answer.
pub(crate) fn not_found_outcome(
    root: &Path,
    symbol: &str,
    limit: Option<usize>,
    format: Format,
    command: &str,
) -> Result<CommandOutcome, CommandError> {
    let references = build_text_references(symbol, &collect_text_sites(root, symbol)?, limit);
    let uses = collect_annotation_uses(root, symbol)?;
    let annotated = (!uses.is_empty()).then(|| build_annotated(symbol, &uses));
    let text = present(
        symbol,
        annotated.as_ref(),
        references,
        format,
        command,
        root,
    )?;
    Ok(CommandOutcome {
        text,
        exit: NOT_FOUND_EXIT,
        stderr: None,
    })
}

/// The not-found answer as `message` in JSON, absent otherwise, so a JSON consumer reads the same
/// scope wording a Markdown reader does. `annotated` carries the declarations the name is written on
/// as an annotation when there are any, skipped otherwise so an undeclared name that annotates
/// nothing serializes exactly as before (KT-109).
#[derive(serde::Serialize)]
struct NotFoundAnswer<'a> {
    found: bool,
    symbol: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotated: Option<&'a AnnotatedDeclarations>,
    text_references: TextReferences,
}

fn present(
    symbol: &str,
    annotated: Option<&AnnotatedDeclarations>,
    references: TextReferences,
    format: Format,
    command: &str,
    root: &Path,
) -> Result<String, CommandError> {
    match format {
        Format::Md => {
            let mut out = crate::no_symbol_message_scoped(symbol, root);
            out.push_str("\n\n");
            if let Some(annotated) = annotated {
                out.push_str(&render_annotated_markdown(annotated));
                out.push('\n');
            }
            out.push_str(&render_text_references_markdown(&references));
            Ok(out)
        }
        Format::Json => crate::as_json(&NotFoundAnswer {
            found: false,
            symbol,
            message: crate::no_symbol_message_scoped(symbol, root),
            annotated,
            text_references: references,
        }),
        Format::Dot => Err(CommandError::unsupported_format(command)),
    }
}

/// Every whole-word text match of `symbol` in the workspace's Kotlin sources, classified by the
/// syntax node it falls in. Generated sources under `build/generated` are scanned too, so a name
/// that only a generated file uses still surfaces (KT-104). A file that cannot be read is skipped
/// rather than failing the scan, the same rule the map and deps walks follow.
fn collect_text_sites(root: &Path, symbol: &str) -> Result<Vec<Location>, CommandError> {
    let mut sites = Vec::new();
    let mut scanned = std::collections::HashSet::new();
    for path in collect_kotlin_files(root)? {
        scan_file_for_sites(root, &path, symbol, &mut sites);
        scanned.insert(path);
    }
    for path in crate::collect_generated_kotlin_files(root) {
        if scanned.insert(path.clone()) {
            scan_file_for_sites(root, &path, symbol, &mut sites);
        }
    }
    Ok(sites)
}

/// Appends every classified whole-word site of `symbol` in one file. A file that cannot be read
/// contributes nothing.
fn scan_file_for_sites(root: &Path, path: &Path, symbol: &str, sites: &mut Vec<Location>) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    let matches = word_bounded_matches(&source, symbol);
    if matches.is_empty() {
        return;
    }
    let coordinates: Vec<(u32, u32)> = matches.clone();
    let kinds = ktsense_syntax::classify_reference_sites(&source, &coordinates);
    let display = normalized_path(root, path);
    for ((line, _), kind) in matches.into_iter().zip(kinds) {
        sites.push(Location::new(display.clone(), line).with_kind(kind));
    }
}

/// Every declaration in the workspace's Kotlin sources written with `@<annotation>`, found by the
/// same traversal [`collect_text_sites`] uses, including generated sources (KT-104). Only files that
/// mention the name as a whole word are parsed, so confirming the annotation stays cheap on a large
/// repository where a handful of files carry it. The attachment itself is `ktsense-syntax`'s; this
/// reads the files and defers to it, keeping the pure crate free of a parser and the filesystem.
pub(crate) fn collect_annotation_uses(
    root: &Path,
    annotation: &str,
) -> Result<Vec<AnnotatedDeclaration>, CommandError> {
    let mut declarations = Vec::new();
    let mut scanned = std::collections::HashSet::new();
    for path in collect_kotlin_files(root)? {
        scan_file_for_annotations(root, &path, annotation, &mut declarations);
        scanned.insert(path);
    }
    for path in crate::collect_generated_kotlin_files(root) {
        if scanned.insert(path.clone()) {
            scan_file_for_annotations(root, &path, annotation, &mut declarations);
        }
    }
    Ok(declarations)
}

/// Appends every declaration in one file written with `@<annotation>`. A file that cannot be read
/// contributes nothing, and one that does not mention the name as a whole word is never parsed.
fn scan_file_for_annotations(
    root: &Path,
    path: &Path,
    annotation: &str,
    declarations: &mut Vec<AnnotatedDeclaration>,
) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    if word_bounded_matches(&source, annotation).is_empty() {
        return;
    }
    let display = normalized_path(root, path);
    declarations.extend(ktsense_syntax::annotated_declarations(
        &display, &source, annotation,
    ));
}

/// Every whole-word occurrence of `symbol` in `source`, as a 1-based line and 1-based UTF-16 column,
/// the coordinate `ktsense_syntax::classify_reference_sites` expects. A match is whole-word when the
/// byte on either side is not part of an identifier, so `putMetric` matches `metrics.putMetric(` and
/// `@putMetric` but not `putMetricValue`.
fn word_bounded_matches(source: &str, symbol: &str) -> Vec<(u32, u32)> {
    if symbol.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (row, line) in source.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut search_from = 0;
        while let Some(offset) = line[search_from..].find(symbol) {
            let start = search_from + offset;
            let end = start + symbol.len();
            let bounded_before = start == 0 || !is_identifier_byte(bytes[start - 1]);
            let bounded_after = end >= line.len() || !is_identifier_byte(bytes[end]);
            if bounded_before && bounded_after {
                let column = line[..start].encode_utf16().count() as u32 + 1;
                found.push((row as u32 + 1, column));
            }
            search_from = start + 1;
        }
    }
    found
}

/// Whether a byte can appear in the middle of a Kotlin identifier, so a match flanked by one is not
/// whole-word. ASCII letters, digits and the underscore; a non-ASCII byte is treated as a boundary,
/// which is deliberate and matches the whole-word intent for the identifiers this scan looks for.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Java text references for `symbol` under `root`, grouped and capped, or `None` when no `.java`
/// file mentions the name (KT-112). A root with no Java yields `None`, so `trace` and `context` stay
/// byte-identical on an all-Kotlin workspace.
pub(crate) fn java_text_references(
    root: &Path,
    symbol: &str,
    limit: Option<usize>,
) -> Result<Option<TextReferences>, CommandError> {
    let sites = collect_java_sites(root, symbol)?;
    Ok((!sites.is_empty()).then(|| build_text_references(symbol, &sites, limit)))
}

/// Kotlin text references for `symbol`, each attributed to its enclosing declaration, or `None` when
/// no Kotlin source mentions the name. Used when the resolved definition is in a `.java` file, where
/// the engine resolves no Kotlin references to it (KT-112).
pub(crate) fn kotlin_text_references(
    root: &Path,
    symbol: &str,
    limit: Option<usize>,
) -> Result<Option<TextReferences>, CommandError> {
    let sites = collect_text_sites(root, symbol)?;
    if sites.is_empty() {
        return Ok(None);
    }
    let skeletons = skeletons_for_sites(root, &sites);
    Ok(Some(build_text_references_attributed(
        symbol, &sites, &skeletons, limit,
    )))
}

/// The Java text references as flat, budget-ready lines for a `context` bundle (KT-112): sorted by
/// path then line, carrying no enclosing declaration because the Java scan does not attribute.
pub(crate) fn java_foreign_references(
    root: &Path,
    symbol: &str,
) -> Result<Vec<ForeignReference>, CommandError> {
    let mut references: Vec<ForeignReference> = collect_java_sites(root, symbol)?
        .into_iter()
        .map(|site| ForeignReference {
            path: site.path,
            line: site.line,
            enclosing: None,
        })
        .collect();
    references.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Ok(references)
}

/// The Kotlin text references as flat, budget-ready lines for a `context` bundle whose definition is
/// in a `.java` file (KT-112): each attributed to its enclosing declaration through the KT-102
/// attribution, sorted by path then line.
pub(crate) fn kotlin_foreign_references(
    root: &Path,
    symbol: &str,
) -> Result<Vec<ForeignReference>, CommandError> {
    let sites = collect_text_sites(root, symbol)?;
    let skeletons = skeletons_for_sites(root, &sites);
    let by_path: std::collections::HashMap<&str, &FileSkeleton> = skeletons
        .iter()
        .map(|skeleton| (skeleton.path.as_str(), skeleton))
        .collect();
    let mut references: Vec<ForeignReference> = sites
        .into_iter()
        .map(|site| {
            let enclosing = by_path
                .get(site.path.as_str())
                .and_then(|skeleton| fully_qualified_enclosing(skeleton, site.line))
                .map(|enclosing| enclosing.fqn);
            ForeignReference {
                path: site.path,
                line: site.line,
                enclosing,
            }
        })
        .collect();
    references.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Ok(references)
}

/// Every whole-word text match of `symbol` in the workspace's `.java` sources, classified by the
/// pure Java lexer so a mention in a comment or string is counted apart from a use in code (KT-112).
/// A file that cannot be read is skipped rather than failing the scan.
fn collect_java_sites(root: &Path, symbol: &str) -> Result<Vec<Location>, CommandError> {
    let mut sites = Vec::new();
    for path in crate::collect_java_files(root)? {
        scan_java_file_for_sites(root, &path, symbol, &mut sites);
    }
    Ok(sites)
}

/// Appends every classified whole-word site of `symbol` in one Java file. A file that cannot be read
/// contributes nothing, and one that does not mention the name is never classified.
fn scan_java_file_for_sites(root: &Path, path: &Path, symbol: &str, sites: &mut Vec<Location>) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    let matches = word_bounded_matches(&source, symbol);
    if matches.is_empty() {
        return;
    }
    let coordinates: Vec<(u32, u32)> = matches.clone();
    let kinds = classify_java_sites(&source, &coordinates);
    let display = normalized_path(root, path);
    for ((line, _), kind) in matches.into_iter().zip(kinds) {
        sites.push(Location::new(display.clone(), line).with_kind(kind));
    }
}

/// The skeleton of each file carrying a site, parsed once, for attributing a Kotlin text reference to
/// its enclosing declaration. A file that cannot be read or parsed is absent, so its sites render
/// unattributed rather than failing the scan.
fn skeletons_for_sites(root: &Path, sites: &[Location]) -> Vec<FileSkeleton> {
    let mut skeletons = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for site in sites {
        if seen.insert(site.path.clone()) {
            if let Some(skeleton) = crate::trace::skeleton_at(root, &site.path) {
                skeletons.push(skeleton);
            }
        }
    }
    skeletons
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The word-boundary contract in one table: a bare use, a use after a dot and after an `@`, all
    /// match; a name embedded in a longer identifier on either side does not; and a match inside a
    /// string is still reported here (classification, not this scan, is what separates prose from
    /// code). The UTF-16 column accounts for a multi-byte character before the match.
    #[test]
    fn whole_word_matches_are_found_with_utf16_columns_and_embedded_names_are_not() {
        let source = concat!(
            "val m = metrics.putMetric(1)\n",
            "@putMetric\n",
            "fun putMetricValue() {}\n",
            "// putMetric here\n",
            "val s = \"é putMetric\"\n",
        );

        let observed = word_bounded_matches(source, "putMetric");

        assert_eq!(observed, vec![(1, 17), (2, 2), (4, 4), (5, 12)]);
    }

    /// KT-112 scanning end to end over a temp workspace with one Java and one Kotlin file: the Java
    /// scan counts a comment mention apart from a code use and does not read the Kotlin file, while
    /// the Kotlin scan attributes its hit to the enclosing declaration. Composed as one value.
    #[test]
    fn the_java_and_kotlin_scans_separate_sources_count_mentions_and_attribute_kotlin() {
        use std::fs;
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        fs::create_dir_all(root.join("j")).expect("mkdir j");
        fs::create_dir_all(root.join("k")).expect("mkdir k");
        fs::write(
            root.join("j/A.java"),
            "class A {\n  void run() { executeUpdate(1); }\n  // executeUpdate note\n}\n",
        )
        .expect("write java");
        fs::write(
            root.join("k/B.kt"),
            "package app\nclass B {\n  fun run() { executeUpdate(2) }\n}\n",
        )
        .expect("write kotlin");

        let java = java_text_references(root, "executeUpdate", None)
            .expect("scan")
            .expect("java hits");
        let kotlin = kotlin_text_references(root, "executeUpdate", None)
            .expect("scan")
            .expect("kotlin hits");

        let observed = (
            java.total_sites,
            java.text_mention_sites,
            java.file_count,
            kotlin.total_sites,
            kotlin.groups[0].sites[0].enclosing.clone(),
        );
        assert_eq!(observed, (2, 1, 1, 1, Some("app.B.run".to_string())));
    }
}
