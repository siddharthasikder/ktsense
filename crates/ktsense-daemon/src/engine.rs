//! The engine port the daemon serves, and the warm `LspClient` adapter behind it.
//!
//! The server core depends only on [`Engine`], so the daemon's transport, lifecycle and idle
//! handling are testable against a hand-built engine with no upstream install. [`WarmEngine`] is the
//! production adapter: it owns one initialized `LspClient` for the daemon's whole life, which is the
//! point of the daemon, and it decides what happens when that client faults.

use std::future::Future;

use ktsense_lsp::{InitializeConfig, LspClient, LspError};
use serde_json::Value;

/// One request routed from a connected client to the warm engine.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineRequest {
    pub method: String,
    pub params: Value,
}

/// What the engine produced for a request, and whether the daemon may keep serving afterwards.
#[derive(Debug, Clone, PartialEq)]
pub enum HandlerOutcome {
    /// A successful answer.
    Reply(Value),
    /// The request failed, but the warm session is intact and the daemon keeps serving.
    Error(String),
    /// The warm session reached a terminal fault; the daemon must stop rather than serve a dead
    /// session for the rest of its hour-long life.
    Faulted(String),
}

/// The behaviour the daemon serves. One warm session lives behind an implementation.
pub trait Engine: Send + Sync + 'static {
    fn handle(&self, request: EngineRequest) -> impl Future<Output = HandlerOutcome> + Send;
    fn shutdown(self) -> impl Future<Output = ()> + Send;
}

/// The production engine: one initialized `LspClient` kept warm for the daemon's whole life.
pub struct WarmEngine {
    client: LspClient,
}

impl WarmEngine {
    /// Launches the upstream engine and runs the `initialize` handshake, returning an engine ready
    /// to serve. This is the thin call a later CLI `daemon start` wraps around [`crate::run`].
    pub async fn warm_up(config: InitializeConfig) -> Result<Self, LspError> {
        let client = ktsense_lsp::launch().await?;
        client.initialize(&config).await?;
        Ok(Self { client })
    }

    /// Wraps an already-spawned client, so a session prepared elsewhere can back the daemon.
    pub fn from_client(client: LspClient) -> Self {
        Self { client }
    }
}

impl Engine for WarmEngine {
    async fn handle(&self, request: EngineRequest) -> HandlerOutcome {
        outcome_for(self.client.request(&request.method, request.params).await)
    }

    async fn shutdown(self) {
        let mut client = self.client;
        let _ = client.shutdown().await;
    }
}

/// Maps a raw engine result to a handler outcome. A framing fault is terminal for the client, so it
/// stops the daemon; every other error is reported to the caller while the warm session lives on.
fn outcome_for(result: Result<Value, LspError>) -> HandlerOutcome {
    match result {
        Ok(value) => HandlerOutcome::Reply(value),
        Err(fault @ LspError::Framing(_)) => HandlerOutcome::Faulted(fault.to_string()),
        Err(other) => HandlerOutcome::Error(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktsense_lsp::FramingError;
    use std::time::Duration;

    #[test]
    fn only_a_framing_fault_stops_the_daemon() {
        let observed = [
            outcome_for(Ok(Value::Null)),
            outcome_for(Err(LspError::Framing(FramingError::MissingContentLength))),
            outcome_for(Err(LspError::Timeout {
                method: "textDocument/references".to_string(),
                timeout: Duration::from_secs(10),
            })),
        ]
        .map(|outcome| matches!(outcome, HandlerOutcome::Faulted(_)));

        assert_eq!(observed, [false, true, false]);
    }
}
