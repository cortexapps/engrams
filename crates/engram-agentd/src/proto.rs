//! Wire protocol for host ↔ in-guest agent.
//!
//! Both ends are Rust and we own both schemas; the wire format is
//! `4-byte big-endian length` + bincode-encoded body. Bincode (1.x)
//! plugs into the existing serde derives, encodes `Vec<u8>` byte-for-
//! byte (vs JSON which inflates 3-5×), and is fast enough that the
//! agent doesn't burn CPU on encoding inside a 128 MiB guest VM.
//!
//! ## Conversation shape
//!
//! After the optional handshake (see below), the host sends one
//! [`WireRequest`] per connection. The verb's response shape
//! depends on the variant:
//!
//! - `WireRequest::Exec(req)` — agent streams [`WireExecEvent`]s
//!   ending with `Exit`. Same as the original exec-only protocol;
//!   the new envelope just wraps it.
//! - `WireRequest::Stat | Upload | Download | Ping | Shutdown` —
//!   agent sends exactly one [`WireResponse`] and closes.
//!
//! ### Without auth (development, default for back-compat):
//!
//! ```text
//!   host ──[ WireRequest::Exec(WireExecRequest) ]──► agent
//!   agent ──[ WireExecEvent::Stdout(bytes) ]──► host    (0+ times)
//!   agent ──[ WireExecEvent::Stderr(bytes) ]──► host    (0+ times)
//!   agent ──[ WireExecEvent::Exit(status)  ]──► host    (exactly once)
//!   <connection closed>
//! ```
//!
//! For non-streaming verbs:
//!
//! ```text
//!   host ──[ WireRequest::Stat { path } ]──► agent
//!   agent ──[ WireResponse::Stat(WireStatResponse) ]──► host
//!   <connection closed>
//! ```
//!
//! ### With first-frame token auth (production, when the agent is
//! started with `--token <T>` or finds `engram_token=<T>` on the
//! kernel cmdline):
//!
//! ```text
//!   host ──[ WireHandshake { token, agent_version } ]──► agent
//!   agent ──[ WireHandshakeAck { ok, message } ]──► host
//!     (if !ok, agent closes; host treats as auth failure)
//!   host ──[ WireRequest::* ]──► agent
//!     ... same as above ...
//! ```
//!
//! Ordering across stdout/stderr is best-effort: the agent fans out
//! both readers into a single mpsc channel. Within a single stream
//! ordering is preserved; across streams it isn't (which matches what
//! the kernel itself guarantees on tty interleaving anyway).

use std::collections::HashMap;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum size of one framed message (16 MiB). Caps adversarial
/// inputs and accidental stdout-bomb bugs without being so tight that
/// a real `cargo build` log line truncates.
pub const MAX_MSG_BYTES: usize = 16 * 1024 * 1024;

/// One exec request, wire-encoded as the *first* frame the host sends
/// after connecting to the agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireExecRequest {
    /// argv. `command[0]` is the program; never invokes a shell.
    pub command: Vec<String>,
    /// Optional bytes piped to the child's stdin before EOF. Use
    /// `Some(Vec::new())` to close stdin immediately; `None` to leave
    /// stdin connected to /dev/null.
    pub stdin: Option<Vec<u8>>,
    /// Env vars layered onto the child's environment (added, not
    /// replaced). The agent inherits its own env first.
    pub env: HashMap<String, String>,
    /// `current_dir` for the child. `None` keeps the agent's cwd.
    pub workdir: Option<String>,
    /// Wall-clock timeout. After this, the agent SIGKILLs the child
    /// and emits `Exit(None)`. `None` disables the timeout.
    pub timeout_ms: Option<u64>,
}

/// Each event the agent emits during exec. The stream is terminated by
/// exactly one `Exit` (or an abrupt connection close on agent crash,
/// which the host treats as a failure).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// `None` = signalled / timed out. `Some(0)` = clean exit 0.
    Exit(Option<i32>),
}

/// First frame the host sends when auth is enabled. The agent
/// validates `token` against the value it loaded at startup
/// (`--token <T>` CLI arg or `engram_token=<T>` on the kernel
/// cmdline) and replies with [`WireHandshakeAck`]. `agent_version`
/// is purely informational — the agent logs it so a deployment-
/// version skew between host and guest is visible in agent logs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireHandshake {
    pub token: String,
    pub agent_version: String,
}

/// Agent's response to [`WireHandshake`]. On `ok = true` the agent
/// continues to read the [`WireRequest`] frame as in the no-auth
/// flow. On `ok = false` the agent closes the connection after
/// sending the ack — `message` carries a single short reason the
/// host can surface in logs (NEVER include the expected token or
/// any guess at the supplied one — the message is plaintext on
/// the vsock, not a secrets channel).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireHandshakeAck {
    pub ok: bool,
    pub message: Option<String>,
}

/// Multi-verb request envelope. The host sends exactly one of these
/// per connection after the (optional) handshake; the agent
/// dispatches and either streams (Exec) or sends a single
/// [`WireResponse`] (everything else). Single-frame size cap is
/// [`MAX_MSG_BYTES`] (16 MiB) per the existing framing layer; for
/// Upload / Download that bounds payload size at 16 MiB. Multi-
/// frame chunked uploads are a follow-up.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireRequest {
    /// Run a command in the sandbox and stream its output. Wraps
    /// the original [`WireExecRequest`] one-to-one — back-compat
    /// preserved by callers updating to `WireRequest::Exec(req)`.
    Exec(WireExecRequest),
    /// Read filesystem metadata for `path` inside the sandbox.
    /// Returns [`WireResponse::Stat`] always — `exists = false`
    /// instead of an error if the path is missing, to keep
    /// "does this exist" cheap to ask.
    Stat { path: String },
    /// Write `bytes` to `path` inside the sandbox, creating parent
    /// directories as needed. `mode` is the unix file mode to
    /// `chmod` to after the write (mostly for `+x` on uploaded
    /// scripts); `None` keeps the OS default.
    Upload {
        path: String,
        bytes: Vec<u8>,
        mode: Option<u32>,
    },
    /// Read the entire contents of `path` from the sandbox into
    /// the response. Errors with `WireResponse::Error` if the
    /// file doesn't exist or exceeds the framing cap.
    Download { path: String },
    /// Liveness probe. Agent replies [`WireResponse::Pong`].
    Ping,
    /// Ask the agent to exit cleanly after replying. The agent
    /// sends [`WireResponse::ShutdownAck`] then closes; main.rs's
    /// accept loop notices the agent task ended and exits the
    /// process. Used for graceful VM shutdown coordination.
    Shutdown,
}

/// Single-shot response for non-streaming [`WireRequest`] verbs.
/// `Exec` doesn't get one — its response is the stream of
/// [`WireExecEvent`]s.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireResponse {
    Stat(WireStatResponse),
    /// Successful upload. No payload — the host knows what it
    /// sent.
    UploadOk,
    Download(WireDownloadResponse),
    Pong,
    ShutdownAck,
    /// Anything the agent couldn't fulfil. `message` is a short
    /// human-readable reason; `kind` mirrors the std `io::ErrorKind`
    /// stringly so the host can map back to a typed error
    /// without a wire-format tied to the unstable enum.
    Error {
        kind: String,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireStatResponse {
    pub exists: bool,
    pub size: u64,
    /// Modification time in seconds since the Unix epoch, or 0
    /// when the platform doesn't expose one. Useful for "did this
    /// file change since I last looked".
    pub mtime_unix: i64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireDownloadResponse {
    pub bytes: Vec<u8>,
}

// ---- Framing -----------------------------------------------------------

/// Read one length-prefixed frame off `r` and bincode-decode it.
pub async fn read_msg<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: AsyncReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}")))
}

/// Bincode-encode `msg` and write it as a length-prefixed frame.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}"))
    })?;
    if body.len() > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "encoded frame {} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})",
                body.len()
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn req() -> WireExecRequest {
        WireExecRequest {
            command: vec!["echo".into(), "hello".into()],
            stdin: None,
            env: HashMap::from([("RUST_LOG".into(), "info".into())]),
            workdir: Some("/tmp".into()),
            timeout_ms: Some(5_000),
        }
    }

    #[tokio::test]
    async fn write_then_read_round_trips_request() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &req()).await.unwrap();
        let mut cur = Cursor::new(buf);
        let got: WireExecRequest = read_msg(&mut cur).await.unwrap();
        assert_eq!(got, req());
    }

    #[tokio::test]
    async fn write_then_read_round_trips_each_event_variant() {
        for ev in [
            WireExecEvent::Stdout(b"hello\n".to_vec()),
            WireExecEvent::Stderr(vec![0xff, 0x00, 0xff]),
            WireExecEvent::Exit(Some(0)),
            WireExecEvent::Exit(Some(137)),
            WireExecEvent::Exit(None),
        ] {
            let mut buf = Vec::new();
            write_msg(&mut buf, &ev).await.unwrap();
            let mut cur = Cursor::new(buf);
            let got: WireExecEvent = read_msg(&mut cur).await.unwrap();
            assert_eq!(got, ev);
        }
    }

    #[tokio::test]
    async fn read_truncated_length_prefix_errors() {
        let mut cur = Cursor::new(vec![0u8; 2]); // < 4 bytes
        let err = read_msg::<_, WireExecEvent>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_rejects_oversized_length() {
        // Forge a length prefix of MAX+1 with no body. Catches a
        // malicious / corrupt peer before we vec![0; HUGE].
        let bad_len = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let mut cur = Cursor::new(bad_len.to_vec());
        let err = read_msg::<_, WireExecEvent>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    }

    #[tokio::test]
    async fn read_rejects_garbage_body() {
        // 4-byte length says "1 byte" but it's not valid bincode for a
        // WireExecEvent. Should error InvalidData rather than panic.
        let mut buf = vec![0, 0, 0, 1, 0xff];
        let mut cur = Cursor::new(std::mem::take(&mut buf));
        let err = read_msg::<_, WireExecEvent>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn binary_stdout_round_trips_byte_for_byte() {
        // The whole point of switching from JSON to bincode: a non-UTF-8
        // stdout chunk survives unmolested.
        let chunk: Vec<u8> = (0..=255u8).cycle().take(8 * 1024).collect();
        let ev = WireExecEvent::Stdout(chunk.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &ev).await.unwrap();
        let mut cur = Cursor::new(buf);
        let WireExecEvent::Stdout(got) = read_msg(&mut cur).await.unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(got, chunk);
    }
}
