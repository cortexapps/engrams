use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use super::ids::SandboxId;

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
    pub cpu: CpuLimit,
    pub memory: MemoryLimit,
    pub disk: DiskLimit,
    /// Wall-clock TTL after which the host agent will force-stop the VM.
    pub ttl: Option<Duration>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    /// Long-running agent process to launch alongside the sandbox —
    /// the harness adapter (Claude Code wrapper, noop dev harness,
    /// etc.). `None` = quiescent sandbox that only runs explicit
    /// `exec()`s; useful for bootstrap and tests.
    ///
    /// `ProcessBackend` spawns this as a child rooted in the
    /// sandbox cwd at `create()` and SIGTERMs it at `destroy()`.
    /// Firecracker reads it via `engram-bootstrap` from a file
    /// dropped into the rootfs (Phase 5+); for now, Firecracker
    /// errors if `agent.is_some()` rather than silently ignoring.
    #[serde(default)]
    pub agent: Option<AgentSpec>,
}

/// Argv + env for the long-running "agent" process (Claude Code,
/// the dev noop harness, future adapters). Backends launch this at
/// `create()` and tear it down at `destroy()`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Argv. `argv[0]` must be reachable by the backend — for
    /// ProcessBackend this means an absolute host path; for
    /// Firecracker it's a path inside the rootfs.
    pub argv: Vec<String>,
    /// Extra env on top of `SandboxSpec::env`. Used to inject the
    /// harness-hub address, session id, attach token, etc. without
    /// polluting the sandbox-wide env.
    #[serde(default)]
    pub env: HashMap<String, String>,
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

    #[test]
    fn exec_event_terminal_only_on_exit() {
        assert!(!ExecEvent::Stdout(Bytes::from_static(b"x")).is_terminal());
        assert!(!ExecEvent::Stderr(Bytes::from_static(b"x")).is_terminal());
        assert!(ExecEvent::Exit(Some(0)).is_terminal());
        assert!(ExecEvent::Exit(None).is_terminal());
    }
}
