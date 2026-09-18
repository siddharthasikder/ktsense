//! Content-Length framing for LSP over stdio.
//!
//! The decoder accumulates raw bytes and yields one JSON body at a time. It is deliberately
//! transport-free so the awkward cases (a message split across reads, several messages in one read,
//! odd header casing, a truncated body) are unit-testable on byte slices without spawning a process.

use thiserror::Error;

const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";
const CONTENT_LENGTH: &str = "content-length";

/// Largest body a single frame may declare. 64 MiB is orders of magnitude above any real LSP message
/// (even a `references` reply spanning a large repository is a few MB of JSON locations), yet small
/// enough that a bogus or hostile `Content-Length` cannot drive an unbounded allocation.
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

/// Largest header block tolerated before its terminator arrives. Real LSP headers are a `Content-Length`
/// and an optional `Content-Type`, tens of bytes; 8 KiB is generous headroom while bounding the buffer
/// (and the repeated terminator scan) when a stream never sends `\r\n\r\n`.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// A framing fault that cannot be recovered by reading more bytes.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FramingError {
    #[error("frame header block carried no Content-Length")]
    MissingContentLength,
    #[error("Content-Length `{0}` is not a valid byte count")]
    InvalidContentLength(String),
    #[error("frame header block was not valid UTF-8")]
    InvalidHeaderText,
    #[error("Content-Length {length} exceeds the {limit} byte frame ceiling")]
    FrameTooLarge { length: usize, limit: usize },
    #[error("frame header grew past {limit} bytes without a terminator")]
    HeaderTooLarge { limit: usize },
}

/// Prepends the `Content-Length` header to an already-serialized JSON body.
pub(crate) fn encode(body: &[u8]) -> Vec<u8> {
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(body);
    framed
}

/// Incremental decoder that turns a byte stream into framed JSON bodies.
#[derive(Default)]
pub(crate) struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Returns the next complete body, `Ok(None)` when more bytes are still needed, or an error for
    /// a header block that can never become valid.
    pub(crate) fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FramingError> {
        let Some(separator) = find_subslice(&self.buffer, HEADER_TERMINATOR) else {
            if self.buffer.len() > MAX_HEADER_BYTES {
                return Err(FramingError::HeaderTooLarge {
                    limit: MAX_HEADER_BYTES,
                });
            }
            return Ok(None);
        };
        let header_text = std::str::from_utf8(&self.buffer[..separator])
            .map_err(|_| FramingError::InvalidHeaderText)?;
        let length = parse_content_length(header_text)?;
        let body_start = separator + HEADER_TERMINATOR.len();
        let frame_end = body_start
            .checked_add(length)
            .filter(|end| length <= MAX_CONTENT_LENGTH && *end <= isize::MAX as usize)
            .ok_or(FramingError::FrameTooLarge {
                length,
                limit: MAX_CONTENT_LENGTH,
            })?;
        if self.buffer.len() < frame_end {
            return Ok(None);
        }
        let body = self.buffer[body_start..frame_end].to_vec();
        self.buffer.drain(..frame_end);
        Ok(Some(body))
    }
}

fn parse_content_length(header_block: &str) -> Result<usize, FramingError> {
    for line in header_block.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(CONTENT_LENGTH) {
            let value = value.trim();
            return value
                .parse()
                .map_err(|_| FramingError::InvalidContentLength(value.to_string()));
        }
    }
    Err(FramingError::MissingContentLength)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_all(decoder: &mut FrameDecoder) -> Vec<Result<Option<Vec<u8>>, FramingError>> {
        let mut outcomes = Vec::new();
        loop {
            let outcome = decoder.next_frame();
            let stop = !matches!(outcome, Ok(Some(_)));
            outcomes.push(outcome);
            if stop {
                break;
            }
        }
        outcomes
    }

    fn bodies(decoder: &mut FrameDecoder) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(Some(body)) = decoder.next_frame() {
            out.push(String::from_utf8(body).unwrap());
        }
        out
    }

    #[test]
    fn reassembles_a_frame_split_across_reads() {
        let framed = encode(br#"{"jsonrpc":"2.0","id":1}"#);
        let (head, tail) = framed.split_at(10);
        let (mid, rest) = tail.split_at(6);

        let mut decoder = FrameDecoder::default();
        let mut seen = Vec::new();
        for chunk in [head, mid, rest] {
            decoder.push(chunk);
            seen.push(
                decoder
                    .next_frame()
                    .unwrap()
                    .map(|b| String::from_utf8(b).unwrap()),
            );
        }

        assert_eq!(
            seen,
            vec![None, None, Some(r#"{"jsonrpc":"2.0","id":1}"#.to_string())]
        );
    }

    #[test]
    fn splits_multiple_frames_from_one_read() {
        let mut buffer = encode(br#"{"id":1}"#);
        buffer.extend_from_slice(&encode(br#"{"id":2}"#));
        let mut decoder = FrameDecoder::default();
        decoder.push(&buffer);

        assert_eq!(bodies(&mut decoder), vec![r#"{"id":1}"#, r#"{"id":2}"#]);
    }

    #[test]
    fn tolerates_header_casing_and_extra_fields() {
        let body = br#"{"ok":true}"#;
        let mut framed = format!(
            "content-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
            body.len()
        )
        .into_bytes();
        framed.extend_from_slice(body);
        let mut decoder = FrameDecoder::default();
        decoder.push(&framed);

        assert_eq!(bodies(&mut decoder), vec![r#"{"ok":true}"#]);
    }

    #[test]
    fn withholds_a_truncated_body_until_the_rest_arrives() {
        let framed = encode(br#"{"id":7}"#);
        let split = framed.len() - 3;
        let mut decoder = FrameDecoder::default();
        decoder.push(&framed[..split]);
        let before = decoder.next_frame().unwrap();
        decoder.push(&framed[split..]);
        let after = decoder
            .next_frame()
            .unwrap()
            .map(|b| String::from_utf8(b).unwrap());

        assert_eq!((before, after), (None, Some(r#"{"id":7}"#.to_string())));
    }

    #[test]
    fn rejects_header_blocks_that_can_never_be_valid() {
        let cases = [
            "Content-Type: application/json\r\n\r\n",
            "Content-Length: not-a-number\r\n\r\n",
        ];
        let outcomes: Vec<_> = cases
            .into_iter()
            .map(|header| {
                let mut decoder = FrameDecoder::default();
                decoder.push(header.as_bytes());
                decoder.next_frame()
            })
            .collect();

        assert_eq!(
            outcomes,
            vec![
                Err(FramingError::MissingContentLength),
                Err(FramingError::InvalidContentLength(
                    "not-a-number".to_string()
                )),
            ]
        );
    }

    #[test]
    fn drains_to_empty_after_the_last_frame() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&encode(br#"{"id":1}"#));

        let outcomes = drain_all(&mut decoder);
        let tail = outcomes.last().cloned().unwrap();
        assert_eq!((outcomes.len(), tail), (2, Ok(None)));
    }

    #[test]
    fn rejects_frames_and_headers_that_breach_the_ceilings() {
        let above_ceiling = format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1);
        let overflowing = format!("Content-Length: {}\r\n\r\n", usize::MAX);

        let observed = [
            decode_once(above_ceiling.as_bytes()),
            decode_once(overflowing.as_bytes()),
            decode_once(&vec![b'x'; MAX_HEADER_BYTES + 1]),
        ];

        assert_eq!(
            observed,
            [
                Err(FramingError::FrameTooLarge {
                    length: MAX_CONTENT_LENGTH + 1,
                    limit: MAX_CONTENT_LENGTH,
                }),
                Err(FramingError::FrameTooLarge {
                    length: usize::MAX,
                    limit: MAX_CONTENT_LENGTH,
                }),
                Err(FramingError::HeaderTooLarge {
                    limit: MAX_HEADER_BYTES,
                }),
            ]
        );
    }

    fn decode_once(bytes: &[u8]) -> Result<Option<Vec<u8>>, FramingError> {
        let mut decoder = FrameDecoder::default();
        decoder.push(bytes);
        decoder.next_frame()
    }
}
