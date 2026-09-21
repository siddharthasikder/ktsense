//! Snapshot of `map` over the `multi-module` fixture.
//!
//! The map needs no engine, so this runs in the default install-free suite. The binary is invoked
//! with the workspace root as its working directory and a relative `--root`, so the pinned output
//! carries fixture-relative paths and never this machine's absolute paths.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn map(args: &[&str]) -> Run {
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

/// One composed record, so the snapshot proves the exit code and the silent stderr as well as the
/// text. A map that printed the right thing while reporting failure would still be wrong.
fn record(run: &Run) -> String {
    format!(
        "exit {}\nstderr {}\n---\n{}",
        run.code.expect("exited normally"),
        if run.stderr.is_empty() {
            "empty"
        } else {
            run.stderr.trim()
        },
        run.stdout
    )
}

#[test]
fn a_generous_budget_maps_every_fixture_file_most_central_package_first() {
    let run = map(&["map", "--budget", "4000", "--root", "fixtures/multi-module"]);

    insta::assert_snapshot!(record(&run));
}

#[test]
fn a_tight_budget_truncates_and_says_how_many_files_it_dropped() {
    let run = map(&["map", "--budget", "60", "--root", "fixtures/multi-module"]);

    insta::assert_snapshot!(record(&run));
}

#[test]
fn json_carries_the_same_ranking_with_the_budget_bound() {
    let run = map(&[
        "map",
        "--budget",
        "4000",
        "--root",
        "fixtures/multi-module",
        "--format",
        "json",
    ]);

    insta::assert_snapshot!(record(&run));
}

#[test]
fn dot_is_refused_because_only_deps_produces_a_graph() {
    let run = map(&[
        "map",
        "--budget",
        "4000",
        "--root",
        "fixtures/multi-module",
        "--format",
        "dot",
    ]);

    insta::assert_snapshot!(record(&run));
}

/// KT-22a end to end, on the shape the committed fixtures cannot express.
///
/// Every file here sits in one package, so no file imports another and every importer count is
/// zero. Ranking on imports alone therefore had nothing left but the path, and `Abstract.kt` led
/// the map by alphabet. `Central` is referenced by each of the three users while `AbstractThing` is
/// referenced once, so ranking on references puts `Central.kt` first instead.
///
/// The `multi-module` fixture cannot show this: it is properly multi-package with explicit imports,
/// so its import counts and its reference counts agree and its committed snapshot changes only in
/// the footer. This is why the acceptance corpus is a single-package one.
#[test]
fn inside_one_package_the_most_referenced_file_leads_where_imports_saw_nothing() {
    let corpus = tempfile::tempdir().expect("temp dir");
    let write = |name: &str, body: &str| {
        std::fs::write(corpus.path().join(name), body).expect("write source");
    };
    write(
        "Abstract.kt",
        "package one\n\nabstract class AbstractThing\n",
    );
    write("Central.kt", "package one\n\nclass Central\n");
    for user in ["UserA.kt", "UserB.kt", "UserC.kt"] {
        write(
            user,
            concat!(
                "package one\n",
                "\n",
                "class User {\n",
                "    fun build(): Central = Central()\n",
                "    fun also(c: Central): Central = c\n",
                "}\n",
            ),
        );
    }
    write(
        "Solo.kt",
        "package one\n\nclass Solo {\n    fun thing(): AbstractThing? = null\n}\n",
    );

    let run = map(&[
        "map",
        "--budget",
        "4000",
        "--root",
        &corpus.path().display().to_string(),
    ]);

    let ranked: Vec<String> = run
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .map(str::to_string)
        .collect();
    assert_eq!(
        (run.code, ranked.first().map(String::as_str), run.stderr),
        (Some(0), Some("Central.kt"), String::new()),
        "full map was: {}",
        run.stdout
    );
}
