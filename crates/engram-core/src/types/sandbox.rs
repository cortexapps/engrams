use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use super::ids::SandboxId;
use super::image::NetworkPolicy;

/// Spec for creating a sandbox via `SandboxBackend::create`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Human-readable image identifier (e.g. `warm-2026-04-27`).
    /// Carried on snapshots for record-keeping; not used by the
    /// backend to locate the rootfs (that's `rootfs_source`).
    pub image: String,
    /// Resolved on-disk path to the image's rootfs. ProcessBackend
    /// materializes this directory into the sandbox cwd; future
    /// FirecrackerBackend points the root drive at it (or its
    /// `rootfs.ext4` sibling). `None` = empty workdir / fresh boot
    /// from base kernel; useful for bootstrap and tests.
    #[serde(default)]
    pub rootfs_source: Option<PathBuf>,
    /// Phase 5+: OCI registry URI for the bake image (e.g.
    /// `gcr.io/cortex/api:warm-X`). When set, the host-agent pulls
    /// it into its content-addressable cache and uses the cached
    /// rootfs.ext4 as `rootfs_source`. When `None`, the legacy
    /// `rootfs_source` path applies as-is — preserves single-host
    /// dev workflows that pre-bake images into `<local_path>/images/`.
    #[serde(default)]
    pub image_uri: Option<String>,
    // ADR 0021 P1.5b retired `harness_pack_uri` + `harness_substrate`.
    // The harness now lives in the image rootfs at the manifest-
    // declared `[harness] exec` path — there's no separate registry
    // URI to resolve and no per-session ext4 substrate to attach.
    pub cpu: CpuLimit,
    pub memory: MemoryLimit,
    pub disk: DiskLimit,
    /// Wall-clock TTL after which the host agent will force-stop the VM.
    pub ttl: Option<Duration>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    /// Per-sandbox egress policy derived from the image manifest's
    /// `[network]` block plus any session-time augmentation (e.g. a
    /// `WorkspaceSpec::Git` URL host gets auto-allowed so clone
    /// works). Backends with hard-isolation networking (FC) translate
    /// this into iptables rules; backends without (VZ's Apple NAT)
    /// log a warn-once when `default = Deny` and `allow_hosts` is
    /// non-empty.
    #[serde(default)]
    pub network: NetworkPolicy,
}

/// Argv + env for the long-running "agent" process (Claude Code,
/// the dev noop harness, future adapters). Passed to
/// `SandboxBackend::start_agent` at session-bind time — *not*
/// stored on `SandboxSpec`, because the agent's argv is per-session
/// (`session_id`, attach token, etc.) while `SandboxSpec` is a
/// per-image template.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Argv. `argv[0]` must be reachable by the backend — for
    /// ProcessBackend this means an absolute host path; for
    /// Firecracker it's a path inside the rootfs.
    pub argv: Vec<String>,
    /// Extra env on top of the sandbox-wide env. Used to inject
    /// the harness-hub address, session id, attach token, etc.
    /// without polluting the sandbox-wide env.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Per-host egress-proxy CA cert in PEM form (ADR 0021 P1).
    /// Populated by the host-agent *after* receiving the spec from
    /// coord, immediately before handing it to the sandbox backend —
    /// only the host knows its own CA. The Firecracker backend pushes
    /// this via `InstallHostCa` over vsock right after `wait_agent_ready`
    /// and before `SpawnHarness`, replacing the pre-0021 path where the
    /// CA rode in on the harness drive. `None` skips the install — used
    /// by tests, dev backends, and any deploy without egress proxying.
    ///
    /// **Wire format note**: this field intentionally does NOT carry
    /// `#[serde(skip_serializing_if = "Option::is_none")]`. AgentSpec
    /// crosses the coord ↔ host-agent gRPC boundary as bincode (see
    /// `engram-protocol/src/grpc_client.rs::start_agent`), and bincode
    /// is positional — skipping a field on encode breaks the decoder
    /// with "unexpected end of file" because it has no field names to
    /// look up. The `#[serde(default)]` covers the legacy-snapshot
    /// JSON path (manifest read of older sidecars that pre-date this
    /// field).
    #[serde(default)]
    pub host_ca_pem: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CpuLimit {
    pub vcpus: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MemoryLimit {
    pub max_mib: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DiskLimit {
    pub max_gib: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    pub timeout: Option<Duration>,
}

/// Output event from a streaming `exec`. The stream is terminated by
/// exactly one `Exit` event (or an error).
///
/// The variants are intentionally `Bytes` rather than `String` so a
/// process emitting non-UTF-8 output (binary tools, raw pipe content)
/// flows through unmolested. Callers stringify lossily at the API edge.
#[derive(Clone, Debug)]
pub enum ExecEvent {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(Option<i32>),
}

impl ExecEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Exit(_))
    }
}

/// Boxed event stream. Owned (`'static`) so it can be moved into a
/// background task or returned across an `axum` handler boundary.
pub type ExecEventStream = Pin<Box<dyn Stream<Item = ExecEvent> + Send + 'static>>;

/// Streaming counterpart to [`ExecHandle`]. The backend returns
/// immediately with metadata + a stream; the stream yields output as
/// the underlying process produces it and ends with a single `Exit`.
pub struct ExecStream {
    pub sandbox_id: SandboxId,
    pub exec_id: String,
    pub events: ExecEventStream,
}

impl std::fmt::Debug for ExecStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecStream")
            .field("sandbox_id", &self.sandbox_id)
            .field("exec_id", &self.exec_id)
            .field("events", &"<Stream<Item = ExecEvent>>")
            .finish()
    }
}

/// Buffered counterpart to [`ExecStream`]. Returned by the default
/// `SandboxBackend::exec` which drains the stream into in-memory
/// buffers — fine for short commands, never use it for long-running
/// agent processes (use `exec_stream` instead).
#[derive(Debug)]
pub struct ExecHandle {
    pub sandbox_id: SandboxId,
    pub exec_id: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: Option<i32>,
}

/// Resource accounting for a single exec, surfaced on `ExecCompleted`
/// events and the sync `/exec` response.
///
/// `wall_ms` is the time the exec was actually running, measured by
/// the coordinator. Cheap to capture, available on every platform.
///
/// `peak_rss_kb`, `user_cpu_ms`, `sys_cpu_ms` are reserved for
/// backend-supplied data (Firecracker's `GET /metrics`, Linux cgroups,
/// libvirt rusage, etc.). The dev `ProcessBackend` on macOS doesn't
/// have a clean way to capture them without unsafe FFI, so they're
/// `None` there. They land for real with the Phase 2 Firecracker
/// integration, which exposes per-VM CPU + memory metrics natively.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ExecRusage {
    pub wall_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_rss_kb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_cpu_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sys_cpu_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the bincode/serde footgun PR #40 ran into:
    /// `AgentSpec` crosses the coord ↔ host-agent gRPC boundary as
    /// bincode, and bincode is positional — any field marked
    /// `#[serde(skip_serializing_if = "Option::is_none")]` makes the
    /// encoder emit a shorter buffer when that field is `None`, and
    /// the decoder hits "unexpected end of file" when it tries to
    /// read the missing tag. The test pins the round-trip for both
    /// `None` and `Some` host_ca_pem so a future "let's clean up the
    /// JSON shape" change can't silently break the wire.
    #[test]
    fn agent_spec_bincode_roundtrips_with_none_and_some_host_ca_pem() {
        let none = AgentSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: HashMap::from_iter([("FOO".into(), "bar".into())]),
            host_ca_pem: None,
        };
        let bytes = bincode::serialize(&none).expect("bincode encode None");
        let back: AgentSpec = bincode::deserialize(&bytes).expect("bincode decode None");
        assert_eq!(back.argv, none.argv);
        assert_eq!(back.env, none.env);
        assert!(back.host_ca_pem.is_none());

        let some = AgentSpec {
            argv: vec!["/opt/engram/harness/harness".into()],
            env: HashMap::new(),
            host_ca_pem: Some(
                "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".into(),
            ),
        };
        let bytes = bincode::serialize(&some).expect("bincode encode Some");
        let back: AgentSpec = bincode::deserialize(&bytes).expect("bincode decode Some");
        assert_eq!(back.host_ca_pem, some.host_ca_pem);
    }

    #[test]
    fn exec_event_terminal_only_on_exit() {
        assert!(!ExecEvent::Stdout(Bytes::from_static(b"x")).is_terminal());
        assert!(!ExecEvent::Stderr(Bytes::from_static(b"x")).is_terminal());
        assert!(ExecEvent::Exit(Some(0)).is_terminal());
        assert!(ExecEvent::Exit(None).is_terminal());
    }
}
