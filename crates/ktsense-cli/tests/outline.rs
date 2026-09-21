//! End-to-end tests for the `outline` command, driving the built binary with `assert_cmd`.
//!
//! Rendered text is asserted by structure, not byte-for-byte: the renderer and extractor are being
//! corrected under a separate card (KT-09), so pinning exact output here would be brittle.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tiny-app/src/main/kotlin/app/service/UserService.kt"
);

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn ktsense(args: &[&str]) -> Run {
    let output = Command::cargo_bin("ktsense")
        .expect("binary builds")
        .args(args)
        .output()
        .expect("binary runs");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
    }
}

#[test]
fn markdown_outline_is_the_public_skeleton_and_nothing_more() {
    let run = ktsense(&["outline", FIXTURE]);

    let observed = (
        run.code,
        run.stdout.contains("class UserService"),
        run.stdout.contains("```kotlin"),
        run.stdout.contains("isCached"),
        run.stderr.is_empty(),
    );

    assert_eq!(observed, (Some(0), true, true, false, true));
}

#[test]
fn private_flag_is_the_only_thing_that_reveals_private_declarations() {
    let hidden = ktsense(&["outline", FIXTURE]).stdout.contains("isCached");
    let revealed = ktsense(&["outline", FIXTURE, "--private"])
        .stdout
        .contains("isCached");

    assert_eq!((hidden, revealed), (false, true));
}

#[test]
fn kdoc_flag_is_the_only_thing_that_prints_documentation() {
    let without = ktsense(&["outline", FIXTURE]).stdout.contains("/**");
    let with = ktsense(&["outline", FIXTURE, "--kdoc"])
        .stdout
        .contains("/**");

    assert_eq!((without, with), (false, true));
}

#[test]
fn json_format_emits_the_queryable_serialized_skeleton() {
    let run = ktsense(&["outline", FIXTURE, "--format", "json"]);
    let document: serde_json::Value = serde_json::from_str(&run.stdout).expect("valid JSON");

    let observed = (
        run.code,
        document["package"].as_str(),
        document["declarations"][0]["name"].as_str(),
        document["declarations"][0]["kind"].as_str(),
    );

    assert_eq!(
        observed,
        (
            Some(0),
            Some("app.service"),
            Some("UserService"),
            Some("class")
        )
    );
}

#[test]
fn both_failure_paths_exit_one_with_a_message_that_names_the_cause_and_the_path() {
    let broken_path = format!("{}/broken.kt", env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(&broken_path, "class Broken( fun {{{ @@@ >>>\n").expect("write fixture");

    let missing = ktsense(&["outline", "/no/such/path/Absent.kt"]);
    let unparseable = ktsense(&["outline", &broken_path]);

    let observed = (
        (
            missing.code,
            missing.stderr.contains("no such file"),
            missing.stderr.contains("Absent.kt"),
            missing.stdout.is_empty(),
        ),
        (
            unparseable.code,
            unparseable.stderr.contains("syntax errors"),
            unparseable.stderr.contains("broken.kt"),
            unparseable.stdout.is_empty(),
        ),
    );

    assert_eq!(
        observed,
        ((Some(1), true, true, true), (Some(1), true, true, true))
    );
}

#[test]
fn a_non_kotlin_file_is_named_as_such_rather_than_blamed_on_kotlin_syntax() {
    let prose_path = format!("{}/notes.txt", env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(&prose_path, "just prose >>> not code @@@ {{{\n").expect("write fixture");

    let run = ktsense(&["outline", &prose_path]);

    let observed = (
        run.code,
        run.stderr.contains("not a Kotlin file"),
        run.stderr.contains("has Kotlin syntax errors"),
        run.stdout.is_empty(),
    );

    assert_eq!(observed, (Some(1), true, false, true));
}

#[test]
fn a_directory_argument_is_reported_as_unreadable_not_as_bad_kotlin() {
    let run = ktsense(&["outline", env!("CARGO_TARGET_TMPDIR")]);

    let observed = (
        run.code,
        run.stderr.contains("cannot read"),
        run.stderr.contains("syntax errors"),
        run.stdout.is_empty(),
    );

    assert_eq!(observed, (Some(1), true, false, true));
}

#[test]
fn an_empty_file_outlines_cleanly() {
    let empty_path = format!("{}/Empty.kt", env!("CARGO_TARGET_TMPDIR"));
    std::fs::write(&empty_path, "").expect("write fixture");

    let run = ktsense(&["outline", &empty_path]);

    assert_eq!((run.code, run.stderr.is_empty()), (Some(0), true));
}

#[test]
fn malformed_invocations_are_usage_errors_on_stderr_distinct_from_the_ambiguous_code() {
    let invocations: [&[&str]; 3] = [
        &["outline", "--nope", "/tmp/x.kt"],
        &[],
        &["outline", "f.kt", "--format", "xml"],
    ];

    let observed: Vec<(Option<i32>, bool, bool)> = invocations
        .iter()
        .map(|args| {
            let run = ktsense(args);
            (run.code, run.stderr.is_empty(), run.stdout.is_empty())
        })
        .collect();

    assert_eq!(
        observed,
        vec![
            (Some(2), false, true),
            (Some(2), false, true),
            (Some(2), false, true),
        ]
    );
}

#[test]
fn help_is_written_to_stdout_with_a_success_status() {
    let run = ktsense(&["--help"]);

    let observed = (
        run.code,
        run.stdout.contains("Usage"),
        run.stderr.is_empty(),
    );

    assert_eq!(observed, (Some(0), true, true));
}

#[test]
fn the_root_flag_resolves_a_relative_outline_path_instead_of_being_ignored() {
    let fixture = std::path::Path::new(FIXTURE);
    let dir = fixture
        .parent()
        .and_then(std::path::Path::to_str)
        .expect("fixture directory");
    let name = fixture
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("fixture file name");

    let with_root = ktsense(&["--root", dir, "outline", name]);
    let without_root = ktsense(&["outline", name]);

    let observed = (
        (
            with_root.code,
            with_root.stdout.contains("class UserService"),
        ),
        (
            without_root.code,
            without_root.stderr.contains("no such file"),
        ),
    );

    assert_eq!(observed, ((Some(0), true), (Some(1), true)));
}
