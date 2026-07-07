use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::SandboxError;
use crate::types::cow_state::{CowState, CowStateRecord};
use crate::types::egress::SessionEgressPolicy;
use crate::types::endpoints::GuestEndpoints;
use crate::types::ids::SandboxId;
use crate::types::image::WarmConfig;
use crate::types::sandbox::{
    AgentSpec, ExecEvent, ExecHandle, ExecRequest, ExecStream, SandboxProbe, SandboxSpec,
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

/// ADR 0026: handler for inbound in-guest artifact-upload connections
/// (one stream per guest dial on `UPLOAD_VSOCK_PORT`). Same shape as
/// [`ForgeSink`] but a separate channel — the host reads an
/// `UploadRequest` header frame, then the raw file body, streams it to
/// the coord, and replies. FC wires this through its vsock UDS;
/// `ProcessBackend` doesn't need it (its in-guest helper hits the
/// coord's loopback HTTP artifact endpoint).
pub type UploadSink = Arc<dyn Fn(HarnessByteStream) + Send + Sync>;

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
/// ADR 0022 Option A: a point-in-time sample of guest memory across a
/// backend's live sandboxes on one host, summed. `pss_bytes` (proportional
/// set size) charges each shared clean page to a fraction of the sandboxes
/// mapping it, so `pss/rss` is the **density ratio**: ≈1.0 when every
/// sandbox holds a private copy (UFFD `UFFDIO_COPY`), and well below 1.0
/// when same-template siblings `MAP_PRIVATE`-share one base memfile (File
/// backend). The productized substrate for the density measurement — and
/// the metric a later UI ADR reads. Host-aggregate (not per-template) for
/// now: low cardinality, no stale series, and exact in the common
/// single-template-per-host case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuestMemoryStats {
    /// Σ PSS/RSS over sandboxes NOT flagged `parked` — i.e. sandboxes
    /// whose session holds a coordinator memory reservation
    /// (`SessionState::reserves_host_memory`). This is the figure the
    /// RAM ledger (`ram_ledger.rs`, issue #540) adds back into
    /// `allocatable_mib`.
    pub pss_bytes: u64,
    pub rss_bytes: u64,
    /// How many sandboxes were successfully sampled (a dead/unreadable
    /// process is skipped, never fatal).
    pub sampled: u32,
    /// Σ PSS over sandboxes flagged `parked` — RAM-resident but
    /// reservation-free (epic-parking-ladder rungs 2-3). `0` until a
    /// backend ever parks a sandbox (today: always 0, no backend sets
    /// the flag yet). Never added back into `allocatable_mib` — see
    /// [`GuestMemoryStats::pss_bytes`].
    pub parked_pss_bytes: u64,
    /// Σ RSS over sandboxes flagged `parked` — measured alongside
    /// `parked_pss_bytes` (the same `smaps_rollup` read returns both)
    /// but previously discarded. Without this, the density signal
    /// (`Σpss/Σrss < 1.0`) can never be evaluated for parked residents
    /// once the parking ladder lands — the exact population the density
    /// math cares about. Never folded into `allocatable_mib`; a
    /// gauge-only figure, same posture as `parked_pss_bytes`.
    pub parked_rss_bytes: u64,
    /// How many parked sandboxes were successfully sampled.
    pub parked_sampled: u32,
}

/// ADR 0045 C2: see [`SandboxBackend::post_copy_source_view`].
#[derive(Clone, Debug)]
pub struct PostCopySourceView {
    pub fc_pid: u32,
    /// The substrate base dir (tmpfs) — the page server resolves the
    /// exact base file by scanning the FC process's maps for it.
    pub uffd_base_dir: PathBuf,
}

/// Result of [`SandboxBackend::start_browser`] /
/// [`HostClient::start_browser`](crate::traits::HostClient::start_browser).
///
/// `port` is the in-guest RFB port x11vnc is serving on (what the caller
/// dials/relays). `warning` (issue #569) is `Some` when x11vnc came up but
/// chromium's CDP debug port never answered agentd's bounded probe — chrome
/// may be dead or crash-looping behind a healthy VNC. Diagnostic only: a
/// warning never fails the call, and it propagates as log surface up
/// through the host gRPC layer (deliberately NOT into the app-level
/// protos / orchestrator / web).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserStart {
    pub port: u16,
    pub warning: Option<String>,
}

/// ADR 0080: outcome of [`SandboxBackend::refresh_agent`] — did the
/// restored guest's agentd already match the attached bundle
/// generation, or did it re-exec onto a newer one?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRefresh {
    /// The running agentd already matches the slot's content stamp
    /// (the steady state: one stat inside the guest, nothing spawned).
    UpToDate,
    /// The slot carried a different generation; agentd re-exec'd onto
    /// it and answered ready again. New sessions on this host run the
    /// fleet's current agentd without any image recapture.
    Restarted,
}

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

    /// ADR 0026: register a sink for inbound in-guest artifact-upload
    /// connections (one stream per guest dial on `UPLOAD_VSOCK_PORT`).
    /// Mirrors [`set_forge_sink`](Self::set_forge_sink); default no-op
    /// (`ProcessBackend` uses the coord's loopback HTTP artifact
    /// endpoint instead).
    fn set_upload_sink(&self, _sink: UploadSink) {}

    /// Push per-session egress policy to the backend. The
    /// coordinator calls this after `create_for_session` returns,
    /// once the sandbox's `guest_endpoints` is known and before
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

    /// ADR 0066: open a raw duplex byte stream to the in-guest `engram-agentd`
    /// vsock listener on `port` (e.g. `PROXY_PORT_VSOCK_PORT`). VM backends
    /// (FC, VZ) return a connected vsock stream; the caller writes the relay
    /// header, reads the ack, and splices bytes. Backends with no VM boundary
    /// (Process — the guest's `127.0.0.1` *is* the host's loopback) return
    /// `None`, and the caller dials the loopback port directly instead.
    ///
    /// Default: `None` (no vsock). Reuses the same owned `HarnessByteStream`
    /// the harness/forge/upload sinks already carry.
    async fn open_guest_stream(
        &self,
        _id: SandboxId,
        _port: u32,
    ) -> Result<Option<HarnessByteStream>, SandboxError> {
        Ok(None)
    }

    /// Snapshot a running sandbox. ADR 0007 Phase 6: the backend
    /// chooses its own local staging directory (per
    /// [`Self::snapshot_path_for`]) — coord doesn't dictate where
    /// the per-host filesystem cache lives any more, because chunks
    /// in BlobStorage are the cross-host durability primitive and
    /// the local files are just a cache. Returns the metadata the
    /// coord persists to the `snapshots` row.
    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError>;

    /// ADR 0045 D5: the pause-side half of an eviction snapshot — see
    /// `HostClient::snapshot_begin`. Backends that can't background the
    /// upload keep the default (callers fall back to [`Self::snapshot`]).
    async fn snapshot_begin(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::SnapshotId, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `snapshot_begin`".into(),
        ))
    }

    /// ADR 0045 D5: await the background upload spawned by
    /// [`Self::snapshot_begin`]; returns the durable metadata.
    async fn snapshot_wait(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `snapshot_wait`".into(),
        ))
    }

    /// ADR 0045 C1: freeze this sandbox for a live move — pause + NBD
    /// drain + diff capture + local-sink re-chunk; the guest STAYS
    /// PAUSED and the sandbox is fenced until commit/abort. Default
    /// errs (`InvalidSpec`) so non-FC backends and pre-C1 binaries
    /// fall back to the snapshot-rehome teleport.
    async fn migration_capture(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::snapshot::MigrationCaptureOut, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_capture`".into(),
        ))
    }

    /// ADR 0045 C2: the pre-pause half of a post-copy move. Mints the
    /// export identity (the page server parks the dest handler's Hello
    /// until the capture registers it) and packages everything the
    /// destination can use BEFORE the source pauses: the sidecar, the
    /// inline v+1 session manifest (post-copy never re-chunks at
    /// capture), the live disk ref, the hot set. NO pause, no fence —
    /// the guest keeps running until `migration_capture_postcopy`.
    async fn migration_presetup(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::snapshot::MigrationPresetupOut, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_presetup`".into(),
        ))
    }

    /// ADR 0045 C2: the blackout half. Pause → NBD fsync + dirty-tail
    /// drain → vmstate-only snapshot (fork v3) → pagemap scan → seal
    /// the page server's export (the parked dest handler unblocks).
    /// The guest stays paused serving pages until commit (post-drain)
    /// or abort; `state.bin` becomes fetchable via `migration_fetch`.
    async fn migration_capture_postcopy(
        &self,
        _id: SandboxId,
        _export_id: &str,
    ) -> Result<crate::types::snapshot::PostCopyCaptureOut, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_capture_postcopy`".into(),
        ))
    }

    /// ADR 0045 C2 (destination): await the background drain's
    /// terminal outcome — `Done` (source releasable) or `PeerLost`
    /// (dest poisoned + paused; the caller rewinds the session).
    async fn migration_drain_wait(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::snapshot::DrainOutcome, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_drain_wait`".into(),
        ))
    }

    /// ADR 0045 C1: stream an open export's artifacts (allowlisted).
    async fn migration_fetch(
        &self,
        _export_id: &str,
        _items: Vec<crate::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<crate::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_fetch`".into(),
        ))
    }

    /// ADR 0045 C1: the move landed — destroy the frozen source VM and
    /// drop its export + local snapshot dir.
    async fn migration_commit(&self, _id: SandboxId, _export_id: &str) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_commit`".into(),
        ))
    }

    /// ADR 0045 C1: the move failed — re-queue the drained disk tier,
    /// unfence, and un-pause the guest in place. Zero loss.
    async fn migration_abort(&self, _id: SandboxId, _export_id: &str) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `migration_abort`".into(),
        ))
    }

    /// ADR 0028 Fix A: diff-flavored sibling of [`Self::snapshot`].
    /// Same snapshot-dir + sidecar + vmstate contract, but the memory
    /// artifact is `memory.diff` — a sparse file holding ONLY the
    /// pages dirtied since the last capture (KVM dirty bitmap, which
    /// resets on capture, so successive calls chain). The checkpoint
    /// pipeline overlays it onto a rolling full memory image and
    /// re-chunks incrementally; the returned metadata's
    /// `memory_manifest` is patched by that pipeline, exactly like
    /// `snapshot()`'s.
    ///
    /// Requires dirty tracking armed (`track_dirty_pages` at boot /
    /// `enable_diff_snapshots` at load). Default impl returns
    /// `InvalidSpec` for backends with no diff concept (Process, VZ,
    /// mocks); callers treat that as "diff checkpointing unsupported
    /// here" and fall back to full captures, mirroring the
    /// `wait_agent_ready` convention.
    async fn snapshot_diff(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend has no diff-snapshot support".into(),
        ))
    }

    /// ADR 0028 Fix A: does this backend produce coherent, O(dirty-set)
    /// memory checkpoints worth running the periodic checkpoint driver
    /// against? Only Firecracker with `track_dirty_pages` armed does —
    /// VZ has no working guest-memory snapshot (Apple arm64
    /// save/restore is broken, ADR 0003; it clone-snapshots disk +
    /// cold-boots), and Process has no snapshots at all. The driver
    /// gates on this so a split-mode VZ / Process host never pauses
    /// its VMs every cadence interval for a memory-less snapshot that
    /// seeds no chain and writes no record. Default `false`.
    fn supports_diff_checkpoints(&self) -> bool {
        false
    }

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
    ///
    /// ADR 0035: this is the *resume* flavor — aux RO bundles stay on
    /// the generation the snapshot pinned (live guest processes may
    /// hold fds into them). Fresh session creates go through
    /// [`Self::restore_fresh`].
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;

    /// ADR 0035/0055: restore for a *fresh* session (the base-snapshot path) —
    /// identical to [`Self::restore`] except, while the VM is load-paused,
    /// (a) aux RO bundles swap to the host's current generation and (b) the
    /// per-session `selected_mounts` (ADR 0055 skills) are `patch_drive`d into
    /// reserved slots. Default delegates to `restore` for backends without
    /// aux-drive support (VZ, Process, mocks); the FC backend overrides.
    /// `selected_mounts` is ignored by the default.
    async fn restore_fresh(
        &self,
        metadata: SnapshotMetadata,
        _selected_mounts: Vec<crate::types::sandbox::AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        self.restore(metadata).await
    }

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

    /// ADR 0035/0062: the directory this backend reads its RO bundle stamp
    /// (`current.json`) and staged `<sha>.squashfs` generations from — i.e.
    /// where `restore_fresh` resolves a selected skill/harness sha to a file
    /// and `build_base_snapshot` reads the sentinel.
    ///
    /// This is the SINGLE SOURCE OF TRUTH for "where this host's bundles live":
    /// the host-agent reports `current_bundles` by reading the stamp from THIS
    /// path (see `HostAgent::run`), so the heartbeat's advertised catalog is, by
    /// construction, the exact dir the backend will later attach from. The trap
    /// this closes (ADR 0062): the heartbeat and the FC backend used to resolve
    /// the dir independently (the reporter from `ENGRAM_BUNDLE_DIR`, the FC
    /// config from a hardcoded default), so a host could advertise a sha it
    /// couldn't attach. Route every consumer through this accessor instead.
    ///
    /// Defaults to the fleet-canonical [`AuxRoDrive::SHARED_DIR`]; FC/VZ return
    /// their configured dir (production: the default; dev/e2e: `ENGRAM_BUNDLE_DIR`).
    fn bundle_dir(&self) -> &std::path::Path {
        std::path::Path::new(crate::types::sandbox::AuxRoDrive::SHARED_DIR)
    }

    /// The on-disk extension of this backend's staged bundle files
    /// (`<bundle_dir>/<sha256>.<ext>`). FC packs squashfs; VZ packs erofs (its
    /// Kata guest kernel has `CONFIG_EROFS_FS` but no `CONFIG_SQUASHFS`). The
    /// host-agent's `BundleStore` materializes/sweeps generations by this name,
    /// so it MUST match what the backend actually attaches (`bundle_dir` + this)
    /// — otherwise a restore looks for `<sha>.squashfs`, misses the staged
    /// `<sha>.erofs`, and faults to BlobStorage ("blob not found"). The blob
    /// KEY (`AuxRoDrive::blob_key`) stays extension-free — BlobStorage is keyed
    /// by content sha, so only the local staged filename carries the extension.
    /// Defaults to squashfs; VZ overrides to erofs.
    fn bundle_file_ext(&self) -> &'static str {
        "squashfs"
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

    /// ADR 0022 Option A: per-restore-flavor variant of
    /// [`Self::restore_memory_is_lazy`]. `fresh == true` is a base
    /// `session.create` (the `restore_fresh` flavor), which can use the
    /// File backend against the resident per-template memfile even when
    /// idle-resume (`fresh == false`) serves memory lazily via UFFD. The
    /// `PooledBackend` calls this so it materializes the contiguous
    /// `memory.bin` for base-create (File) and skips it for resume
    /// (UFFD). Default delegates to the flavor-agnostic method so
    /// backends that don't bifurcate (VZ, Process) need not implement it.
    fn restore_memory_is_lazy_for(&self, _fresh: bool) -> bool {
        self.restore_memory_is_lazy()
    }

    /// ADR 0022 Option A: sample summed guest memory (PSS/RSS) across this
    /// backend's live sandboxes — the density signal. `None` for backends
    /// that can't measure it (VZ/Process, or non-Linux where there's no
    /// `/proc/<pid>/smaps_rollup`); the host then emits no density gauge.
    /// Must be cheap + error-tolerant: it runs on the periodic heartbeat
    /// tick and must never block or fail the workload
    /// ([reliability_and_latency_first] / telemetry-must-not-gate-workload).
    async fn guest_memory_stats(&self) -> Option<GuestMemoryStats> {
        None
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
    ///
    /// `warm` is the image's optional capture-time prewarm hook
    /// ([`WarmConfig`]): when `Some`, the backend runs it (via `exec`)
    /// after agentd-ready and before the snapshot freezes, so a
    /// long-lived process it spawns is captured live. A warm failure is
    /// fail-loud — it aborts the capture (and the enable).
    ///
    /// `capture_env` is the resolved capture-time env (the coordinator
    /// already resolved any secret refs) merged over the config `[env]`
    /// into the warm hook's exec environment. Empty for an image with no
    /// warm env or no warm hook.
    ///
    /// `capture_egress` (ADR 0080, wire v13) is the egress policy to
    /// register for the capture VM's guest IP while the warm hook runs —
    /// assembled coordinator-side from the config's `warm.network` (one
    /// egress builder for sessions and captures alike). `None` ⇒ register
    /// nothing: the capture stays egress-less (the proxy denies unknown
    /// guests). The backend must NOT derive egress from `warm` itself.
    ///
    /// `progress` (issue #539) receives [`crate::types::CaptureProgress`]
    /// events for the call's lifetime — see the matching doc on
    /// [`crate::traits::HostClient::build_base_snapshot`].
    async fn build_base_snapshot(
        &self,
        _spec: SandboxSpec,
        _warm: Option<WarmConfig>,
        _capture_env: std::collections::HashMap<String, String>,
        _capture_egress: Option<crate::types::egress::SessionEgressPolicy>,
        _progress: tokio::sync::mpsc::Sender<crate::types::CaptureProgress>,
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

    /// ADR 0080: ask the restored guest's agentd to adopt the agentd
    /// bundle generation now attached at its reserved slot
    /// (`AuxRoDrive::AGENTD_SLOT_INDEX`). The captured agentd re-mounts
    /// the bundle slots, compares the slot's content stamp against the
    /// one it booted from, and — if they differ — re-execs itself onto
    /// the new binary before any session state binds. Called by the
    /// fresh-create restore path only (resumes keep their pinned
    /// agentd; the version is per-session).
    ///
    /// Default: `UpToDate` no-op. VZ cold-boots every restore, so its
    /// stage-1 init always picks the attached generation; Process runs
    /// no guest agentd at all.
    async fn refresh_agent(&self, _id: SandboxId) -> Result<AgentRefresh, SandboxError> {
        Ok(AgentRefresh::UpToDate)
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
        _selected_mounts: Vec<crate::types::sandbox::AuxRoDrive>,
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

    /// ADR 0045 C2 (E2B fold): where this sandbox's uffd-handler dumps
    /// its working-set trace (fault-order hot set), when the backend
    /// runs one. The migration capture reads it best-effort to ship a
    /// `hot_chunks` rider so the destination warms the guest's hot set
    /// first. `None` = backend has no per-sandbox trace (VZ, process).
    fn working_set_trace_path(&self, _id: SandboxId) -> Option<PathBuf> {
        None
    }

    /// ADR 0019 / telemetry restoration (#526): where this sandbox's
    /// uffd-handler dumps its per-jail prefault-effectiveness snapshot
    /// (`PrefaultStats` in `engram-uffd-handler`), a sibling of
    /// [`working_set_trace_path`](Self::working_set_trace_path) in the
    /// same jail dir. `PooledBackend::restore` reads it after a resume
    /// completes and emits `engram_resume_prefault_*` — the standing
    /// detector for "prefault shipped but silently stopped firing" (it
    /// went inert three separate, undetected ways before this). `None`
    /// = backend has no per-sandbox prefault detector (VZ, process).
    fn prefault_stats_path(&self, _id: SandboxId) -> Option<PathBuf> {
        None
    }

    /// ADR 0045 C2: what the source page server needs to read this
    /// sandbox's guest memory from outside: FC's pid (this process is
    /// its parent, so `process_vm_readv` is YAMA-legal) and the tmpfs
    /// dir holding the substrate base file its guest RAM is
    /// MAP_PRIVATE of (the `/proc/<pid>/maps` filter key). `None` ⇒
    /// not a substrate-restored FC sandbox (cannot post-copy).
    fn post_copy_source_view(&self, _id: SandboxId) -> Option<PostCopySourceView> {
        None
    }

    /// ADR 0045 C2 (destination): the uffd-handler's control socket
    /// for this sandbox, when it was spawned in peer mode (drain
    /// progress / PeerLost reports). `None` otherwise.
    fn post_copy_control_sock(&self, _id: SandboxId) -> Option<PathBuf> {
        None
    }

    /// ADR 0044 K2 survivor rehydrate: the host block device this
    /// sandbox's rootfs drive reads (e.g. `/dev/nbd4` for a
    /// chunked-NBD rootfs). After a host-agent restart the new
    /// generation must re-serve EXACTLY this device — the surviving
    /// FC holds an open fd to it, so attaching a fresh slot would
    /// serve a device nobody reads. `None` for sandboxes whose
    /// rootfs isn't a block device (file-backed, or backend doesn't
    /// track it).
    fn rootfs_device(&self, _id: SandboxId) -> Option<PathBuf> {
        None
    }

    /// ADR 0045 C2: compose the restore sidecar from LIVE sandbox
    /// state (the presetup's pre-pause package; byte-identical to the
    /// capture-time sidecar by construction).
    fn compose_live_sidecar(
        &self,
        _id: SandboxId,
        _memory_manifest: Option<crate::types::manifest::ManifestRef>,
    ) -> Result<Vec<u8>, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `compose_live_sidecar`".into(),
        ))
    }

    /// ADR 0045 C2: write a vmstate-only snapshot package (sidecar +
    /// fork-v3 `state.bin`, no memory artifact). Caller holds the VM
    /// paused and owns resume.
    async fn snapshot_vmstate_only_package(
        &self,
        _id: SandboxId,
        _sidecar_json: &[u8],
    ) -> Result<(crate::types::SnapshotId, PathBuf), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `snapshot_vmstate_only_package`".into(),
        ))
    }

    /// ADR 0045 C2: persist (or clear, `None`) the sandbox's post-copy
    /// migration role into the backend's reattach manifest so a
    /// host-agent restart re-learns the lifecycle fences. No-op for
    /// backends with no reattach story (VZ, process).
    async fn set_manifest_migration_role(
        &self,
        _id: SandboxId,
        _role: Option<&str>,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

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

    /// ADR 0068 probe-before-host_lost: ground-truth liveness for ONE
    /// sandbox. Default impl (VZ/Process — neither backend has an
    /// orphan-VM mode: the process IS the sandbox, so the live
    /// in-memory map member already is ground truth) reports
    /// `known_to_backend` from `list()` membership and mirrors it into
    /// `process_alive`. FC overrides this with an INDEPENDENT check —
    /// reading the persisted per-sandbox manifest's three-axis pid
    /// identity (the same one the survivor-reattach pass trusts)
    /// rather than trusting the in-memory map, since the in-memory map
    /// (or its heartbeat-carried mirror `running_sandboxes`) being
    /// wrong is exactly the desync this probe exists to catch.
    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        let known_to_backend = self.list().await?.contains(&id);
        Ok(SandboxProbe {
            known_to_backend,
            process_alive: known_to_backend,
        })
    }

    /// The sandbox's guest-network identity — a single coherent value
    /// replacing the retired `guest_ip` / `netns_name_for` /
    /// `vm_internal_ip` accessor triple (the "three-IP accident").
    /// See [`GuestEndpoints`] for what each field means and why they
    /// can differ (netns SNAT slot vs. in-VM eth0 address vs. the
    /// namespace to dial from — real FC network mechanism, now
    /// expressed as named fields instead of sibling methods a caller
    /// had to choose among).
    ///
    /// `None` if the sandbox isn't running, the backend has no
    /// host→guest routing wired yet, or the guest hasn't reported an
    /// address (VZ pre-agentd-answer). Default `None`, matching the
    /// old `guest_ip` default: "shell/egress unavailable".
    async fn guest_endpoints(&self, _id: SandboxId) -> Option<GuestEndpoints> {
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

    /// ADR 0065: ensure the in-guest browser stack is running and x11vnc is
    /// bound, returning the port plus an optional chromium-liveness warning
    /// (issue #569 — see [`BrowserStart`]). FC/VZ override to send
    /// `StartBrowser` over the agentd channel; the dev ProcessBackend has no
    /// real guest and inherits this default (the feature is gated to FC/VZ
    /// profiles).
    async fn start_browser(&self, _id: SandboxId) -> Result<BrowserStart, SandboxError> {
        Ok(BrowserStart {
            port: 5900,
            warning: None,
        })
    }

    /// ADR 0065: tear down the in-guest browser stack. Default no-op.
    async fn stop_browser(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
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
