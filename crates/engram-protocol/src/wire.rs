//! Wire types for the coordinator ↔ host-agent WebSocket channel.
//!
//! The transport is bincode-encoded `Frame` values inside WebSocket
//! Binary messages (the WS framing handles message boundaries; we don't
//! need a length prefix). Every Request carries a `req_id`; Responses
//! and Stream items carry the same id so the client-side demuxer can
//! match them. Notifies (heartbeat, hello, etc.) have no `req_id`.
//!
//! This module deliberately avoids using `bytes::Bytes` and trait objects
//! — every payload is plain serde-derived data so bincode can round-trip
//! it without extra machinery.

use std::collections::HashMap;
use std::time::Duration;

use engram_core::types::sandbox::SandboxSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxId};
use serde::{Deserialize, Serialize};

use crate::heartbeat::{Heartbeat, HeartbeatAck};

/// Top-level frame on the wire.
///
/// Request/Response frames carry a `req_id` allocated by the side that
/// originated the call (currently always coordinator → host). Stream
/// frames also carry the originating `req_id`. Notifies are unrelated
/// to any specific request.
///
/// Request frames also carry a [`TraceContext`] (W3C-style 16-byte
/// trace id + 8-byte span id) so the host's `tracing` span for the
/// dispatched RPC can include the same ids the coordinator's
/// originating span did. With a future tracing-opentelemetry collector
/// these stitch into real distributed traces; today they at least
/// give `grep` something common across coord + host logs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Frame {
    Request {
        req_id: u64,
        trace: TraceContext,
        kind: RequestKind,
    },
    Response {
        req_id: u64,
        result: Result<ResponseKind, RemoteError>,
    },
    /// Streaming output for an in-flight Request. Concretely: ExecStream
    /// items. Terminated by exactly one `StreamItem::ExecExit`.
    Stream {
        req_id: u64,
        item: StreamItem,
    },
    /// Heartbeat / hello / push notifications that aren't tied to a
    /// specific outstanding request.
    Notify(NotifyKind),
}

/// W3C-shaped trace context — 16-byte trace id + 8-byte span id. The
/// coordinator generates fresh ids per outgoing Request (proper
/// propagation from a parent span lands when the workspace adopts
/// `tracing-opentelemetry`). Hosts attach the values as fields on the
/// span they create around `handle_request` so per-RPC log lines
/// across both sides share `trace_id` / `span_id`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TraceContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
}

impl TraceContext {
    /// Generate a fresh, random context. Cryptographic-quality
    /// randomness isn't required here — these are debug correlation
    /// ids, not security tokens — but `uuid::Uuid::new_v4` is what
    /// the workspace already pulls in for sandbox/session ids and
    /// gives 16 bytes of v4 randomness for free.
    pub fn random() -> Self {
        let trace_id = *uuid::Uuid::new_v4().as_bytes();
        let mut span_id = [0u8; 8];
        // Use the low half of a fresh v4 UUID for the span id —
        // independent randomness keeps the span id from being
        // trivially derivable from the trace id.
        span_id.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..8]);
        Self { trace_id, span_id }
    }

    /// Hex-format the trace id for log fields. Matches the W3C
    /// traceparent header style (lowercase, no separators).
    pub fn trace_id_hex(&self) -> String {
        hex_lower(&self.trace_id)
    }

    pub fn span_id_hex(&self) -> String {
        hex_lower(&self.span_id)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Coordinator → host requests. Mirrors the [`SandboxBackend`] trait
/// surface so a thin `RemoteSandboxBackend` can wrap one of these per
/// trait method.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RequestKind {
    CreateSandbox {
        spec: SandboxSpec,
    },
    DestroySandbox {
        sandbox_id: SandboxId,
    },
    ListSandboxes,
    /// Start an exec on the given sandbox. The host replies with
    /// `ResponseKind::ExecStarted` carrying the assigned `exec_id`,
    /// then streams `StreamItem::ExecStdout/Stderr/Exit` frames against
    /// the same `req_id`.
    ExecStart {
        sandbox_id: SandboxId,
        request: WireExecRequest,
    },
    Snapshot {
        sandbox_id: SandboxId,
        /// Path on the host's filesystem where the snapshot should
        /// land. Phase 3a single-host (or `--mode=all`) means coord and
        /// host share a filesystem; multi-host work in 3b refines this
        /// into a host-relative scheme.
        dest_path: String,
    },
    Restore {
        src_path: String,
    },
}

/// Successful response payloads. Errors take the [`RemoteError`] path
/// and never appear here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResponseKind {
    /// `CreateSandbox` reply.
    SandboxCreated { sandbox_id: SandboxId },
    /// Generic ack for `DestroySandbox`.
    Destroyed,
    /// `ListSandboxes` reply.
    Sandboxes { ids: Vec<SandboxId> },
    /// `ExecStart` reply — the stream of stdout/stderr/exit follows on
    /// `Frame::Stream` with the same `req_id`.
    ExecStarted { exec_id: String },
    /// `Snapshot` reply.
    Snapshotted { metadata: SnapshotMetadata },
    /// `Restore` reply.
    Restored { sandbox_id: SandboxId },
}

/// In-flight streaming items for an `ExecStart` request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StreamItem {
    ExecStdout(Vec<u8>),
    ExecStderr(Vec<u8>),
    /// Terminal frame for an exec stream. The demuxer drops the stream
    /// channel after this.
    ExecExit { status: Option<i32> },
}

/// Connection-level events that don't correspond to a request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NotifyKind {
    /// First frame the host sends after the WS upgrade. The coordinator
    /// uses `host_id` to route subsequent traffic.
    Hello {
        host_id: HostId,
        agent_version: String,
    },
    Heartbeat(Heartbeat),
    HeartbeatAck(HeartbeatAck),
}

/// Wire-friendly mirror of [`engram_core::types::sandbox::ExecRequest`].
/// We re-define rather than reuse so `Duration` round-trips cleanly via
/// bincode (the source type's serde derives use the duration's default
/// representation, which is fine, but pinning the wire shape means a
/// future change to the in-process type doesn't silently change the
/// wire format).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireExecRequest {
    pub command: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    /// Wall-clock timeout in milliseconds. `None` disables.
    pub timeout_ms: Option<u64>,
}

impl WireExecRequest {
    pub fn from_engine(req: engram_core::types::sandbox::ExecRequest) -> Self {
        Self {
            command: req.command,
            stdin: req.stdin,
            env: req.env,
            workdir: req.workdir,
            timeout_ms: req.timeout.map(|d| d.as_millis() as u64),
        }
    }

    pub fn into_engine(self) -> engram_core::types::sandbox::ExecRequest {
        engram_core::types::sandbox::ExecRequest {
            command: self.command,
            stdin: self.stdin,
            env: self.env,
            workdir: self.workdir,
            timeout: self.timeout_ms.map(Duration::from_millis),
        }
    }
}

/// Wire-friendly mirror of [`engram_core::SandboxError`]. The original
/// carries a `BoxError` source which can't serialize; here we collapse
/// it to a string. The discriminant survives so callers can match.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RemoteError {
    NotFound,
    AlreadyExists,
    LimitExceeded(String),
    Vm(String),
    InvalidSpec(String),
    Snapshot(String),
    Io(String),
    Timeout,
    /// Anything that doesn't fit the variants above — typically a host-
    /// agent-internal error not modelled in `SandboxError` (e.g. WS
    /// disconnect mid-call).
    Other(String),
}

impl RemoteError {
    pub fn from_sandbox(err: engram_core::SandboxError) -> Self {
        use engram_core::SandboxError as S;
        match err {
            S::NotFound => Self::NotFound,
            S::AlreadyExists => Self::AlreadyExists,
            S::LimitExceeded(m) => Self::LimitExceeded(m),
            S::Vm(e) => Self::Vm(e.to_string()),
            S::InvalidSpec(m) => Self::InvalidSpec(m),
            S::Snapshot(m) => Self::Snapshot(m),
            S::Io(e) => Self::Io(e.to_string()),
            S::Timeout => Self::Timeout,
        }
    }

    pub fn into_sandbox(self) -> engram_core::SandboxError {
        use engram_core::SandboxError as S;
        match self {
            Self::NotFound => S::NotFound,
            Self::AlreadyExists => S::AlreadyExists,
            Self::LimitExceeded(m) => S::LimitExceeded(m),
            Self::Vm(m) => S::Vm(Box::new(StringError(m))),
            Self::InvalidSpec(m) => S::InvalidSpec(m),
            Self::Snapshot(m) => S::Snapshot(m),
            Self::Io(m) => S::Io(std::io::Error::other(m)),
            Self::Timeout => S::Timeout,
            Self::Other(m) => S::Vm(Box::new(StringError(m))),
        }
    }
}

#[derive(Debug)]
struct StringError(String);

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StringError {}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecEvent, MemoryLimit};

    #[test]
    fn frame_request_round_trips_via_bincode() {
        let trace = TraceContext::random();
        let f = Frame::Request {
            req_id: 42,
            trace,
            kind: RequestKind::DestroySandbox {
                sandbox_id: SandboxId::new(),
            },
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Request {
                req_id,
                trace: trace_back,
                kind: RequestKind::DestroySandbox { sandbox_id },
            } => {
                assert_eq!(req_id, 42);
                assert_eq!(trace_back.trace_id, trace.trace_id);
                assert_eq!(trace_back.span_id, trace.span_id);
                let _ = sandbox_id;
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn create_sandbox_round_trips_with_full_spec() {
        let spec = SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            cpu: CpuLimit { vcpus: 2 },
            memory: MemoryLimit { max_mib: 1024 },
            disk: DiskLimit { max_gib: 10 },
            ttl: None,
            env: HashMap::from([("K".into(), "V".into())]),
            workdir: Some("/work".into()),
        };
        let f = Frame::Request {
            req_id: 1,
            trace: TraceContext::random(),
            kind: RequestKind::CreateSandbox { spec: spec.clone() },
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Request {
                kind: RequestKind::CreateSandbox { spec: got },
                ..
            } => {
                assert_eq!(got.image, spec.image);
                assert_eq!(got.cpu.vcpus, 2);
                assert_eq!(got.memory.max_mib, 1024);
                assert_eq!(got.env.get("K").map(String::as_str), Some("V"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn response_carries_remote_error_round_trip() {
        let f = Frame::Response {
            req_id: 7,
            result: Err(RemoteError::NotFound),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Response {
                req_id,
                result: Err(RemoteError::NotFound),
            } => assert_eq!(req_id, 7),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn stream_item_round_trips_binary_chunks() {
        // Non-UTF-8 stdout must survive bincode -> Vec<u8> -> bincode.
        let chunk: Vec<u8> = (0..=255u8).collect();
        let item = Frame::Stream {
            req_id: 12,
            item: StreamItem::ExecStdout(chunk.clone()),
        };
        let bytes = bincode::serialize(&item).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Stream {
                item: StreamItem::ExecStdout(got),
                ..
            } => assert_eq!(got, chunk),
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn notify_hello_carries_host_id() {
        let host = HostId::new();
        let f = Frame::Notify(NotifyKind::Hello {
            host_id: host,
            agent_version: "0.1.0".into(),
        });
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Notify(NotifyKind::Hello {
                host_id,
                agent_version,
            }) => {
                assert_eq!(host_id, host);
                assert_eq!(agent_version, "0.1.0");
            }
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn wire_exec_request_round_trips_through_engine_type() {
        let original = engram_core::types::sandbox::ExecRequest {
            command: vec!["echo".into(), "hi".into()],
            stdin: Some(b"input".to_vec()),
            env: HashMap::from([("A".into(), "B".into())]),
            workdir: Some("/tmp".into()),
            timeout: Some(Duration::from_millis(5_500)),
        };
        let wire = WireExecRequest::from_engine(original.clone());
        let bytes = bincode::serialize(&wire).unwrap();
        let back: WireExecRequest = bincode::deserialize(&bytes).unwrap();
        let recovered = back.into_engine();
        assert_eq!(recovered.command, original.command);
        assert_eq!(recovered.stdin, original.stdin);
        assert_eq!(recovered.env, original.env);
        assert_eq!(recovered.workdir, original.workdir);
        assert_eq!(recovered.timeout, original.timeout);
    }

    #[test]
    fn remote_error_round_trips_through_sandbox_error() {
        let cases = [
            engram_core::SandboxError::NotFound,
            engram_core::SandboxError::AlreadyExists,
            engram_core::SandboxError::Timeout,
            engram_core::SandboxError::InvalidSpec("bad".into()),
            engram_core::SandboxError::Snapshot("fail".into()),
            engram_core::SandboxError::LimitExceeded("cpu".into()),
        ];
        for original in cases {
            let original_msg = original.to_string();
            let wire = RemoteError::from_sandbox(original);
            let bytes = bincode::serialize(&wire).unwrap();
            let back: RemoteError = bincode::deserialize(&bytes).unwrap();
            let recovered = back.into_sandbox();
            assert_eq!(
                recovered.to_string(),
                original_msg,
                "Display should round-trip"
            );
        }
    }

    #[test]
    fn exec_event_terminal_only_on_exit() {
        // Quick reminder this still holds — used by the demuxer to know
        // when to drop the per-stream channel.
        assert!(!ExecEvent::Stdout(Default::default()).is_terminal());
        assert!(ExecEvent::Exit(Some(0)).is_terminal());
    }
}
