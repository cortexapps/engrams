use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::SandboxError;
use crate::types::cow_state::{CowState, CowStateRecord};
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

/// ADR 0023: handler for inbound in-guest forge connections (one
/// stream per guest dial on `FORGE_VSOCK_PORT`). Same shape as
/// [`HarnessSink`] but a separate channel — the host reads a
/// `ForgeRequest`, validates the broker token, and replies. FC wires
/// this through its vsock UDS; `ProcessBackend` doesn't need it (its
/// in-guest helper hits the coord's HTTP forge endpoint on loopback).
pub type ForgeSink = Arc<dyn Fn(HarnessByteStream) + Send + Sync>;

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
    /// don't support spawning an in-VM agent inherit it; backends
    /// that do (FC, VZ, ProcessBackend) override.
    async fn start_agent(&self, _id: SandboxId, _agent: AgentSpec) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `start_agent` yet".into(),
        ))
    }

    // ADR 0021 P1.5: `swap_harness_drive` retired. Per-session
    // harness mounting via virtio-blk PATCH /drives is gone — the
    // harness lives in the rootfs at the manifest-declared exec path,
    // so there's no host file to swap.

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

    /// ADR 0023: register a sink for inbound in-guest forge connections
    /// (one stream per guest dial on `FORGE_VSOCK_PORT`). Mirrors
    /// [`set_harness_sink`](Self::set_harness_sink); default no-op
    /// (`ProcessBackend` uses the coord's HTTP forge endpoint instead).
    fn set_forge_sink(&self, _sink: ForgeSink) {}

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

    /// ADR 0018 commit 12m: pause the VM without taking a snapshot.
    /// Idempotent — calling on an already-paused VM is a no-op
    /// success. Used by [`crate::traits::host_client`]-side
    /// orchestration to flush disk + capture memory while the guest
    /// is quiesced (closes the flush-vs-pause race that surfaced
    /// during cross-host evac validation: bytes written by the
    /// guest between flush and FC's internal pause landed in memory
    /// but not in the published disk manifest).
    ///
    /// Default impl returns `Ok(())` for backends with no pause
    /// concept (Process, mocks). FC overrides to call
    /// `FirecrackerClient::pause()`.
    async fn pause(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0018 commit 12m: resume a paused VM. Symmetric companion
    /// to [`Self::pause`]. Idempotent on an already-running VM.
    ///
    /// Most callers don't invoke `resume` directly — [`Self::snapshot`]'s
    /// internal `create_snapshot` pause/capture/resume cycle resumes
    /// the VM on return. `resume` is exposed for diagnostics +
    /// orchestration paths that pause without taking a snapshot
    /// (none today, but the symmetry keeps the trait surface honest).
    async fn resume(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Restore a sandbox from a previously-taken snapshot. ADR 0007
    /// Phase 6: takes the metadata directly (carrying the manifest
    /// refs + snapshot id) rather than a host-local path — the
    /// backend looks up its own staging dir for `metadata.id` and
    /// rehydrates from chunks if local files are missing.
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;

    /// ADR 0020 P1: block until the guest's agentd has dialled its
    /// ready port — i.e. the kernel booted, the rootfs mounted, and
    /// bootstrap/agentd reached `accept()`. This is the same wait
    /// [`Self::start_agent`] does before spawning a harness, exposed
    /// standalone so the base-snapshot capture can reach a quiescent
    /// guest *without* binding a session harness. Default errors —
    /// only the FC backend (which owns the per-sandbox `agent_ready`
    /// watch) implements it.
    async fn wait_agent_ready(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `wait_agent_ready` (FC-only)".into(),
        ))
    }

    /// ADR 0020 P1: the host-local stub harness ext4 the base-snapshot
    /// capture attaches as the harness drive (so the captured snapshot
    /// carries a harness drive slot that `swap_harness_drive` can
    /// re-point per session at restore time). `None` when no stub is
    /// configured — `build_base_snapshot` then fails fast. Only the FC
    /// backend (which holds `FirecrackerConfig.stub_harness_path`)
    /// returns a path.
    fn stub_harness_path(&self) -> Option<PathBuf> {
        None
    }

    /// ADR 0020 Route B: whether `restore` serves guest memory lazily
    /// (UFFD, chunk-native) rather than from a materialized
    /// `memory.bin`. When `true`, the wrapping `PooledBackend` skips
    /// `materialize_memory_if_missing` on restore — the handler faults
    /// chunks straight from the (prefetched) chunk cache, so rebuilding
    /// the contiguous file would be pure overhead. Default `false`
    /// (File mode: memory.bin is required before `load_snapshot`).
    fn restore_memory_is_lazy(&self) -> bool {
        false
    }

    /// ADR 0020 P1: boot `spec` to agentd-ready with the stub harness
    /// attached (harness unmounted — the option-D capture point), take
    /// a portable FC snapshot (chunked memory + uploaded state/sidecar),
    /// tear the capture VM down, and return the snapshot's metadata. The
    /// coord calls this on a host during `POST /api/enabled-images`;
    /// `create_session` later restores from the resulting snapshot.
    ///
    /// Implemented on `PooledBackend` (which owns the chunk-store +
    /// state/sidecar upload that make the snapshot portable). Default
    /// errors so non-pooled backends opt out cleanly.
    async fn build_base_snapshot(
        &self,
        _spec: SandboxSpec,
    ) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `build_base_snapshot` (needs the pooled chunk-store wrapper)".into(),
        ))
    }

    /// ADR 0020 P1: merge per-session env (manifest env + resolved
    /// literal secrets + ENGRAM_SESSION_* ) into a restored sandbox's
    /// environment, so `exec` and the harness see it. A base snapshot is
    /// shared across sessions and can't carry per-session secrets, so
    /// they're injected here post-restore (in cold-create they rode
    /// `vm_spec.env`). ProcessBackend merges into the sandbox's spec env.
    ///
    /// Default is a no-op. NOTE (FC follow-up): the FC guest is already
    /// running from the snapshot, so its env can't be rewritten host-
    /// side — the harness receives session env via `start_agent`'s
    /// `AgentSpec.env`; sandbox-wide exec-env injection on FC restore
    /// needs an agentd-side merge and is tracked separately.
    async fn merge_session_env(
        &self,
        _id: SandboxId,
        _env: std::collections::HashMap<String, String>,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0020 P1: restore a per-image base snapshot for a session and
    /// ADR 0020 P1: restore the image's shared base snapshot for a
    /// new session and inject the per-session env. ADR 0021 P1.5
    /// retired the option-D substrate-swap stage that used to ride
    /// here — the harness lives in the rootfs now and arrives with
    /// the snapshot itself. Implemented on `PooledBackend`.
    async fn restore_base_for_session(
        &self,
        _metadata: SnapshotMetadata,
        _session_env: std::collections::HashMap<String, String>,
    ) -> Result<SandboxId, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `restore_base_for_session` (needs the pooled chunk-store wrapper)".into(),
        ))
    }

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

    /// In-VM dial target for the SHELL tab.
    ///
    /// `guest_ip` returns the IP the host-side egress-proxy registry
    /// uses to identify the session — for warm-restored sandboxes
    /// that's the netns SNAT slot (e.g. 10.200.0.6), the IP the
    /// host SEES traffic coming from after netns POSTROUTING SNAT.
    /// That's the right value for the egress proxy.
    ///
    /// The SHELL tab needs a different IP: the in-VM `eth0`
    /// address that ttyd is bound to. For warm-restored sandboxes
    /// inside a per-VM netns, every VM gets the bake-time
    /// `10.200.0.2` (the bake CIDR's guest octet), and the host's
    /// `proxy_shell` flow enters the netns before dialing — so it
    /// dials `10.200.0.2:7681` *through the TAP*, not the netns's
    /// own veth IP. Returning `guest_ip` (the SNAT slot) here
    /// would dial the veth and miss the VM entirely (prod-shape
    /// failure mode caught by e2e_shell_warm: `connect 10.200.0.6:
    /// 7681: Connection refused` because nothing's bound on the
    /// netns's veth IP).
    ///
    /// For cold-created sandboxes there's no netns + SNAT
    /// indirection: the VM's eth0 is on a TAP in root netns, so
    /// `guest_ip` and `vm_internal_ip` collapse to the same value.
    ///
    /// Default returns the same value as `guest_ip`, matching the
    /// behaviour of pre-M1.16 backends and any future backend that
    /// doesn't need the distinction.
    async fn vm_internal_ip(&self, id: SandboxId) -> Option<String> {
        self.guest_ip(id).await
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

    /// ADR 0016 Phase A: per-sandbox COW diagnostic snapshot.
    /// `None` for backends without an NBD-chunked disk view
    /// (Process, VZ-without-NBD, FC before its NBD attach lands) —
    /// the diagnostic surface treats absence as "this sandbox isn't
    /// chunk-tracked" rather than "this sandbox has zero dirty
    /// bytes." Only `PooledBackend` overrides today; the FC + VZ
    /// inner backends inherit the default. See [`CowState`] for
    /// what the fields mean and ADR 0016 for the design.
    async fn cow_state(&self, _id: SandboxId) -> Option<CowState> {
        None
    }

    /// Bulk variant: one record per sandbox this backend hosts that
    /// *has* a COW view. Skips sandboxes whose individual
    /// `cow_state()` would return `None`. Default empty; only
    /// `PooledBackend` overrides. Lets the host-side gRPC server
    /// answer `CowStateAll` with one map-iteration rather than
    /// `list() + N × cow_state()`.
    async fn cow_state_all(&self) -> Vec<CowStateRecord> {
        Vec::new()
    }

    /// ADR 0016 Phase B commit 4a — explicit admin trigger for the
    /// FlushScheduler's primitive. Forces an immediate
    /// `ChunkedDiskBackend::flush()` on `id` and, if any chunks were
    /// drained, returns the freshly-published `ManifestRef`. Returns
    /// `Ok(None)` when this backend has no chunk-tracked view of
    /// the sandbox (mirrors `cow_state` semantics) or when there
    /// were no dirty chunks.
    ///
    /// Paired with the 30s tick + threshold-notify in
    /// `FlushScheduler` per `[explicit_admin_triggers_for_testability]`:
    /// the scheduler is the implicit trigger; this is the explicit
    /// one. E2E tests use this to drive the publish-to-PG round-trip
    /// without sleeping a full cadence. Operators use it to flush a
    /// stuck sandbox's pending dirty bytes on demand. Default
    /// `Ok(None)` so non-chunked backends compile unchanged.
    async fn flush_sandbox(
        &self,
        _id: SandboxId,
    ) -> Result<Option<crate::types::manifest::ManifestRef>, SandboxError> {
        Ok(None)
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
