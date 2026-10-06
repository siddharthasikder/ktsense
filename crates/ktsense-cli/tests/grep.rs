//! End-to-end tests for the `grep` command against the `multi-module` fixture. The hit count is
//! pinned to the `rg -c` total for the same pattern over the fixture's `.kt` files (KT-102), the
//! grouping and labels are checked in Markdown, and the structured answer is checked in JSON.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/multi-module");

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn grep(extra: &[&str]) -> Run {
    let mut args = vec!["--root", FIXTURE];
    args.extend_from_slice(extra);
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .args(&args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// The whole Markdown answer for `save|OrderId`: the text-match precision, the hit total that
/// equals `rg -c` over the fixture (16 lines across 7 files, all in code), the production label,
/// a hit attributed to its enclosing declaration, and an import hit under the file header.
#[test]
fn markdown_groups_every_hit_under_its_file_and_declaration_with_the_rg_count() {
    let run = grep(&["--format", "md", "grep", "save|OrderId"]);

    let observed = (
        run.code,
        run.stdout.contains("precision: text match"),
        run.stdout
            .contains("16 hits in 7 files. 16 in code, 0 in comments or strings."),
        run.stdout
            .contains("### core/src/main/kotlin/shop/order/OrderRepository.kt (production)"),
        run.stdout
            .contains("shop.order.OrderRepository.save\n  4: fun save(order: Order): OrderId"),
        run.stdout
            .contains("(file header)\n  5: import shop.order.OrderId"),
        run.stderr.is_empty(),
    );

    assert_eq!(observed, (Some(0), true, true, true, true, true, true));
}

/// JSON carries the same answer as data: the precision, the pre-cap total, the file count, and one
/// file's declaration-grouped hits with the enclosing FQN and the production flag.
#[test]
fn json_carries_the_precision_totals_and_declaration_grouped_hits() {
    let run = grep(&["--format", "json", "grep", "save|OrderId"]);
    let document: serde_json::Value = serde_json::from_str(&run.stdout).expect("valid JSON");

    let repository = document["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|file| file["path"] == "core/src/main/kotlin/shop/order/OrderRepository.kt")
        .expect("the repository file is present")
        .clone();

    let observed = (
        run.code,
        document["precision"].as_str(),
        document["total_hits"].as_u64(),
        document["file_count"].as_u64(),
        document["text_mention_hits"].as_u64(),
        repository["test"].as_bool(),
        repository["declarations"][0]["fqn"].as_str(),
        repository["declarations"][0]["hits"][0]["line"].as_u64(),
    );

    assert_eq!(
        observed,
        (
            Some(0),
            Some("text match"),
            Some(16),
            Some(7),
            Some(0),
            Some(false),
            Some("shop.order.OrderRepository.save"),
            Some(4),
        )
    );
}

/// The `--path` prefix narrows the search to a subtree, the per-file `--limit` caps hits and
/// reports the dropped count, and an invalid pattern is a failure with a clear message rather than
/// a panic. Checked as one table across the three behaviours.
#[test]
fn path_narrows_limit_caps_and_an_invalid_pattern_fails_cleanly() {
    let scoped = grep(&["grep", "OrderId", "--path", "core/"]);
    let capped = grep(&[
        "--format", "json", "grep", "OrderId", "--path", "db/", "--limit", "1",
    ]);
    let capped_json: serde_json::Value = serde_json::from_str(&capped.stdout).expect("valid JSON");
    let broken = grep(&["grep", "("]);

    let scoped_only_core = scoped
        .stdout
        .lines()
        .filter(|line| line.starts_with("### "))
        .all(|line| line.contains("core/"));
    let first_capped_file = capped_json["files"][0].clone();

    let observed = (
        scoped.code,
        scoped_only_core,
        capped.code,
        first_capped_file["declarations"][0]["hits"]
            .as_array()
            .map(Vec::len),
        first_capped_file["omitted"].as_u64(),
        broken.code,
        broken.stderr.contains("invalid search pattern"),
    );

    assert_eq!(
        observed,
        (Some(0), true, Some(0), Some(1), Some(3), Some(1), true)
    );
}

/// A pattern that begins with `-` is a pattern, not a flag: Kotlin's `->` and a negative literal are
/// ordinary things to search for, and the MCP tool passes the pattern as a bare argument with no
/// `--` before it. `->` occurs in no fixture source, so it is a search that matched nothing.
#[test]
fn a_pattern_that_begins_with_a_hyphen_is_searched_rather_than_parsed_as_a_flag() {
    let arrow = grep(&["--format", "json", "grep", "->"]);
    let arrow_json: serde_json::Value = serde_json::from_str(&arrow.stdout).unwrap_or_default();

    let observed = (
        arrow.code,
        arrow_json["total_hits"].as_u64(),
        arrow.stderr.contains("unexpected argument"),
    );

    assert_eq!(observed, (Some(0), Some(0), false));
}
