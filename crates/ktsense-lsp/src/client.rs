//! Async client that drives a `kmp-lsp` child process over stdio.
//!
//! The client owns the child, a background reader task, and a map of in-flight request ids. Callers
//! issue [`LspClient::request`]/[`LspClient::notify`], run the [`LspClient::initialize`] handshake,
//! observe server notifications through [`LspClient::next_notification`], and always finish with
//! [`LspClient::shutdown`], which guarantees the child is gone before it returns.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex as TokioMutex};
use tokio::time::timeout;

use crate::framing::{self, FrameDecoder};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_EXIT_GRACE: Duration = Duration::from_secs(2);
const READ_CHUNK: usize = 8192;

/// Everything that can go wrong while talking to the engine.
#[derive(Debug, Error)]
pub enum LspError {
    #[error("failed to spawn engine `{path}`: {source}")]
    Spawn {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("i/o error talking to the engine: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not encode/decode LSP JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("request `{method}` timed out after {timeout:?}")]
    Timeout { method: String, timeout: Duration },
    #[error("engine answered `{method}` with error {code}: {message}")]
    Response {
        method: String,
        code: i64,
        message: String,
    },
    #[error("engine stream closed before `{method}` was answered")]
    ChildExited { method: String },
}

/// Options for the LSP `initialize` handshake.
pub struct InitializeConfig {
    pub root_uri: String,
    pub ignore_patterns: Vec<String>,
}

/// An unsolicited notification pushed by the engine (for example `$/progress`).
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// How the child terminated during [`LspClient::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// The child exited on its own within the grace period.
    Exited,
    /// The child ignored `exit` and had to be killed.
    Killed,
}

type ResponseError = (i64, String);
type ResponseOutcome = Result<Value, ResponseError>;
type Pending = Arc<StdMutex<HashMap<i64, oneshot::Sender<ResponseOutcome>>>>;

/// A running `kmp-lsp` child and the machinery to talk to it.
pub struct LspClient {
    child: tokio::process::Child,
    stdin: Arc<TokioMutex<tokio::process::ChildStdin>>,
    pending: Pending,
    notifications: mpsc::UnboundedReceiver<Notification>,
    next_id: AtomicI64,
    request_timeout: Duration,
    exit_grace: Duration,
}

impl LspClient {
    /// Spawns the engine at `binary`, wiring up framing and the reader task.
    pub async fn spawn(binary: &Path) -> Result<Self, LspError> {
        let mut command = Command::new(binary);
        command
            // kmp-lsp 0.26.0 writes env_logger INFO lines onto stdout unless RUST_LOG=error, which
            // corrupts the framing stream; suppress it at the source. (see AGENTS.md)
            .env("RUST_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| LspError::Spawn {
            path: binary.display().to_string(),
            source,
        })?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), notif_tx));

        Ok(Self {
            child,
            stdin: Arc::new(TokioMutex::new(stdin)),
            pending,
            notifications: notif_rx,
            next_id: AtomicI64::new(1),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            exit_grace: DEFAULT_EXIT_GRACE,
        })
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout;
    }

    pub fn set_exit_grace(&mut self, grace: Duration) {
        self.exit_grace = grace;
    }

    /// Sends a request, awaits the matching response (or times out), and returns its `result`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(err) = self.write_message(&message).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        match timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err((code, message)))) => Err(LspError::Response {
                method: method.to_string(),
                code,
                message,
            }),
            Ok(Err(_closed)) => Err(LspError::ChildExited {
                method: method.to_string(),
            }),
            Err(_elapsed) => {
                self.pending.lock().unwrap().remove(&id);
                Err(LspError::Timeout {
                    method: method.to_string(),
                    timeout: self.request_timeout,
                })
            }
        }
    }

    /// Sends a notification (a message with no id, expecting no reply).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_message(&message).await
    }

    /// Runs the `initialize` handshake and returns the server's `InitializeResult`.
    pub async fn initialize(&self, config: &InitializeConfig) -> Result<Value, LspError> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": config.root_uri,
            "capabilities": {},
            "initializationOptions": {
                "indexingOptions": { "ignorePatterns": config.ignore_patterns }
            }
        });
        let result = self.request("initialize", params).await?;
        // kmp-lsp 0.26.0 will not honour `shutdown`/`exit` until it has seen `initialized`; it must
        // follow the initialize response immediately or the child leaks. (see AGENTS.md)
        self.notify("initialized", json!({})).await?;
        Ok(result)
    }

    /// Returns the next server notification, or `None` once the engine stream has closed.
    pub async fn next_notification(&mut self) -> Option<Notification> {
        self.notifications.recv().await
    }

    /// Requests `shutdown`, sends `exit`, and guarantees the child is reaped: it waits a short grace
    /// period and kills the child if it has not exited, so no caller can leak the process.
    pub async fn shutdown(&mut self) -> Result<Teardown, LspError> {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        match timeout(self.exit_grace, self.child.wait()).await {
            Ok(Ok(_status)) => Ok(Teardown::Exited),
            Ok(Err(err)) => Err(err.into()),
            Err(_elapsed) => {
                self.child.kill().await?;
                self.child.wait().await?;
                Ok(Teardown::Killed)
            }
        }
    }

    async fn write_message(&self, message: &Value) -> Result<(), LspError> {
        let framed = framing::encode(&serde_json::to_vec(message)?);
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&framed).await?;
        stdin.flush().await?;
        Ok(())
    }
}

async fn read_loop(
    mut stdout: ChildStdout,
    pending: Pending,
    notifications: mpsc::UnboundedSender<Notification>,
) {
    let mut decoder = FrameDecoder::default();
    let mut chunk = vec![0u8; READ_CHUNK];
    'read: loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        decoder.push(&chunk[..read]);
        loop {
            match decoder.next_frame() {
                Ok(Some(body)) => dispatch(&body, &pending, &notifications),
                Ok(None) => continue 'read,
                Err(_malformed) => break 'read,
            }
        }
    }
    fail_pending(&pending);
}

fn dispatch(body: &[u8], pending: &Pending, notifications: &mpsc::UnboundedSender<Notification>) {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if message.get("id").is_none() {
            let _ = notifications.send(Notification {
                method: method.to_string(),
                params: message.get("params").cloned().unwrap_or(Value::Null),
            });
        }
        return;
    }
    if let Some(id) = message.get("id").and_then(Value::as_i64) {
        if let Some(sender) = pending.lock().unwrap().remove(&id) {
            let _ = sender.send(response_outcome(&message));
        }
    }
}

fn response_outcome(message: &Value) -> ResponseOutcome {
    if let Some(error) = message.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Err((code, text));
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}

fn fail_pending(pending: &Pending) {
    pending.lock().unwrap().clear();
}
