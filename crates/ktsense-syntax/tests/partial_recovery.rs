//! Partial-parse recovery (KT-52a): a file with a localized ERROR node yields the declarations
//! that parsed, marked partial, instead of the whole-file rejection KT-52 documented. A file whose
//! every token is garbage recovers nothing and fails, so a partial skeleton is never an empty one
//! dressed as a confident answer.
//!
//! The four `.gap.kt` fixtures are the same four constructs `grammar_gaps.rs` pins as still failing
//! to parse; here they prove the recovered outline around each. The `.ok.kt` controls parse clean
//! and must never be marked partial.

use std::fs;
use std::path::{Path, PathBuf};

fn gap_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/grammar-gaps")
}

fn read(construct: &str, half: &str) -> String {
    fs::read_to_string(gap_root().join(format!("{construct}.{half}.kt")))
        .expect("readable grammar-gap fixture")
}

/// Per construct: the gap recovers the top-level declaration the error sat inside, marks the
/// skeleton partial, and the one-token control recovers the same declaration unmarked.
#[test]
fn every_gap_recovers_its_top_level_declaration_and_is_marked_partial() {
    let constructs = [
        ("declaration-then-callable-reference", "call"),
        ("nullable-callable-reference", "reference"),
        ("parenthesized-expression-statement", "call"),
        ("semicolon-opening-class-body", "Holder"),
    ];

    let observed: Vec<(&str, bool, bool, usize)> = constructs
        .iter()
        .map(|(construct, name)| {
            let gap =
                ktsense_syntax::extract(format!("{construct}.gap.kt"), &read(construct, "gap"))
                    .expect("gap recovers a partial skeleton");
            let ok = ktsense_syntax::extract(format!("{construct}.ok.kt"), &read(construct, "ok"))
                .expect("control extracts");
            (
                *construct,
                gap.partial && gap.declarations.iter().any(|d| d.name.contains(name)),
                ok.partial,
                gap.declaration_count(),
            )
        })
        .collect();

    let expected: Vec<(&str, bool, bool, usize)> = constructs
        .iter()
        .map(|(construct, _)| {
            (
                *construct,
                true,
                false,
                if *construct == "semicolon-opening-class-body" {
                    2
                } else {
                    1
                },
            )
        })
        .collect();

    assert_eq!(observed, expected);
}

/// A file whose every token is garbage collapses to a top-level ERROR node with nothing beneath it,
/// so extraction recovers no structure and fails rather than returning an empty confident skeleton.
#[test]
fn all_garbage_recovers_nothing_and_fails_rather_than_returning_an_empty_skeleton() {
    let recovered =
        ktsense_syntax::extract("broken.kt", "class Broken( fun {{{ @@@ >>>\n").is_err();

    assert!(recovered);
}
