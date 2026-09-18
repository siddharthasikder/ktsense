//! Client side of the daemon protocol: connect, verify the hello, issue requests, ask it to stop.
//!
//! A later CLI stage uses this to route a command to a running daemon; the lifecycle helpers in
//! [`crate::server`] use it to detect and stop a daemon. Connecting verifies the daemon's protocol
//! version before any request is sent, so a stale binary left listening is refused rather than
//! trusted.

use std::path::Path;

use serde_json::Value;
use thiserror::Error;
use tokio::net::UnixStream;

use crate::wire::{self, ClientFrame, ServerFrame, WireError};
use crate::PROTOCOL_VERSION;

/// A protocol version that does not match this build's [`PROTOCOL_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolMismatch {
    pub expected: u32,
    pub actual: u32,
}

/// Everything that can go wrong talking to a daemon.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon transport error: {0}")]
    Wire(#[from] WireError),
    #[error("daemon closed the connection before sending its hello")]
    NoHello,
    #[error("daemon closed the connection before answering")]
    ClosedEarly,
    #[error("daemon speaks protocol {actual}, this build speaks {expected}")]
    Protocol { expected: u32, actual: u32 },
    #[error("daemon sent an unexpected frame")]
    UnexpectedFrame,
    #[error("daemon reported: {message}")]
    Engine { message: String },
}

/// Refuses a hello whose protocol version does not match this build.
pub fn verify_protocol(actual: u32) -> Result<(), ProtocolMismatch> {
    if actual == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolMismatch {
            expected: PROTOCOL_VERSION,
            actual,
        })
    }
}

/// A connection to a running daemon, past the hello handshake.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connects and consumes the hello, refusing a daemon whose protocol version does not match.
    pub async fn connect(socket_path: &Path) -> Result<Self, ClientError> {
        let mut stream = UnixStream::connect(socket_path)
            .await
            .map_err(WireError::Io)?;
        match wire::read_frame(&mut stream).await? {
            Some(ServerFrame::Hello { protocol_version }) => {
                verify_protocol(protocol_version).map_err(|mismatch| ClientError::Protocol {
                    expected: mismatch.expected,
                    actual: mismatch.actual,
                })?;
                Ok(Self { stream })
            }
            Some(_) => Err(ClientError::UnexpectedFrame),
            None => Err(ClientError::NoHello),
        }
    }

    /// Sends one request and returns its answer.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let frame = ClientFrame::Request {
            method: method.to_string(),
            params,
        };
        wire::write_frame(&mut self.stream, &frame).await?;
        match wire::read_frame(&mut self.stream).await? {
            Some(ServerFrame::Result { value }) => Ok(value),
            Some(ServerFrame::Error { message }) => Err(ClientError::Engine { message }),
            Some(ServerFrame::Hello { .. }) => Err(ClientError::UnexpectedFrame),
            None => Err(ClientError::ClosedEarly),
        }
    }

    /// Asks the daemon to shut down. The daemon tears down its warm session and removes its socket.
    pub async fn stop(mut self) -> Result<(), ClientError> {
        wire::write_frame(&mut self.stream, &ClientFrame::Stop).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matching_protocol_is_accepted_and_others_are_refused() {
        let observed = (
            verify_protocol(PROTOCOL_VERSION).is_ok(),
            verify_protocol(PROTOCOL_VERSION + 1),
        );
        assert_eq!(
            observed,
            (
                true,
                Err(ProtocolMismatch {
                    expected: PROTOCOL_VERSION,
                    actual: PROTOCOL_VERSION + 1,
                })
            )
        );
    }
}
