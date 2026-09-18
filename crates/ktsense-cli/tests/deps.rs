//! End-to-end tests for the `deps` command against the `multi-module` fixture, whose README freezes
//! the expected counts and the single planted cycle.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/multi-module");

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn deps(extra: &[&str]) -> Run {
    let mut args = vec!["--root", FIXTURE];
    args.extend_from_slice(extra);
    args.push("deps");
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

#[test]
fn markdown_reports_the_frozen_counts_and_the_single_planted_cycle() {
    let run = deps(&["--format", "md"]);

    let observed = (
        run.code,
        run.stdout.contains("4 packages"),
        run.stdout.contains("6 edges"),
        run.stdout.contains("3 external imports"),
        run.stdout.contains("1 cycle"),
        run.stdout
            .contains("{ shop.app.checkout, shop.app.reporting }"),
        run.stderr.is_empty(),
    );

    assert_eq!(observed, (Some(0), true, true, true, true, true, true));
}

#[test]
fn json_pins_the_node_edge_external_and_cycle_contract() {
    let run = deps(&["--format", "json"]);
    let document: serde_json::Value = serde_json::from_str(&run.stdout).expect("valid JSON");

    let cycle: Vec<&str> = document["cycles"][0]
        .as_array()
        .expect("one cycle")
        .iter()
        .map(|member| member.as_str().expect("string member"))
        .collect();

    let observed = (
        run.code,
        document["level"].as_str(),
        document["nodes"].as_array().map(Vec::len),
        document["edges"].as_array().map(Vec::len),
        document["external"].as_array().map(Vec::len),
        document["cycles"].as_array().map(Vec::len),
        cycle,
    );

    assert_eq!(
        observed,
        (
            Some(0),
            Some("package"),
            Some(4),
            Some(6),
            Some(3),
            Some(1),
            vec!["shop.app.checkout", "shop.app.reporting"],
        )
    );
}

#[test]
fn dot_emits_a_digraph_carrying_the_resolved_edges() {
    let run = deps(&["--format", "dot"]);

    let observed = (
        run.code,
        run.stdout.contains("digraph deps {"),
        run.stdout.contains("\"shop.db\" -> \"shop.order\""),
        run.stderr.is_empty(),
    );

    assert_eq!(observed, (Some(0), true, true, true));
}
