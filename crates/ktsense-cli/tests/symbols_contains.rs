//! `symbols --contains` over the `multi-module` fixture.
//!
//! The partial-name search reads the workspace's own syntax skeletons, so it needs no engine and
//! runs in the default install-free suite. The binary runs with the workspace root as its working
//! directory and a relative `--root`, so the answer carries fixture-relative paths.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn symbols(args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .current_dir(WORKSPACE_ROOT)
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

/// A `--contains Repository` search names every repository type, most relevant first, states its
/// source, and exits successfully. The whole composed result is one assertion: exit, the qualified
/// names in rank order, the source header, and the silent stderr.
#[test]
fn contains_lists_every_repository_in_rank_order_from_the_syntax_index() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "Repository",
        "--root",
        "fixtures/multi-module",
    ]);

    let qualified_names: Vec<String> = run
        .stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| token.contains("Repository"))
        .map(str::to_string)
        .collect();
    let states_source = run.stdout.contains("source: syntax index");

    assert_eq!(
        (run.code, qualified_names, states_source, run.stderr),
        (
            Some(0),
            vec![
                "shop.order.OrderRepository".to_string(),
                "shop.db.JdbcOrderRepository".to_string(),
                "shop.db.InMemoryOrderRepository".to_string(),
            ],
            true,
            String::new(),
        )
    );
}

/// A query nothing contains is a real "no such declaration" answer: exit 1, and the KT-87 scope
/// wording saying only this workspace was searched.
#[test]
fn contains_that_matches_nothing_exits_one_with_the_workspace_scope_wording() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "Nonexistent",
        "--root",
        "fixtures/multi-module",
    ]);

    assert_eq!(
        (
            run.code,
            run.stderr.contains("in this workspace"),
            run.stdout,
        ),
        (Some(1), true, String::new())
    );
}

/// The JSON answer carries what the Markdown header says: where the matches came from and how many
/// the cap left out, so a tool chaining on JSON learns the listing is partial. `--limit 1` over the
/// three repository types keeps the best-ranked one and reports the other two.
#[test]
fn contains_json_states_its_source_and_the_matches_the_cap_hid() {
    let run = symbols(&[
        "symbols",
        "--contains",
        "Repository",
        "--limit",
        "1",
        "--format",
        "json",
        "--root",
        "fixtures/multi-module",
    ]);
    let parsed: serde_json::Value = serde_json::from_str(&run.stdout).expect("json stdout");

    assert_eq!(
        (
            run.code,
            parsed["source"].clone(),
            parsed["matches"].as_array().map(Vec::len),
            parsed["matches"][0]["fqn"].clone(),
            parsed["more_matches"].clone(),
        ),
        (
            Some(0),
            serde_json::json!("syntax index"),
            Some(1),
            serde_json::json!("shop.order.OrderRepository"),
            serde_json::json!(2),
        )
    );
}
