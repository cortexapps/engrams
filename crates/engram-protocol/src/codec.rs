//! Bincode <-> WebSocket message conversion for [`Frame`] values.
//!
//! WebSockets handle their own message framing, so we don't need a
//! length prefix on the wire — each `Frame` is one binary message.
//! The 16 MiB cap mirrors `engram-agentd`'s `MAX_MSG_BYTES` so a
//! pathological agent can't blow up the coordinator with a single
//! oversized exec output frame.

use crate::wire::Frame;
use tokio_tungstenite::tungstenite::Message;

/// Maximum encoded size of a single frame (16 MiB). Caps adversarial
/// inputs and accidental stdout-bomb bugs without being so tight that
/// a real exec stdout chunk truncates.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum CodecError {
    Encode(String),
    /// Encoded frame exceeded `MAX_FRAME_BYTES`. Caller should treat
    /// this as a protocol violation by the local side.
    TooLarge {
        encoded: usize,
        limit: usize,
    },
    /// The decoded WS message wasn't a Binary message we could handle
    /// (e.g. a Ping or Text frame).
    UnexpectedMessage(&'static str),
    Decode(String),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode(m) => write!(f, "frame encode failed: {m}"),
            Self::TooLarge { encoded, limit } => {
                write!(f, "encoded frame {encoded} bytes exceeds limit {limit}")
            }
            Self::UnexpectedMessage(k) => write!(f, "unexpected ws message: {k}"),
            Self::Decode(m) => write!(f, "frame decode failed: {m}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Bincode-encode `frame` and wrap it in a binary WebSocket message.
pub fn encode(frame: &Frame) -> Result<Message, CodecError> {
    let bytes = bincode::serialize(frame).map_err(|e| CodecError::Encode(e.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge {
            encoded: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    Ok(Message::Binary(bytes))
}

/// Decode a WebSocket message into a [`Frame`]. Ping/Pong/Close are
/// surfaced as `UnexpectedMessage` — the caller (the read loop) handles
/// them at a layer above this codec since they aren't frames.
pub fn decode(msg: Message) -> Result<Frame, CodecError> {
    match msg {
        Message::Binary(bytes) => {
            if bytes.len() > MAX_FRAME_BYTES {
                return Err(CodecError::TooLarge {
                    encoded: bytes.len(),
                    limit: MAX_FRAME_BYTES,
                });
            }
            bincode::deserialize(&bytes).map_err(|e| CodecError::Decode(e.to_string()))
        }
        Message::Text(_) => Err(CodecError::UnexpectedMessage("text")),
        Message::Ping(_) => Err(CodecError::UnexpectedMessage("ping")),
        Message::Pong(_) => Err(CodecError::UnexpectedMessage("pong")),
        Message::Close(_) => Err(CodecError::UnexpectedMessage("close")),
        Message::Frame(_) => Err(CodecError::UnexpectedMessage("raw_frame")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{NotifyKind, RequestKind, StreamItem};
    use engram_core::{HostId, SandboxId};

    #[test]
    fn encode_decode_round_trips_request() {
        let f = Frame::Request {
            req_id: 99,
            trace: crate::wire::TraceContext::random(),
            kind: RequestKind::DestroySandbox {
                sandbox_id: SandboxId::new(),
            },
        };
        let msg = encode(&f).unwrap();
        let back = decode(msg).unwrap();
        match back {
            Frame::Request { req_id, .. } => assert_eq!(req_id, 99),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn encode_decode_round_trips_stream_with_binary_payload() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let f = Frame::Stream {
            req_id: 4,
            item: StreamItem::ExecStdout(bytes.clone()),
        };
        let msg = encode(&f).unwrap();
        let back = decode(msg).unwrap();
        match back {
            Frame::Stream {
                item: StreamItem::ExecStdout(got),
                ..
            } => assert_eq!(got, bytes),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn encode_decode_round_trips_notify() {
        let f = Frame::Notify(NotifyKind::Hello {
            host_id: HostId::new(),
            agent_version: "x".into(),
        });
        let msg = encode(&f).unwrap();
        match decode(msg).unwrap() {
            Frame::Notify(NotifyKind::Hello { agent_version, .. }) => {
                assert_eq!(agent_version, "x")
            }
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn decode_text_message_is_unexpected() {
        let err = decode(Message::Text("hello".into())).unwrap_err();
        assert!(matches!(err, CodecError::UnexpectedMessage("text")));
    }

    #[test]
    fn decode_close_message_is_unexpected() {
        let err = decode(Message::Close(None)).unwrap_err();
        assert!(matches!(err, CodecError::UnexpectedMessage("close")));
    }
}
