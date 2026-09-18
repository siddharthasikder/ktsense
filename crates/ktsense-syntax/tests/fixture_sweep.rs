//! The sweep the card's acceptance names: every Kotlin file in `fixtures/tiny-app` parses with no
//! ERROR node and yields declarations.
//!
//! `kmp-lsp check` reports all 18 fixture files OK, so a failure here is this crate's grammar
//! binding or extraction, never a malformed fixture.

use std::fs;
use std::path::{Path, PathBuf};

use ktsense_core::{render_skeleton, RenderOptions};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/tiny-app/src/main/kotlin")
        .canonicalize()
        .expect("fixture root exists")
}

fn kotlin_files(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(directory).expect("readable fixture directory") {
        let path = entry.expect("readable entry").path();
        if path.is_dir() {
            found.extend(kotlin_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "kt") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// A file that parses cleanly but yields nothing would be a silent extraction failure, so both
/// facts are gathered per file and asserted together.
#[test]
fn every_fixture_file_parses_without_error_nodes_and_yields_declarations() {
    let files = kotlin_files(&fixture_root());

    let offenders: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let source = fs::read_to_string(path).expect("readable fixture file");
            let relative = path.strip_prefix(fixture_root()).unwrap_or(path);
            let tree = ktsense_syntax::parse(&source).expect("parse");
            let skeleton =
                ktsense_syntax::extract(relative.to_string_lossy(), &source).expect("extract");

            match (tree.root_node().has_error(), skeleton.declaration_count()) {
                (false, count) if count > 0 => None,
                (has_error, count) => Some(format!(
                    "{}: has_error={has_error} declarations={count}",
                    relative.display()
                )),
            }
        })
        .collect();

    assert_eq!(
        (files.len(), offenders),
        (15, Vec::new()),
        "expected 15 fixture files, all clean"
    );
}

/// Guards the claim the fixture README makes about `internals.kt`: nothing public. Rendering it with
/// default options must produce nothing at all, and with `--private` must produce something.
#[test]
fn the_all_private_fixture_file_renders_empty_by_default() {
    let path = fixture_root().join("app/util/internals.kt");
    let source = fs::read_to_string(&path).expect("readable");
    let skeleton = ktsense_syntax::extract("app/util/internals.kt", &source).expect("extract");

    let observed = (
        render_skeleton(&skeleton, &RenderOptions::default()),
        render_skeleton(&skeleton, &RenderOptions::default().with_private())
            .lines()
            .count(),
    );

    assert_eq!(observed, (String::new(), 4));
}

/// The one external import in the fixture, which KT-20 will count as its single external edge.
#[test]
fn imports_are_captured_verbatim_including_the_single_external_one() {
    let path = fixture_root().join("app/Main.kt");
    let source = fs::read_to_string(&path).expect("readable");
    let skeleton = ktsense_syntax::extract("app/Main.kt", &source).expect("extract");

    let external: Vec<&String> = skeleton
        .imports
        .iter()
        .filter(|import| !import.starts_with("app."))
        .collect();

    assert_eq!(
        (skeleton.imports.len(), external),
        (9, vec![&"kotlinx.coroutines.runBlocking".to_string()])
    );
}
