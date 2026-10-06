//! Length-prefixed JSON framing for the daemon's Unix-socket protocol.
//!
//! Each frame is a 4-byte big-endian length followed by that many bytes of JSON. This is the
//! daemon's own transport and is distinct from the Content-Length framing [`crate`] uses to drive
//! the upstream engine; keeping them separate means the daemon protocol can evolve without touching
//! the engine codec. The reader is generic over any async byte stream so the socket and the tests
//! exercise the same path.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame the daemon will read or write. A daemon answer is a compressed skeleton or a list
/// of locations, kilobytes at most; 16 MiB is generous headroom while bounding the allocation a
/// hostile or corrupt length prefix can request.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// A frame the daemon sends to a connected client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Sent first on every connection so the client can detect a stale or mismatched daemon. This
    /// frame is version-independent: every daemon, of any protocol version, announces its version
    /// through it, so a client can read the version of a daemon it does not match. Its wire bytes
    /// are pinned (see the wire test) and must stay decodable forever.
    Hello { protocol_version: u32 },
    /// A successful answer to a request.
    Result { value: Value },
    /// A request that failed; the connection stays open unless the daemon is also stopping.
    Error { message: String },
}

/// A frame a client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Request {
        method: String,
        params: Value,
    },
    /// Asks the daemon to shut down. This frame is version-independent and its wire bytes are pinned
    /// (see the wire test): every protocol version has accepted `{"type":"stop"}` and every future
    /// one must, so `daemon stop` can stop a daemon whose protocol version it does not match. The
    /// deployed protocol-7 build was measured to accept exactly these bytes (KT-113).
    Stop,
}

/// A framing or encoding fault on the daemon transport.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("i/o error on the daemon socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not encode/decode a daemon frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame length {length} exceeds the {limit} byte ceiling")]
    TooLarge { length: usize, limit: usize },
}

/// Reads one frame, or `None` when the peer closed the stream cleanly before the next frame began.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, WireError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length_bytes = [0u8; 4];
    match reader.read_exact(&mut length_bytes).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length > MAX_FRAME {
        return Err(WireError::TooLarge {
            length,
            limit: MAX_FRAME,
        });
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

/// Serializes and writes one frame, flushing so the peer sees it without waiting for more traffic.
pub async fn write_frame<W, T>(writer: &mut W, frame: &T) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(frame)?;
    if body.len() > MAX_FRAME {
        return Err(WireError::TooLarge {
            length: body.len(),
            limit: MAX_FRAME,
        });
    }
    let length = body.len() as u32;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn a_written_frame_reads_back_unchanged() {
        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            &ServerFrame::Hello {
                protocol_version: 7,
            },
        )
        .await
        .unwrap();
        let mut cursor = Cursor::new(buffer);
        let decoded: Option<ServerFrame> = read_frame(&mut cursor).await.unwrap();
        let trailing: Option<ServerFrame> = read_frame(&mut cursor).await.unwrap();

        assert_eq!(
            (decoded, trailing),
            (
                Some(ServerFrame::Hello {
                    protocol_version: 7
                }),
                None
            )
        );
    }

    #[tokio::test]
    async fn an_oversized_length_prefix_is_refused_without_allocating() {
        let mut framed = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(b"ignored");
        let mut cursor = Cursor::new(framed);
        let outcome: Result<Option<ClientFrame>, WireError> = read_frame(&mut cursor).await;

        assert!(matches!(
            outcome,
            Err(WireError::TooLarge {
                length,
                limit
            }) if length == MAX_FRAME + 1 && limit == MAX_FRAME
        ));
    }

    /// The two frames `daemon stop` and `daemon status` rely on across a protocol mismatch are
    /// pinned to their exact wire bytes. If serde renaming, field order or the framing ever changes
    /// these, a current build can no longer stop or read the version of an already-deployed daemon,
    /// so this fails rather than letting that regress silently. The protocol-7 build was measured to
    /// emit this hello and accept this stop (KT-113).
    #[tokio::test]
    async fn the_cross_version_frames_are_pinned_to_their_wire_bytes() {
        let frame = |body: &[u8]| {
            let mut out = (body.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(body);
            out
        };
        let mut stop = Vec::new();
        write_frame(&mut stop, &ClientFrame::Stop).await.unwrap();
        let mut hello = Vec::new();
        write_frame(
            &mut hello,
            &ServerFrame::Hello {
                protocol_version: 7,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            (stop, hello),
            (
                frame(br#"{"type":"stop"}"#),
                frame(br#"{"type":"hello","protocol_version":7}"#),
            )
        );
    }
}
