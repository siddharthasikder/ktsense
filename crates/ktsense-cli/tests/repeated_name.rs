//! KT-64: which occurrence of a repeated declaration name `trace` asks the engine about, proven on
//! the in-process path and on the warm-daemon path, and proven to be the same one.
//!
//! Line 3 of the fixture declares or mentions `value` three times: the function's own name at
//! character column 5, the parameter the engine reports at 27, and a reference to it at 41. Nine
//! astral-plane characters stand before the parameter, so the three units a caller might pass
//! disagree: its 0-based UTF-16 offset is 35 and its byte column is 54. Read as a character column,
//! 35 is nearer the reference at 41 than the parameter it names, and the leftmost match is a
//! different declaration altogether, so one line separates first-occurrence selection, raw-unit
//! comparison, and the correct answer.
//!
//! The proof mechanism is the replay engine's exact params check: each session script expects
//! `textDocument/implementation` and `textDocument/references` at the parameter and nowhere else, and
//! `fake_lsp` fails the run on any other position. An exit code of 0 therefore means the command asked
//! where it was supposed to; a wrong column cannot pass by answering the same set, which is how the
//! real engine would let it through, since `kmp-lsp` 0.26.0 resolves references by the identifier text
//! at the position.
//!
//! The two arms are driven so neither can borrow the other's answer: `KTSENSE_NO_DAEMON=1` forces the
//! in-process path, `KTSENSE_REQUIRE_DAEMON=1` forbids the in-process fallback, and the daemon arm is
//! given no `find` output at all, so a routed trace that fell back to command mode would resolve
//! nothing instead of quietly agreeing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::cargo::CommandCargoExt;
use serde_json::{json, Value};

const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const SHORT_INDEX_CAP_MS: &str = "150";
const RELATIVE: &str = "src/main/kotlin/shop/Three.kt";

const SOURCE: &str = "package shop\n\nfun value(/* 𝕊𝕊𝕊𝕊𝕊𝕊𝕊𝕊𝕊 */ value: Int) = value\n\nfun caller(): Int = value(1)\n";

/// The declaration line, and the character column of each whole-word `value` on it.
const DECLARATION_LINE: u32 = 3;
const PARAMETER_COLUMN: u32 = 27;
/// The parameter's 0-based UTF-16 offset, which is what an LSP `Position.character` carries and what
/// a raw comparison against character columns would resolve to the reference at column 41.
const PARAMETER_UTF16_OFFSET: u32 = 35;
/// What command-mode `find --json` reports: near the parameter rather than on it, the skew the engine
/// was measured to have.
const REPORTED_FIND_COLUMN: u32 = 24;

const CALL_LINE: u32 = 5;
const CALL_COLUMN: u32 = 21;

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// The replay engine, built on first use so a package-scoped run does not fail on a missing helper.
fn fake_lsp() -> PathBuf {
    static FAKE: OnceLock<PathBuf> = OnceLock::new();
    FAKE.get_or_init(|| {
        let path = Path::new(env!("CARGO_BIN_EXE_ktsense")).with_file_name("fake_lsp");
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
            let status = Command::new(cargo)
                .args(["build", "-p", "ktsense-lsp", "--bin", "fake_lsp"])
                .current_dir(WORKSPACE_ROOT)
                .status()
                .expect("cargo runs");
            assert!(status.success(), "building fake_lsp failed");
        }
        path
    })
    .clone()
}

/// A workspace holding only the repeated-name file, canonicalized so the daemon's root check and the
/// URIs in the scripts agree with what the command reports.
struct Fixture {
    _home: tempfile::TempDir,
    root: PathBuf,
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp home");
        let canonical = std::fs::canonicalize(home.path()).expect("canonical home");
        let root = canonical.join("root");
        let file = root.join(RELATIVE);
        std::fs::create_dir_all(file.parent().expect("file has a parent")).expect("create dirs");
        std::fs::write(&file, SOURCE).expect("write the source");
        let runtime = canonical.join("run");
        std::fs::create_dir_all(&runtime).expect("create the runtime dir");
        Self {
            _home: home,
            root,
            runtime,
        }
    }

    fn file(&self) -> PathBuf {
        self.root.join(RELATIVE)
    }

    fn uri(&self) -> String {
        format!("file://{}", self.file().display())
    }

    fn command(&self, script: &Value, extra_env: &[(&str, &str)]) -> Command {
        let mut command = Command::cargo_bin("ktsense").expect("binary builds");
        command
            .current_dir(WORKSPACE_ROOT)
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("KTSENSE_LSP_PATH", fake_lsp())
            .env("FAKE_LSP_SCRIPT", script.to_string())
            .env("KTSENSE_INDEX_CAP_MS", SHORT_INDEX_CAP_MS)
            .env("KTSENSE_DAEMON_IDLE_SECS", "20")
            .env_remove("FAKE_CMD_STDOUT")
            .args(["--root", self.root.to_str().expect("utf-8 root")]);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command
    }

    fn run(&self, script: &Value, extra_env: &[(&str, &str)], args: &[&str]) -> Run {
        let output = self
            .command(script, extra_env)
            .args(args)
            .output()
            .expect("binary runs");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    }
}

fn position(line: u32, column: u32) -> Value {
    json!({ "line": line - 1, "character": column - 1 })
}

fn at(uri: &str, line: u32, column: u32) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": position(line, column) })
}

fn location(uri: &str, line: u32, column: u32) -> Value {
    let start = position(line, column);
    json!({ "uri": uri, "range": { "start": start, "end": start } })
}

fn progress(value: Value) -> Value {
    json!({ "kind": "notify", "method": "$/progress",
            "params": { "token": "indexing", "value": value } })
}

/// The session every arm drives: handshake, a completed index, then the two positional requests
/// pinned to the parameter, then teardown. `warm_lookup` is the `workspace/symbol` step a routed
/// trace adds before them, and one an in-process trace sends only when command-mode `find` reported
/// nothing; here `find` reports the declaration, so the in-process arm never reaches it.
fn script(uri: &str, warm_lookup: Vec<Value>) -> Value {
    let mut steps = vec![
        json!({ "kind": "expect", "method": "initialize",
                "respond": { "result": { "capabilities": {} } } }),
        json!({ "kind": "expect", "method": "initialized" }),
        progress(json!({ "kind": "begin", "title": "Indexing" })),
        progress(json!({ "kind": "end" })),
    ];
    steps.extend(warm_lookup);
    steps.push(json!({
        "kind": "expect", "method": "textDocument/implementation",
        "params": at(uri, DECLARATION_LINE, PARAMETER_COLUMN),
        "respond": { "result": [] }
    }));
    let mut references = at(uri, DECLARATION_LINE, PARAMETER_COLUMN);
    references["context"] = json!({ "includeDeclaration": true });
    steps.push(json!({
        "kind": "expect", "method": "textDocument/references",
        "params": references,
        "respond": { "result": [
            location(uri, DECLARATION_LINE, PARAMETER_COLUMN),
            location(uri, CALL_LINE, CALL_COLUMN),
        ] }
    }));
    steps.push(json!({ "kind": "expect", "method": "shutdown", "respond": { "result": null } }));
    steps.push(json!({ "kind": "expect", "method": "exit" }));
    json!({ "steps": steps })
}

/// The one `workspace/symbol` exchange a routed trace makes: the exact-name query, answered with the
/// parameter's LSP position, whose `character` is a 0-based UTF-16 offset.
fn warm_lookup(uri: &str) -> Vec<Value> {
    vec![json!({
        "kind": "expect", "method": "workspace/symbol",
        "params": { "query": "value" },
        "respond": { "result": [ {
            "name": "value",
            "kind": 12,
            "location": {
                "uri": uri,
                "range": {
                    "start": { "line": DECLARATION_LINE - 1, "character": PARAMETER_UTF16_OFFSET },
                    "end": { "line": DECLARATION_LINE - 1, "character": PARAMETER_UTF16_OFFSET }
                }
            }
        } ] }
    })]
}

#[test]
fn a_repeated_name_is_traced_at_the_reported_occurrence_on_both_paths() {
    let fixture = Fixture::new();
    let uri = fixture.uri();
    let find = json!([{
        "file": fixture.file().display().to_string(),
        "line": DECLARATION_LINE,
        "col": REPORTED_FIND_COLUMN,
        "name": "value"
    }]);

    let in_process = {
        let mut command = fixture.command(&script(&uri, Vec::new()), &[("KTSENSE_NO_DAEMON", "1")]);
        let output = command
            .env("FAKE_CMD_STDOUT", find.to_string())
            .args(["trace", "value"])
            .output()
            .expect("binary runs");
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("utf-8 stdout"),
            stderr: String::from_utf8(output.stderr).expect("utf-8 stderr"),
        }
    };

    let warm_script = script(&uri, warm_lookup(&uri));
    let started = fixture.run(&warm_script, &[], &["daemon", "start"]);
    let warm = fixture.run(
        &warm_script,
        &[("KTSENSE_REQUIRE_DAEMON", "1")],
        &["trace", "value"],
    );
    let stopped = fixture.run(&warm_script, &[], &["daemon", "stop"]);

    let definition = format!("{RELATIVE}:{DECLARATION_LINE}");
    let observed = (
        (started.code, in_process.code, warm.code, stopped.code),
        in_process.stdout == warm.stdout,
        in_process.stdout.contains(&definition),
        in_process.stdout.contains("shop.caller"),
        in_process.stdout.contains("index: complete"),
        in_process.stderr.is_empty() && warm.stderr.is_empty(),
    );
    assert_eq!(
        observed,
        (
            (Some(0), Some(0), Some(0), Some(0)),
            true,
            true,
            true,
            true,
            true
        ),
        "in-process: {:?} {}\nwarm: {:?} {}\nstart: {}\nstop: {}",
        in_process.code,
        in_process.stdout,
        warm.code,
        warm.stdout,
        started.stderr,
        stopped.stderr,
    );
}
