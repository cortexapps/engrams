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

use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxId, SessionId};
use serde::{Deserialize, Serialize};

use crate::heartbeat::{Heartbeat, HeartbeatAck};

/// Bumped on every wire-incompatible change to the bincode frame
/// shape. The dialer ships this in its [`NotifyKind::Hello`] frame;
/// the coordinator rejects on mismatch. Bincode is positional and
/// schemaless, so a single mismatched int between coord and host
/// silently misaligns every subsequent byte — version-gating the
/// connection is the only safe way to roll mixed-version deploys.
pub const WIRE_VERSION: u32 = 1;

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
// Phase 5+: SandboxSpec gained image_uri / harness_pack_uri fields,
// nudging RequestKind::Create over the variant-size threshold. Boxing
// would change the wire shape (bincode size doesn't change but the
// lib API does). The trade-off doesn't matter for our throughput —
// Frame is sent over a WS once per RPC, not at exec-stream cadence.
#[allow(clippy::large_enum_variant)]
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
    Stream { req_id: u64, item: StreamItem },
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
    /// ADR 0007 Phase 6: backend chooses its own local staging dir
    /// via `snapshot_path_for(snapshot_id)`. Coord no longer
    /// dictates where on the host's filesystem snapshot artifacts
    /// land — chunks in BlobStorage are the cross-host durability
    /// primitive; local files are a per-host cache.
    Snapshot {
        sandbox_id: SandboxId,
    },
    /// ADR 0007 Phase 6: restore by metadata, not by host-local
    /// path. The backend reads its own staging dir for
    /// `metadata.id`; PooledBackend wraps to materialise
    /// `memory.bin` from chunks if the local file was reaped or
    /// the host is fresh.
    Restore {
        metadata: SnapshotMetadata,
    },
    /// Host → coord. The standalone host-agent doesn't have direct
    /// `MetadataStore` / KEK access, so it asks the coord to resolve
    /// OCI registry credentials on its behalf. Coord delegates to
    /// the existing `engram-oci-auth::PgAuthResolver`. Plaintext
    /// creds traverse the WS only at pull time; never persisted on
    /// the host. ADR 0007.
    ResolveRegistryAuth {
        host: String,
    },
    /// Coord → host. Per-host materialize-dir orphan reap. The
    /// admin endpoint runs the same primitive inline in
    /// `--mode=all`; in `--mode=coordinator` it fans this RPC out
    /// across `host_registry`'s connected hosts so each host
    /// sweeps its own local materialize_dir.
    ///
    /// The host-agent surfaces this via its `HostAdminHandler`,
    /// which calls `engram_host_agent::orphan_reap::reap_materialize_dir`
    /// against the host's own materialize_dir + a live-id set fetched
    /// from the coord (sent inline so the host doesn't need DB
    /// access to compute its own live set). ADR 0007.
    ReapMaterializeDir {
        min_age_secs: u64,
        /// Snapshot of `MetadataStore::list_live_disk_manifest_ids`
        /// taken by the coord just before fanout. Hosts don't have
        /// DB access; the coord owns the truth and ships it inline.
        /// Cap implicit: a UUID is 16 bytes; even 1M live manifests
        /// is 16 MiB on the wire — well under bincode's defaults.
        live_disk_manifest_ids: Vec<uuid::Uuid>,
    },
    /// Launch the long-running "agent" process inside an existing
    /// sandbox. Mirrors `HostClient::start_agent`. Frame ordering on
    /// a single WS connection guarantees this is processed after any
    /// preceding `NotifyKind::SessionEgressPolicy` for the same
    /// sandbox, so the host's egress proxy registry is live before
    /// the harness can dial out.
    StartAgent {
        sandbox_id: SandboxId,
        agent: AgentSpec,
    },
    /// Tell the host that an upcoming harness connection identifying
    /// itself with `session_id` should be routed to `sandbox_id`.
    /// `HostClient::bind_session`.
    BindHarnessSession {
        session_id: SessionId,
        sandbox_id: SandboxId,
    },
    /// Drop the session→sandbox binding on the host. `HostClient::unbind_session`.
    UnbindHarnessSession {
        session_id: SessionId,
    },
    /// Forward a user prompt to the harness attached to `sandbox_id`
    /// on this host. `HostClient::send_prompt`.
    SendHarnessPrompt {
        sandbox_id: SandboxId,
        text: String,
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
    /// `ResolveRegistryAuth` reply. `None` for anonymous /
    /// unknown-registry; `Some` for an entry the coord resolved
    /// via its `PgAuthResolver`. Caller plugs the creds into the
    /// OCI client's basic-auth path for the in-flight pull.
    RegistryAuth { creds: Option<RegistryCreds> },
    /// `ReapMaterializeDir` reply. Per-host outcome of the reap
    /// pass; the coord aggregates across hosts in the admin
    /// endpoint's JSON response.
    MaterializeDirReaped { stats: WireReapStats },
    /// Generic ack for `StartAgent`. The actual agent process is
    /// inside the guest; the coord only learns about its lifecycle
    /// through subsequent harness-channel frames.
    AgentStarted,
    /// Generic ack reused by `BindHarnessSession`,
    /// `UnbindHarnessSession`, and `SendHarnessPrompt`. The body is
    /// empty because all three operations either succeed or fail —
    /// failure rides the `RemoteError` path.
    HarnessOk,
}

/// Wire-side mirror of `engram_host_agent::orphan_reap::ReapStats`.
/// Defined here so `engram-protocol` doesn't drag a dep on the
/// host-agent crate (matches the `RegistryCreds` pattern).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WireReapStats {
    pub files_scanned: u64,
    pub files_deleted: u64,
    pub bytes_freed: u64,
    pub files_skipped_unparseable: u64,
    pub files_skipped_too_young: u64,
}

/// Plaintext credentials for an OCI registry, returned by the
/// coord in response to `ResolveRegistryAuth`. Mirrors
/// `engram_oci::auth::BasicCreds` but defined here so the wire
/// type doesn't drag a dep on the heavier OCI crate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryCreds {
    pub username: String,
    pub password: String,
}

/// In-flight streaming items for an `ExecStart` request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StreamItem {
    ExecStdout(Vec<u8>),
    ExecStderr(Vec<u8>),
    /// Terminal frame for an exec stream. The demuxer drops the stream
    /// channel after this.
    ExecExit {
        status: Option<i32>,
    },
}

/// Connection-level events that don't correspond to a request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NotifyKind {
    /// First frame the host sends after the WS upgrade. The coordinator
    /// uses `host_id` to route subsequent traffic and `wire_version`
    /// to reject mismatched binaries (mid-rollout build skew breaks
    /// the bincode wire silently otherwise).
    Hello {
        host_id: HostId,
        agent_version: String,
        wire_version: u32,
    },
    Heartbeat(Heartbeat),
    HeartbeatAck(HeartbeatAck),
    /// Coordinator → host. Per-session egress policy the host-agent's
    /// local egress proxy registers against the session's `guest_ip`.
    /// Sent after the sandbox is created (so `guest_ip` is known) and
    /// before `start_agent` is dispatched (so the harness can't make
    /// egress calls before policy is in place — WS-frame ordering
    /// enforces this). Payload type lives in `engram-core` so the
    /// `SandboxBackend` trait can name it without depending on this
    /// crate. ADR 0006.
    SessionEgressPolicy(SessionEgressPolicy),
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
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 2 },
            memory: MemoryLimit { max_mib: 1024 },
            disk: DiskLimit { max_gib: 10 },
            ttl: None,
            env: HashMap::from([("K".into(), "V".into())]),
            workdir: Some("/work".into()),
            harness_substrate: None,
            network: Default::default(),
            canonical_memory_manifest: None,
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
    fn session_egress_policy_round_trips() {
        use engram_core::types::egress::EgressSecretEntry;
        use engram_core::types::image::SecretMode;
        use engram_core::SessionId;
        let policy = SessionEgressPolicy {
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            guest_ip: "10.200.1.2".parse().unwrap(),
            network_allow_hosts: vec!["api.github.com".into()],
            network_allow_host_patterns: vec!["*.cortex.io".into()],
            secrets: vec![EgressSecretEntry {
                placeholder: "engram_ph_abc_def".into(),
                real_value: "ghp_real_secret".into(),
                allow_hosts: vec!["api.github.com".into()],
                allow_host_patterns: vec![],
            }],
            secret_mode: SecretMode::Broker,
        };
        let frame = Frame::Notify(NotifyKind::SessionEgressPolicy(policy.clone()));
        let bytes = bincode::serialize(&frame).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Notify(NotifyKind::SessionEgressPolicy(got)) => {
                assert_eq!(got.session_id, policy.session_id);
                assert_eq!(got.guest_ip, policy.guest_ip);
                assert_eq!(got.secrets.len(), 1);
                assert_eq!(got.secrets[0].placeholder, "engram_ph_abc_def");
                assert_eq!(got.secret_mode, SecretMode::Broker);
            }
            other => panic!("wrong shape: {other:?}"),
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
    fn reap_materialize_dir_round_trips_through_bincode() {
        // Sanity: the new v3 variant survives a Request → bytes →
        // Request cycle, including the live-set vector. Discriminant
        // alignment + Vec<uuid::Uuid> encoding both have caught real
        // wire breaks in earlier rollouts.
        let live_ids = vec![uuid::Uuid::new_v4(), uuid::Uuid::new_v4()];
        let kind = RequestKind::ReapMaterializeDir {
            min_age_secs: 600,
            live_disk_manifest_ids: live_ids.clone(),
        };
        let frame = Frame::Request {
            req_id: 7,
            trace: TraceContext::default(),
            kind,
        };
        let bytes = bincode::serialize(&frame).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Request {
                kind:
                    RequestKind::ReapMaterializeDir {
                        min_age_secs,
                        live_disk_manifest_ids,
                    },
                ..
            } => {
                assert_eq!(min_age_secs, 600);
                assert_eq!(live_disk_manifest_ids, live_ids);
            }
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn materialize_dir_reaped_response_round_trips() {
        // Response side mirror.
        let stats = WireReapStats {
            files_scanned: 12,
            files_deleted: 5,
            bytes_freed: 1024,
            files_skipped_unparseable: 1,
            files_skipped_too_young: 2,
        };
        let frame = Frame::Response {
            req_id: 7,
            result: Ok(ResponseKind::MaterializeDirReaped {
                stats: stats.clone(),
            }),
        };
        let bytes = bincode::serialize(&frame).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Response {
                result: Ok(ResponseKind::MaterializeDirReaped { stats: got }),
                ..
            } => {
                assert_eq!(got.files_scanned, 12);
                assert_eq!(got.files_deleted, 5);
                assert_eq!(got.bytes_freed, 1024);
                assert_eq!(got.files_skipped_unparseable, 1);
                assert_eq!(got.files_skipped_too_young, 2);
            }
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn start_agent_round_trips_argv_and_env() {
        // Sanity: a non-trivial AgentSpec (multi-word argv, multi-key
        // env including non-ASCII bytes) survives Request → bytes →
        // Request through bincode. Same shape as the
        // reap_materialize_dir test above; protects against discriminant
        // realignment when new variants land.
        let mut env = std::collections::HashMap::new();
        env.insert("MARKER".to_string(), "wire-round-trip ñ".to_string());
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        let agent = AgentSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env,
        };
        let sandbox_id = SandboxId::new();
        let kind = RequestKind::StartAgent {
            sandbox_id,
            agent: agent.clone(),
        };
        let frame = Frame::Request {
            req_id: 11,
            trace: TraceContext::default(),
            kind,
        };
        let bytes = bincode::serialize(&frame).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Request {
                kind:
                    RequestKind::StartAgent {
                        sandbox_id: got_id,
                        agent: got_agent,
                    },
                ..
            } => {
                assert_eq!(got_id, sandbox_id);
                assert_eq!(got_agent.argv, agent.argv);
                assert_eq!(got_agent.env, agent.env);
            }
            other => panic!("wrong shape: {other:?}"),
        }
    }

    #[test]
    fn agent_started_response_round_trips() {
        let frame = Frame::Response {
            req_id: 11,
            result: Ok(ResponseKind::AgentStarted),
        };
        let bytes = bincode::serialize(&frame).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back,
            Frame::Response {
                req_id: 11,
                result: Ok(ResponseKind::AgentStarted),
            }
        ));
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
            wire_version: WIRE_VERSION,
        });
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back {
            Frame::Notify(NotifyKind::Hello {
                host_id,
                agent_version,
                wire_version: _,
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
