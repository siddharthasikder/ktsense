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
    build_annotated, build_text_references, build_text_references_attributed,
    build_text_references_with_text, classify_java_sites, fully_qualified_enclosing,
    java_enclosing_declarations, render_annotated_markdown, render_text_references_markdown,
    render_text_references_titled, AnnotatedDeclaration, AnnotatedDeclarations, FileSkeleton,
    ForeignReference, Location, TextReferences,
};

use crate::{collect_kotlin_files, normalized_path, CommandError, CommandOutcome, Exit, Format};

/// The exit a name nothing declares ends with, unchanged from before this listing existed: the
/// answer on stdout, the status still a failure the caller can branch on.
const NOT_FOUND_EXIT: Exit = Exit::Failure;

/// The sentence the not-found wording gains when `.java` sources under the root hold the name, so a
/// reader knows a Java-only name or a generated accessor (Lombok, AutoValue, Immutables generate
/// setters and getters no source declares) is accounted for in the Java section below rather than
/// absent (KT-115).
const JAVA_NOT_FOUND_SENTENCE: &str =
    "Java-only names and generated accessors (Lombok, AutoValue, Immutables) appear under Java \
     text references below.";

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
    let java = java_text_references(root, symbol, limit)?;
    let text = present(
        symbol,
        annotated.as_ref(),
        references,
        java,
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
/// nothing serializes exactly as before (KT-109). `java_text_references` carries the Java sites when
/// `.java` sources hold the name, skipped otherwise so a root with no Java is byte-identical (KT-115).
#[derive(serde::Serialize)]
struct NotFoundAnswer<'a> {
    found: bool,
    symbol: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotated: Option<&'a AnnotatedDeclarations>,
    text_references: TextReferences,
    #[serde(skip_serializing_if = "Option::is_none")]
    java_text_references: Option<TextReferences>,
}

fn present(
    symbol: &str,
    annotated: Option<&AnnotatedDeclarations>,
    references: TextReferences,
    java: Option<TextReferences>,
    format: Format,
    command: &str,
    root: &Path,
) -> Result<String, CommandError> {
    match format {
        Format::Md => {
            let mut out = crate::no_symbol_message_scoped(symbol, root);
            if java.is_some() {
                out.push_str("\n\n");
                out.push_str(JAVA_NOT_FOUND_SENTENCE);
            }
            out.push_str("\n\n");
            if let Some(annotated) = annotated {
                out.push_str(&render_annotated_markdown(annotated));
                out.push('\n');
            }
            out.push_str(&render_text_references_markdown(&references));
            if let Some(java) = &java {
                out.push('\n');
                out.push_str(&render_text_references_titled(java, "Java text references"));
            }
            Ok(out)
        }
        Format::Json => crate::as_json(&NotFoundAnswer {
            found: false,
            symbol,
            message: crate::no_symbol_message_scoped(symbol, root),
            annotated,
            text_references: references,
            java_text_references: java,
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
/// file mentions the name (KT-112). Each site carries its enclosing Java declaration and its source
/// line, so the listing reads like a KT-102 grep hit (KT-114). A root with no Java yields `None`, so
/// `trace` and `context` stay byte-identical on an all-Kotlin workspace.
pub(crate) fn java_text_references(
    root: &Path,
    symbol: &str,
    limit: Option<usize>,
) -> Result<Option<TextReferences>, CommandError> {
    let scan = scan_java(root, symbol)?;
    if scan.sites.is_empty() {
        return Ok(None);
    }
    Ok(Some(build_text_references_with_text(
        symbol,
        &scan.sites,
        limit,
        |path, line| scan.attribution(path, line),
    )))
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
/// path then line, each carrying its enclosing Java declaration and its source line (KT-114).
pub(crate) fn java_foreign_references(
    root: &Path,
    symbol: &str,
) -> Result<Vec<ForeignReference>, CommandError> {
    let scan = scan_java(root, symbol)?;
    let mut references: Vec<ForeignReference> = scan
        .sites
        .iter()
        .map(|site| {
            let (enclosing, text) = scan.attribution(&site.path, site.line);
            ForeignReference {
                path: site.path.clone(),
                line: site.line,
                enclosing,
                text,
            }
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
                text: None,
            }
        })
        .collect();
    references.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Ok(references)
}

/// Every whole-word text match of `symbol` in the workspace's `.java` sources, each classified by
/// the pure Java lexer (KT-112) and attributed to its enclosing Java declaration and source line by
/// the pure Java enclosing scan (KT-114). A file that cannot be read is skipped rather than failing.
fn scan_java(root: &Path, symbol: &str) -> Result<JavaScan, CommandError> {
    let mut scan = JavaScan::default();
    for path in crate::collect_java_files(root)? {
        scan_java_file(root, &path, symbol, &mut scan);
    }
    Ok(scan)
}

/// Classified Java sites plus, per `(path, line)`, the enclosing declaration and the source line to
/// render beside the site. Keyed by line because a listing groups and renders by line.
#[derive(Default)]
struct JavaScan {
    sites: Vec<Location>,
    attribution: std::collections::HashMap<(String, u32), (Option<String>, Option<String>)>,
}

impl JavaScan {
    /// The enclosing declaration and source line of the site at `(path, line)`, or `(None, None)`
    /// when the scan recorded neither.
    fn attribution(&self, path: &str, line: u32) -> (Option<String>, Option<String>) {
        self.attribution
            .get(&(path.to_string(), line))
            .cloned()
            .unwrap_or((None, None))
    }
}

/// Appends every classified whole-word site of `symbol` in one Java file, and records each matched
/// line's enclosing declaration and source text. A file that cannot be read contributes nothing, and
/// one that does not mention the name is never scanned.
fn scan_java_file(root: &Path, path: &Path, symbol: &str, scan: &mut JavaScan) {
    let Ok(source) = std::fs::read_to_string(path) else {
        return;
    };
    let matches = word_bounded_matches(&source, symbol);
    if matches.is_empty() {
        return;
    }
    let display = normalized_path(root, path);
    let coordinates: Vec<(u32, u32)> = matches.clone();
    let kinds = classify_java_sites(&source, &coordinates);
    for ((line, _), kind) in matches.iter().copied().zip(kinds) {
        scan.sites
            .push(Location::new(display.clone(), line).with_kind(kind));
    }

    let lines: Vec<u32> = matches.iter().map(|&(line, _)| line).collect();
    let enclosings = java_enclosing_declarations(&source, &lines);
    let source_lines: Vec<&str> = source.lines().collect();
    for (line, enclosing) in lines.into_iter().zip(enclosings) {
        let text = source_lines
            .get(line as usize - 1)
            .map(|line| line.to_string());
        scan.attribution
            .insert((display.clone(), line), (enclosing, text));
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

    /// KT-114 scanning end to end over a temp workspace with one Java and one Kotlin file: the Java
    /// scan counts a comment mention apart from a code use, attributes the code use to its enclosing
    /// Java method and carries its source line, and does not read the Kotlin file, while the Kotlin
    /// scan attributes its hit to the enclosing declaration. Composed as one value.
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

        let java_code_site = &java.groups[0].sites[0];
        let observed = (
            java.total_sites,
            java.text_mention_sites,
            java.file_count,
            java_code_site.enclosing.clone(),
            java_code_site.text.clone(),
            kotlin.total_sites,
            kotlin.groups[0].sites[0].enclosing.clone(),
        );
        assert_eq!(
            observed,
            (
                2,
                1,
                1,
                Some("A.run".to_string()),
                Some("  void run() { executeUpdate(1); }".to_string()),
                1,
                Some("app.B.run".to_string()),
            )
        );
    }

    /// KT-115: a name declared nowhere, in a workspace holding both Kotlin and Java sources, lists
    /// its Java sites in the KT-114 layout under a `## Java text references` section after the Kotlin
    /// `## Text references`, and the not-found wording gains the generated-accessor sentence. A root
    /// with no `.java` file renders byte-identically, with neither the section nor the sentence.
    /// Composed over both workspaces and asserted once.
    #[test]
    fn a_name_declared_nowhere_lists_java_sites_after_kotlin_and_a_root_without_java_is_unchanged()
    {
        use std::fs;

        let kotlin =
            "package app\nclass Caller {\n    fun run() { bean.setStagingEnabled(true) }\n}\n";
        let mixed = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(mixed.path().join("k")).expect("mkdir k");
        fs::create_dir_all(mixed.path().join("j")).expect("mkdir j");
        fs::write(mixed.path().join("k/Caller.kt"), kotlin).expect("write kotlin");
        fs::write(
            mixed.path().join("j/Toggle.java"),
            "class Toggle {\n    void run() { bean.setStagingEnabled(true); }\n}\n",
        )
        .expect("write java");

        let kotlin_only = tempfile::tempdir().expect("temp dir");
        fs::write(kotlin_only.path().join("Caller.kt"), kotlin).expect("write kotlin");

        let render = |root: &Path| {
            not_found_outcome(root, "setStagingEnabled", None, Format::Md, "trace")
                .expect("renders")
                .text
        };

        let base = concat!(
            "no declaration named setStagingEnabled in this workspace; library and dependency ",
            "declarations are not searched, so use a text search for external types. No generated ",
            "Kotlin sources were found under build/generated, so if setStagingEnabled is generated, ",
            "run the build and retry",
        );
        let kotlin_section = |path: &str| {
            format!(
                concat!(
                    "## Text references (1 site in 1 file)\n",
                    "precision: text match\n",
                    "1 in code, 0 in comments or strings.\n",
                    "\n{path}\n- 3\n",
                ),
                path = path,
            )
        };
        let expected_mixed = format!(
            "{base}\n\n{sentence}\n\n{kotlin}\n{java}",
            sentence = JAVA_NOT_FOUND_SENTENCE,
            kotlin = kotlin_section("k/Caller.kt"),
            java = concat!(
                "## Java text references (1 site in 1 file)\n",
                "precision: text match\n",
                "1 in code, 0 in comments or strings.\n",
                "\nj/Toggle.java\n",
                "Toggle.run\n",
                "  2: void run() { bean.setStagingEnabled(true); }\n",
            ),
        );
        let expected_kotlin_only = format!("{base}\n\n{}", kotlin_section("Caller.kt"));

        assert_eq!(
            (render(mixed.path()), render(kotlin_only.path())),
            (expected_mixed, expected_kotlin_only),
        );
    }
}
