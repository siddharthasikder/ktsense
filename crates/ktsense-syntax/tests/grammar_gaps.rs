//! Pins the four Kotlin constructs `brokk-tree-sitter-kotlin` 0.4.6 cannot parse, isolated under
//! KT-52 from the five files `ktsense outline` rejects in the pinned corpora.
//!
//! Each construct is valid Kotlin taken from a release-tag file, so the ERROR node is an upstream
//! grammar defect, not this crate's. `kmp-lsp check` 0.26.0 agrees on every position, which is what
//! places the defect upstream.
//!
//! Three facts are pinned per construct, because any one alone can be satisfied by a fixture that
//! no longer means anything: the gap still produces an ERROR node, its one-token control still
//! parses cleanly, and the gap file still holds the construct it is named for. A gap turning green
//! is the signal that upstream has fixed the grammar and the fixture pair can retire.

use std::fs;
use std::path::{Path, PathBuf};

/// Each construct isolated by KT-52: the fixture pair that reproduces it, and the source fragment
/// that must remain in the `.gap.kt` half for the pair to still be about that construct.
const CONSTRUCTS: &[(&str, &str)] = &[
    ("declaration-then-callable-reference", "::local"),
    ("nullable-callable-reference", "String?::plus"),
    ("parenthesized-expression-statement", "(action)"),
    ("semicolon-opening-class-body", "{;"),
];

fn gap_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/grammar-gaps")
}

fn read(construct: &str, half: &str) -> String {
    fs::read_to_string(gap_root().join(format!("{construct}.{half}.kt")))
        .expect("readable grammar-gap fixture")
}

fn rejects(source: &str) -> bool {
    ktsense_syntax::parse(source)
        .expect("tree-sitter returns a tree even for input it cannot parse")
        .root_node()
        .has_error()
}

#[test]
fn every_known_grammar_gap_still_fails_and_its_control_still_parses() {
    let observed: Vec<(&str, bool, bool, bool)> = CONSTRUCTS
        .iter()
        .map(|(construct, fragment)| {
            let gap = read(construct, "gap");
            (
                *construct,
                gap.contains(fragment),
                rejects(&gap),
                rejects(&read(construct, "ok")),
            )
        })
        .collect();

    let expected: Vec<(&str, bool, bool, bool)> = CONSTRUCTS
        .iter()
        .map(|(construct, _)| (*construct, true, true, false))
        .collect();

    assert_eq!(
        observed, expected,
        "per construct: the gap holds its fragment, the gap still errors, the control still parses"
    );
}
