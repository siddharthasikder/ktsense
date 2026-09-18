//! Fake `kmp-lsp` replay engine used by the `ktsense-lsp` adapter tests.
//!
//! This is a spawnable helper binary, not a library: a test starts it as a child process and talks
//! to it over stdin/stdout using LSP framing (`Content-Length: <n>\r\n\r\n<json>`). It replays a
//! declarative JSON script supplied through the `FAKE_LSP_SCRIPT` environment variable so the
//! default `cargo test` exercises the client without any real upstream install.
//!
//! Script shape (see `crates/ktsense-lsp/tests/replay.rs` for worked examples):
//!
//! ```json
//! {
//!   "noisy": false,
//!   "steps": [
//!     { "kind": "expect", "method": "initialize", "respond": { "result": { "capabilities": {} } } },
//!     { "kind": "notify", "method": "$/progress", "params": { "token": "idx" } },
//!     { "kind": "expect", "method": "initialized" },
//!     { "kind": "delay", "ms": 25 },
//!     { "kind": "expect", "method": "shutdown", "respond": { "result": null } },
//!     { "kind": "expect", "method": "exit" }
//!   ]
//! }
//! ```
//!
//! `FAKE_LSP_SCRIPT` holds either inline JSON (a value starting with `{`) or a path to a JSON file.

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

const SCRIPT_ENV: &str = "FAKE_LSP_SCRIPT";

#[derive(Deserialize)]
struct Script {
    #[serde(default)]
    noisy: bool,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Step {
    Expect {
        method: String,
        #[serde(default)]
        params: Option<Value>,
        #[serde(default)]
        respond: Option<Value>,
    },
    Notify {
        method: String,
        #[serde(default)]
        params: Option<Value>,
    },
    Delay {
        ms: u64,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("fake_lsp: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let script = load_script()?;
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if script.noisy && quiet_logging_disabled() {
        emit_log_noise(&mut out)?;
    }

    let mut initialized_seen = false;
    for step in &script.steps {
        match step {
            Step::Delay { ms } => std::thread::sleep(Duration::from_millis(*ms)),
            Step::Notify { method, params } => {
                let message = json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params.clone().unwrap_or(Value::Null),
                });
                write_message(&mut out, &message)?;
            }
            Step::Expect {
                method,
                params,
                respond,
            } => {
                let message = read_message(&mut reader)?
                    .with_context(|| format!("expected `{method}` but stdin closed (EOF)"))?;
                verify_method(method, &message)?;
                if let Some(expected) = params {
                    verify_params(method, expected, &message)?;
                }
                if method == "initialized" {
                    initialized_seen = true;
                }
                if method == "exit" {
                    // kmp-lsp 0.26.0 ignores `exit` until it has received the `initialized`
                    // notification, so a client that skips it must kill the child. Reproduce that
                    // hang rather than exiting cleanly, so the buggy client fails its test.
                    if initialized_seen {
                        return Ok(());
                    }
                    park_until_killed();
                }
                if let Some(respond) = respond {
                    let id = message.get("id").cloned().with_context(|| {
                        format!("script responds to `{method}` but the request carried no id")
                    })?;
                    write_message(&mut out, &build_response(id, respond)?)?;
                }
            }
        }
    }
    Ok(())
}

fn load_script() -> Result<Script> {
    let raw = env::var(SCRIPT_ENV).with_context(|| {
        format!("{SCRIPT_ENV} must hold inline JSON or a path to a JSON script")
    })?;
    let text = if raw.trim_start().starts_with('{') {
        raw
    } else {
        fs::read_to_string(&raw).with_context(|| format!("reading script file `{raw}`"))?
    };
    serde_json::from_str(&text).context("parsing the fake_lsp script as JSON")
}

fn quiet_logging_disabled() -> bool {
    // The real engine only stops writing env_logger INFO lines onto the protocol stream when
    // spawned with RUST_LOG=error. Mirror that so a client that forgets the override meets the
    // same corrupted stream the real one would produce.
    env::var("RUST_LOG").ok().as_deref() != Some("error")
}

fn emit_log_noise(out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "[2026-01-01T00:00:00Z INFO  kmp_lsp] starting language server"
    )?;
    writeln!(
        out,
        "[2026-01-01T00:00:00Z INFO  kmp_lsp::index] indexing workspace"
    )?;
    out.flush()?;
    Ok(())
}

fn verify_method(expected: &str, message: &Value) -> Result<()> {
    let actual = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("<none>");
    if actual == expected {
        return Ok(());
    }
    bail!("expected LSP method `{expected}` but received `{actual}`\n  full message: {message}");
}

fn verify_params(method: &str, expected: &Value, message: &Value) -> Result<()> {
    let actual = message.get("params").unwrap_or(&Value::Null);
    if actual == expected {
        return Ok(());
    }
    bail!("`{method}` params mismatch\n  expected: {expected}\n  actual:   {actual}");
}

fn build_response(id: Value, respond: &Value) -> Result<Value> {
    if let Some(result) = respond.get("result") {
        return Ok(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }
    if let Some(error) = respond.get("error") {
        return Ok(json!({ "jsonrpc": "2.0", "id": id, "error": error }));
    }
    bail!("a `respond` block must set `result` or `error`");
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().context("parsing Content-Length")?);
        }
    }
    let length = content_length.context("frame is missing a Content-Length header")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .context("parsing framed JSON body")
}

fn write_message(out: &mut impl Write, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

fn park_until_killed() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
