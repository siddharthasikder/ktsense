//! `outline` over a file with a localized parse error recovers the declarations that parsed and
//! marks the answer partial, instead of the whole-file rejection KT-52 pinned. The four
//! `fixtures/grammar-gaps/*.gap.kt` files are the same four constructs `ktsense-syntax`'s
//! `grammar_gaps.rs` proves still fail to parse; here they prove the recovered `outline` around
//! each, in both Markdown and JSON.
//!
//! Behaviour is asserted directly and the rendered text is pinned as a golden, so a regression in
//! the exit status, the partial marker, or the recovered signatures fails loudly.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const GAP_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/grammar-gaps");

const GAPS: [&str; 4] = [
    "declaration-then-callable-reference",
    "nullable-callable-reference",
    "parenthesized-expression-statement",
    "semicolon-opening-class-body",
];

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn outline(relative: &str, format: &str) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(GAP_DIR)
        .args(["outline", relative, "--format", format])
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// Every gap outlines successfully, announces itself partial in the Markdown block, and says so as
/// data in JSON; the one-token control parses clean and carries no partial marker either way.
#[test]
fn a_gap_outlines_partial_while_its_control_outlines_clean() {
    let observed: Vec<(&str, bool, bool, bool, bool)> = GAPS
        .iter()
        .map(|gap| {
            let md = outline(&format!("{gap}.gap.kt"), "md");
            let json = outline(&format!("{gap}.gap.kt"), "json");
            let control = outline(&format!("{gap}.ok.kt"), "md");
            let document: serde_json::Value =
                serde_json::from_str(&json.stdout).expect("partial outline is valid JSON");
            (
                *gap,
                md.code == Some(0) && md.stderr.is_empty(),
                md.stdout.contains("// partial:"),
                document["partial"].as_bool() == Some(true),
                control.code == Some(0) && !control.stdout.contains("// partial:"),
            )
        })
        .collect();

    let expected: Vec<(&str, bool, bool, bool, bool)> = GAPS
        .iter()
        .map(|gap| (*gap, true, true, true, true))
        .collect();

    assert_eq!(observed, expected);
}

#[test]
fn partial_outline_goldens_are_pinned_in_both_formats() {
    for gap in GAPS {
        for format in ["md", "json"] {
            let run = outline(&format!("{gap}.gap.kt"), format);
            let record = format!(
                "exit: {}\nstderr: {}\n--- stdout ---\n{}",
                run.code
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                if run.stderr.is_empty() {
                    "(empty)".to_string()
                } else {
                    run.stderr
                },
                run.stdout
            );
            insta::assert_snapshot!(format!("{format}__{}", gap.replace('-', "_")), record);
        }
    }
}
