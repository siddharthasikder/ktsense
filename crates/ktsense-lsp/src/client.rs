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

use crate::framing::{self, FrameDecoder, FramingError};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
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
    #[error("engine produced a malformed frame and the client is faulted: {0}")]
    Framing(FramingError),
    #[error("`{uri}` is not a valid document URI")]
    InvalidUri { uri: String },
    #[error("{message}")]
    Incompatible { message: String },
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
type SharedChild = Arc<TokioMutex<tokio::process::Child>>;
type Fault = Arc<StdMutex<Option<FramingError>>>;

/// A running `kmp-lsp` child and the machinery to talk to it.
pub struct LspClient {
    child: SharedChild,
    stdin: EngineStdin,
    pending: Pending,
    notifications: Option<mpsc::UnboundedReceiver<Notification>>,
    next_id: AtomicI64,
    request_timeout: Duration,
    shutdown_timeout: Duration,
    exit_grace: Duration,
    fault: Fault,
}

/// The child's stdin, `None` once teardown has closed it: dropping the handle is what delivers EOF.
type EngineStdin = Arc<TokioMutex<Option<tokio::process::ChildStdin>>>;

fn stdin_closed() -> LspError {
    LspError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "the engine's stdin was closed by shutdown",
    ))
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
        let child: SharedChild = Arc::new(TokioMutex::new(child));
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let fault: Fault = Arc::new(StdMutex::new(None));
        let (notif_tx, notif_rx) = mpsc::unbounded_channel();
        let reader = Reader {
            pending: Arc::clone(&pending),
            notifications: notif_tx,
            fault: Arc::clone(&fault),
            child: Arc::clone(&child),
        };
        tokio::spawn(reader.run(stdout));

        Ok(Self {
            child,
            stdin: Arc::new(TokioMutex::new(Some(stdin))),
            pending,
            notifications: Some(notif_rx),
            next_id: AtomicI64::new(1),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            exit_grace: DEFAULT_EXIT_GRACE,
            fault,
        })
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout;
    }

    pub fn set_shutdown_timeout(&mut self, timeout: Duration) {
        self.shutdown_timeout = timeout;
    }

    pub fn set_exit_grace(&mut self, grace: Duration) {
        self.exit_grace = grace;
    }

    /// Sends a request, awaits the matching response (or times out), and returns its `result`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        self.request_within(method, params, self.request_timeout)
            .await
    }

    async fn request_within(
        &self,
        method: &str,
        params: Value,
        bound: Duration,
    ) -> Result<Value, LspError> {
        if let Some(fault) = self.current_fault() {
            return Err(LspError::Framing(fault));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let send_and_wait = async {
            self.write_message(&message).await?;
            match rx.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err((code, text))) => Err(LspError::Response {
                    method: method.to_string(),
                    code,
                    message: text,
                }),
                Err(_closed) => Err(LspError::ChildExited {
                    method: method.to_string(),
                }),
            }
        };

        match timeout(bound, send_and_wait).await {
            Ok(result) => result,
            Err(_elapsed) => {
                self.pending.lock().unwrap().remove(&id);
                Err(LspError::Timeout {
                    method: method.to_string(),
                    timeout: bound,
                })
            }
        }
    }

    /// Sends a notification (a message with no id, expecting no reply).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.notify_within(method, params, self.request_timeout)
            .await
    }

    async fn notify_within(
        &self,
        method: &str,
        params: Value,
        bound: Duration,
    ) -> Result<(), LspError> {
        if let Some(fault) = self.current_fault() {
            return Err(LspError::Framing(fault));
        }
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        match timeout(bound, self.write_message(&message)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(LspError::Timeout {
                method: method.to_string(),
                timeout: bound,
            }),
        }
    }

    fn current_fault(&self) -> Option<FramingError> {
        self.fault.lock().unwrap().clone()
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
        // The protocol requires `initialized` right after the initialize response and the fake
        // insists on it. It is not what ends the child: kmp-lsp 0.26.0 answers `shutdown` without it
        // and ignores `exit` with or without it; only stdin EOF terminates the engine. (see AGENTS.md)
        self.notify("initialized", json!({})).await?;
        Ok(result)
    }

    /// Returns the next server notification, or `None` once the engine stream has closed or the
    /// stream has been handed off with [`LspClient::take_notifications`].
    pub async fn next_notification(&mut self) -> Option<Notification> {
        match self.notifications.as_mut() {
            Some(notifications) => notifications.recv().await,
            None => None,
        }
    }

    /// Hands the notification stream to a dedicated consumer, so a long-lived session can track
    /// progress from a background task while requests keep flowing through `&self`. Afterwards this
    /// client sees no notifications itself. Returns `None` if the stream was already taken.
    pub fn take_notifications(&mut self) -> Option<mpsc::UnboundedReceiver<Notification>> {
        self.notifications.take()
    }

    /// Requests `shutdown`, sends `exit`, closes the child's stdin, and guarantees the child is
    /// reaped: the shutdown request and the `exit` notification each carry a short bound so a wedged
    /// child cannot stall teardown, and the child is killed if it has not exited within the grace
    /// period, so no caller can leak the process.
    pub async fn shutdown(&mut self) -> Result<Teardown, LspError> {
        let _ = self
            .request_within("shutdown", Value::Null, self.shutdown_timeout)
            .await;
        let _ = self
            .notify_within("exit", Value::Null, self.shutdown_timeout)
            .await;
        self.close_stdin().await;
        let mut child = self.child.lock().await;
        match timeout(self.exit_grace, child.wait()).await {
            Ok(Ok(_status)) => Ok(Teardown::Exited),
            Ok(Err(err)) => Err(err.into()),
            Err(_elapsed) => {
                child.kill().await?;
                child.wait().await?;
                Ok(Teardown::Killed)
            }
        }
    }

    /// kmp-lsp 0.26.0 never terminates on `exit`, with or without `initialized`; it ends only when
    /// its stdin reaches EOF (verified 2026-09-18, see AGENTS.md). Dropping the handle is that EOF.
    async fn close_stdin(&self) {
        self.stdin.lock().await.take();
    }

    async fn write_message(&self, message: &Value) -> Result<(), LspError> {
        let framed = framing::encode(&serde_json::to_vec(message)?);
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or_else(stdin_closed)?;
        stdin.write_all(&framed).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

/// Background task that drains the child's stdout, dispatching responses and notifications until the
/// stream closes or a frame cannot be decoded.
struct Reader {
    pending: Pending,
    notifications: mpsc::UnboundedSender<Notification>,
    fault: Fault,
    child: SharedChild,
}

impl Reader {
    async fn run(self, mut stdout: ChildStdout) {
        let mut decoder = FrameDecoder::default();
        let mut chunk = vec![0u8; READ_CHUNK];
        let mut decode_fault = None;
        'read: loop {
            let read = match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            decoder.push(&chunk[..read]);
            loop {
                match decoder.next_frame() {
                    Ok(Some(body)) => self.dispatch(&body),
                    Ok(None) => continue 'read,
                    Err(malformed) => {
                        decode_fault = Some(malformed);
                        break 'read;
                    }
                }
            }
        }
        if let Some(fault) = decode_fault {
            *self.fault.lock().unwrap() = Some(fault);
            let _ = self.child.lock().await.start_kill();
        }
        fail_pending(&self.pending);
    }

    fn dispatch(&self, body: &[u8]) {
        let Ok(message) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if message.get("id").is_none() {
                let _ = self.notifications.send(Notification {
                    method: method.to_string(),
                    params: message.get("params").cloned().unwrap_or(Value::Null),
                });
            }
            return;
        }
        if let Some(id) = message.get("id").and_then(Value::as_i64) {
            if let Some(sender) = self.pending.lock().unwrap().remove(&id) {
                let _ = sender.send(response_outcome(&message));
            }
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
