use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::SandboxError;
use crate::types::egress::SessionEgressPolicy;
use crate::types::ids::SandboxId;
use crate::types::sandbox::{
    AgentSpec, ExecEvent, ExecHandle, ExecRequest, ExecStream, SandboxSpec,
};
use crate::types::snapshot::SnapshotMetadata;

/// Combined `AsyncRead + AsyncWrite` so trait-object types below can
/// require both — Rust's trait-object syntax only allows one
/// non-auto trait, so we need this supertrait shim.
pub trait HarnessByteStreamObj: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite + ?Sized> HarnessByteStreamObj for T {}

/// A duplex byte stream the backend hands the harness sink. Owned
/// (`'static`) so the sink can move it into a tokio task.
pub type HarnessByteStream = Pin<Box<dyn HarnessByteStreamObj + Send + Unpin + 'static>>;

/// Callback registered on a [`SandboxBackend`] that wants to expose
/// inbound harness connections. The backend invokes the sink with a
/// fresh `HarnessByteStream` whenever a guest's harness adapter
/// dials in. The sink — typically `HarnessHub::accept_via_session_lookup`
/// — drives the post-attach loop from there.
///
/// `Fn` (not `FnOnce`) so a single sink handles many connections.
pub type HarnessSink = Arc<dyn Fn(HarnessByteStream) + Send + Sync>;

/// VM lifecycle seam. Production implementation: `engram-sandbox-firecracker`
/// (Firecracker over its HTTP-over-Unix-socket API). Dev implementation:
/// `engram-sandbox-process` (host subprocesses, no isolation).
///
/// `exec_stream` is the primary exec method — it returns immediately
/// with a stream of [`ExecEvent`]s so callers can show output in real
/// time (and so multiple subscribers can observe one command running).
/// `exec` is provided with a default impl that drains the stream into
/// an in-memory [`ExecHandle`]; it's a convenience for short commands
/// and unit tests. Backend implementations only need to provide
/// `exec_stream`.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;

    /// Start the long-running agent process for this sandbox (the
    /// harness adapter — Claude Code, the dev noop). Idempotent:
    /// a second call with the agent already running is a no-op.
    ///
    /// **Why agent argv is supplied here, not on `SandboxSpec`.**
    /// `SandboxSpec` is a per-image template; per-session agent argv
    /// (carrying `session_id`, the attach token, etc.) can't ride on
    /// it without conflating "what this image runs" with "what this
    /// session is." The caller supplies the per-session agent at
    /// checkout time.
    ///
    /// **Why this is separate from `create`.** The coordinator
    /// wires routing (e.g. `HarnessHub::bind_session`) before the
    /// agent has a chance to dial out, so an attach can resolve
    /// its target without racing against the spawn.
    ///
    /// Default impl errors with `InvalidSpec` — backends that
    /// don't yet support agents (Firecracker until
    /// `engram-bootstrap` lands) inherit it; backends that do
    /// (ProcessBackend) override.
    async fn start_agent(&self, _id: SandboxId, _agent: AgentSpec) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `start_agent` yet".into(),
        ))
    }

    /// ADR 0014 M1.12 (option D): atomically swap the host file
    /// backing the sandbox's harness virtio-blk drive. Used by
    /// the warm-pool lease path so per-session harness selection
    /// is decoupled from the bake-time template snapshot.
    ///
    /// Implementation contract: pause the VM, `PATCH /drives` on
    /// the harness drive id to point at `new_path`, then resume.
    /// The pause is brief (~30 ms on FC); the resume's
    /// virtio-blk queue-kick invalidates the guest kernel's page
    /// cache for the device, so the next read returns the new
    /// file's bytes (verified by
    /// `engram-sandbox-firecracker/tests/patch_drive_swap.rs`).
    ///
    /// Default: unimplemented. Backends that don't host the
    /// warm pool (VZ, Process) inherit the default — only FC
    /// implements the option-D path.
    async fn swap_harness_drive(
        &self,
        _id: SandboxId,
        _new_path: std::path::PathBuf,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `swap_harness_drive` (option D is FC-only for now)"
                .into(),
        ))
    }

    /// Register a sink that will receive inbound harness connections
    /// (one stream per guest dial). Backends that route the harness
    /// channel through their own transport (FC's vsock UDS, future
    /// ones) call the sink for each fresh connection. Default is a
    /// no-op — `ProcessBackend` doesn't need this hook because its
    /// harness binary dials the host-agent's TCP listener directly.
    ///
    /// Idempotent in the sense that the latest registration wins;
    /// backends shouldn't expect multiple sinks. Pass before any
    /// session-create call so the first dial isn't dropped.
    fn set_harness_sink(&self, _sink: HarnessSink) {}

    /// Push per-session egress policy to the backend. The
    /// coordinator calls this after `create_for_session` returns,
    /// once the sandbox's `guest_ip` is known and before
    /// `start_agent` dispatches — so the harness can't make
    /// network calls before the local proxy knows the policy.
    /// WS-frame ordering between this notify and the subsequent
    /// `start_agent` request is what guarantees the sequencing on
    /// remote-host backends.
    ///
    /// `RemoteSandboxBackend` forwards via the existing WS as a
    /// `NotifyKind::SessionEgressPolicy`. Local backends apply the
    /// policy in-process (used by `--mode=all`). Default is a no-op
    /// for backends with no egress proxy attached (Process /
    /// in-test fixtures). ADR 0006.
    async fn notify_session_policy(
        &self,
        _policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Run a command in the sandbox and return a stream of stdout/stderr
    /// chunks ending with a single [`ExecEvent::Exit`]. Terminating the
    /// stream early (dropping it) does NOT necessarily kill the
    /// underlying process — backends are free to detach and let it run
    /// to completion. Callers that need cancellation should hold the
    /// stream until exit or use a separate kill API (future work).
    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError>;

    /// Convenience wrapper that runs the command to completion and
    /// returns buffered stdout/stderr/exit-status. Default impl drains
    /// `exec_stream`. Don't call this for long-running commands —
    /// stdout/stderr live in memory until the process exits.
    async fn exec(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecHandle, SandboxError> {
        let mut stream = self.exec_stream(id, cmd).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = None;
        while let Some(event) = stream.events.next().await {
            match event {
                ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
                ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
                ExecEvent::Exit(s) => {
                    exit_status = s;
                    break;
                }
            }
        }
        Ok(ExecHandle {
            sandbox_id: stream.sandbox_id,
            exec_id: stream.exec_id,
            stdout,
            stderr,
            exit_status,
        })
    }

    /// Snapshot a running sandbox. ADR 0007 Phase 6: the backend
    /// chooses its own local staging directory (per
    /// [`Self::snapshot_path_for`]) — coord doesn't dictate where
    /// the per-host filesystem cache lives any more, because chunks
    /// in BlobStorage are the cross-host durability primitive and
    /// the local files are just a cache. Returns the metadata the
    /// coord persists to the `snapshots` row.
    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError>;

    /// Restore a sandbox from a previously-taken snapshot. ADR 0007
    /// Phase 6: takes the metadata directly (carrying the manifest
    /// refs + snapshot id) rather than a host-local path — the
    /// backend looks up its own staging dir for `metadata.id` and
    /// rehydrates from chunks if local files are missing.
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;

    /// Local-host path where the backend writes/reads snapshot
    /// artifacts for `snapshot_id`. Used by `PooledBackend` to
    /// chunk `memory.bin` after `snapshot()` returns and to
    /// pre-materialise the file before `restore()`. Backends with
    /// no on-disk snapshot artifacts (e.g. `engram-sandbox-process`)
    /// return a stable per-snapshot dir even if they don't write
    /// FC-style files into it — the caller checks for individual
    /// files before reading.
    fn snapshot_path_for(&self, snapshot_id: crate::types::SnapshotId) -> PathBuf;

    /// ADR 0014 issue #1/#2: commit a snapshot that was just produced
    /// by [`Self::snapshot`]. Signals to the backend that the caller's
    /// downstream pipeline (`record_snapshot` → `destroy` → mark Idle)
    /// has fully succeeded and the snapshot artifacts are now owned by
    /// the `snapshots` row.
    ///
    /// Idempotent: calling commit twice (or commit after abort) is a
    /// no-op. Default impl returns Ok so backends without portable
    /// snapshot artifacts (the in-process and VZ-dev backends) inherit
    /// the trait shape unchanged. The Firecracker pooled backend
    /// overrides to clear its in-flight tracking.
    async fn commit_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0014 issue #1/#2: abort a snapshot previously produced by
    /// [`Self::snapshot`] but whose downstream caller pipeline failed.
    /// Backends with on-disk + BlobStorage artifacts (FC pooled) remove
    /// the per-snapshot directory and the small per-snapshot opaque
    /// blobs (state.bin, sidecar.json, working_set.json). Chunks
    /// stay (content-addressed, dedup-safe, GC'd later).
    ///
    /// Idempotent: aborting twice, or aborting a sandbox with no
    /// in-flight snapshot, is a no-op. The minimal contract is "best
    /// effort cleanup, never panic, never error if there's nothing to
    /// clean." Default impl returns Ok.
    async fn abort_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;

    /// IPv4 address the *host* can use to reach a TCP service running
    /// inside this sandbox's guest. Used by the `GET /sessions/:id/shell`
    /// proxy to dial `ttyd` on the guest. Returns `None` if:
    ///
    /// - the sandbox isn't running yet (no agent up to ask),
    /// - the backend has no host→guest IP routing wired (FC today),
    /// - the agent reported no eligible non-loopback address.
    ///
    /// Default returns `None` so backends that don't yet implement
    /// host→guest IP discovery (FC) inherit a clean "shell unavailable"
    /// surface.
    async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
        None
    }

    /// ADR 0014 issue #6: name of the Linux network namespace the
    /// sandbox's TCP services live behind, if any. Returned for
    /// warm-restored Firecracker sandboxes that run inside a per-VM
    /// `engr-vm-<id>` netns (ADR 0014 M1.16); the host-agent's
    /// ProxyShell handler enters this namespace before dialing
    /// ttyd. `None` for sandboxes whose network is on the host root
    /// (cold FC path before unification, plus all backends without
    /// a netns model: VZ, Process). Default returns None so other
    /// backends inherit the "dial on host root" semantics unchanged.
    async fn netns_name_for(&self, _id: SandboxId) -> Option<String> {
        None
    }

    /// How a harness process inside this backend's sandbox dials
    /// back to the host's harness channel. Drives the `argv` shape
    /// `resolve_harness` builds for the agent. Default is `Vsock`
    /// (the FC / VZ in-VM model); backends that exec the agent as
    /// a host subprocess (`engram-sandbox-process`) override to
    /// `HostTcp`. Centralising this on the trait means the lib's
    /// session-create logic never has to inspect a
    /// `SandboxBackendChoice` enum tag.
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }

    /// Ensure the in-guest `ttyd` is running and bound, returning the
    /// port it accepted on. Called by host-agent's `proxy_shell` flow
    /// just before dialing ttyd — guarantees the host's TCP connect
    /// will find a listener (no more racing the warm-restore against
    /// the in-VM init script that backgrounds ttyd).
    ///
    /// Default implementation assumes ttyd is part of the snapshot
    /// and always listening — returns `Ok(7681)` so non-FC backends
    /// (process, VZ-dev) inherit the legacy behaviour. The FC backend
    /// overrides to send a vsock `StartShell` request to agentd,
    /// which lazily spawns ttyd and only replies once a probe succeeds.
    async fn start_shell(&self, _id: SandboxId) -> Result<u16, SandboxError> {
        Ok(7681)
    }
}

/// How a harness process inside a sandbox reaches the host-side
/// harness channel. Backends declare this via
/// [`SandboxBackend::harness_dial`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessDial {
    /// Agent dials AF_VSOCK CID=2 from inside a VM. Used by FC + VZ.
    Vsock,
    /// Agent runs as a host subprocess and dials TCP loopback. Used
    /// by `engram-sandbox-process` (the test fixture).
    HostTcp,
}
