//! `SandboxBackend` wrapper that adds host-side resource resolution
//! (image cache, chunk store, materialize-to-file, optional NBD,
//! optional egress proxy) on top of the inner backend (VZ or FC).
//!
//! Originally this also held a warm pool of pre-created sandboxes;
//! that was deleted as part of the ADR 0008 follow-up — see commit
//! history for context. Briefly: warm pools' value (~30 ms saved on
//! cold start) didn't earn its complexity in the multi-tenant
//! world we'd grown into (every session is a different
//! SandboxSpec, so the per-key warm-slot hit rate trended to
//! zero), and the FC production-target replacement
//! (canonical-memory restore from ADR 0007 Phase 5) is strictly
//! better. VZ dev now pays full cold-boot (~1 s) per session;
//! that's the explicit tradeoff.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::SandboxBackend;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::image::WarmConfig;
use engram_core::types::sandbox::{
    AgentSpec, AuxBundleRef, ExecRequest, ExecStream, SandboxSpec, WriteFileResult, WriteFileSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use tokio::fs;
use tokio::sync::Mutex;

use crate::egress::HostEgress;
use crate::image_cache::{CachedImage, ImageCache};

/// Cross-platform optional NBD state slot. Linux carries the real
/// state; non-Linux is `()` so the `resolve_rootfs` return signature
/// is consistent and call sites cfg-gate the storage-stash branch.
#[cfg(target_os = "linux")]
type NbdStateSlot = Option<crate::disk_daemon::NbdSandboxState>;
#[cfg(not(target_os = "linux"))]
type NbdStateSlot = ();

#[cfg(target_os = "linux")]
fn nbd_state_none() -> NbdStateSlot {
    None
}
#[cfg(not(target_os = "linux"))]
fn nbd_state_none() -> NbdStateSlot {}

/// ADR 0039 item #19: bounded concurrency for the cold-restore memory
/// chunk prefetch. Raised from 8 — single-stream GCS ~80 MB/s on the
/// prod n2-standard-8 hosts left ~90 % of the 10 Gbps line rate idle
/// while a cold resume blocked 66.5 s on this prefetch (traced). 32
/// (~2.5 GB/s aggregate) saturates closer to line rate without tripping
/// GCS per-object rate limits. Mirrors the re-chunk/upload bound so the
/// save and load paths use the same fleet-tuned fan-out.
const MEMORY_PREFETCH_CONCURRENCY: usize = 32;

/// The cloneable post-phase result future stored per sandbox by
/// `snapshot_begin` / `migration_finish_restore` and consumed by
/// `snapshot_wait`.
///
/// ADR 0045 D5 originally stored a raw `JoinHandle` here and the
/// `snapshot_wait` consumer removed the entry BEFORE awaiting it —
/// so a single cancelled/timed-out wait (tonic drops the server-side
/// handler future on a coordinator deadline or pod restart) lost the
/// only reader, leaving a fully-uploaded snapshot permanently
/// unretrievable and wedging eviction finalize (issue #221).
///
/// The fix: store a [`futures::future::Shared`] of the result so the
/// wait is idempotent and retryable, and tolerant of two concurrent
/// waiters (the coordinator's RPCs are at-least-once). The entry is
/// removed only on *successful* consumption (or by supersession /
/// destroy), not on a cancelled await. The spawned task's
/// [`AbortHandle`](tokio::task::AbortHandle) rides alongside so
/// supersession and destroy can still abort the backing upload.
///
/// `SnapshotMetadata` is `Clone` but `SandboxError` is not, so the
/// error is wrapped in `Arc` to satisfy `Shared`'s `Clone` bound.
type SharedSnapshotResult = futures::future::Shared<
    futures::future::BoxFuture<'static, Result<SnapshotMetadata, Arc<SandboxError>>>,
>;

/// A retryable post-phase result plus the abort handle for its
/// backing task. See [`SharedSnapshotResult`].
struct SnapshotWait {
    shared: SharedSnapshotResult,
    abort: tokio::task::AbortHandle,
}

impl SnapshotWait {
    /// Wrap a spawned post-phase `JoinHandle` into a cloneable,
    /// retryable wait.
    fn from_handle(
        handle: tokio::task::JoinHandle<Result<SnapshotMetadata, SandboxError>>,
    ) -> Self {
        use futures::future::FutureExt as _;
        let abort = handle.abort_handle();
        let shared = (async move {
            match handle.await {
                Ok(Ok(meta)) => Ok(meta),
                Ok(Err(e)) => Err(Arc::new(e)),
                Err(join_err) => Err(Arc::new(SandboxError::Snapshot(format!(
                    "snapshot upload task: {join_err}"
                )))),
            }
        })
        .boxed()
        .shared();
        Self { shared, abort }
    }
}

/// Return the cached image's legacy `rootfs.ext4` path or a
/// typed error when neither it nor a bundle is present.
///
/// ADR 0007 Phase 6: chunked-storage OCI pushes ship only
/// `bundle.json`; the disk is resolved through the chunk store.
/// Callers hit this helper on the fallback paths (no chunk_store
/// wired, or no bundle on the image) — at that point we *need* a
/// concrete file, and the absence of both rootfs.ext4 AND bundle
/// is a config error worth surfacing loudly rather than papering
/// over.
fn legacy_rootfs_path(cached: &CachedImage) -> Result<PathBuf, SandboxError> {
    cached.rootfs_path.clone().ok_or_else(|| {
        SandboxError::InvalidSpec(format!(
            "image {} has neither rootfs.ext4 nor a chunk-store path resolvable here \
             (host missing chunk_store/materialize_dir wiring?)",
            cached.digest,
        ))
    })
}

/// ADR 0045 C2 (destination): a staged post-copy restore awaiting its
/// fetch poller spawn (keyed by the restore metadata's snapshot id;
/// consumed inside `restore_with` once the NBD handle exists). The
/// fields drive the Linux-only poller; non-Linux builds never
/// construct one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone)]
struct PostCopyDestPending {
    source_addr: String,
    export_id: String,
}

/// ADR 0045 C2 disk post-copy (destination): fetch sealed disk chunks
/// from the frozen source over the migration gRPC channel. Dialed
/// lazily and reused across fetches (the drain's bounded fan-out and
/// the guest's demand faults share it; HTTP/2 multiplexes). A failed
/// attempt resets the channel so the next one re-dials. `Err` after
/// the bounded retries is TERMINAL — the overlay latches lost and the
/// coordinator rewinds.
#[cfg(target_os = "linux")]
struct GrpcPostCopyDiskFetcher {
    source_addr: String,
    export_id: String,
    client: tokio::sync::Mutex<Option<engram_protocol::grpc_client::GrpcHostClient>>,
}

#[cfg(target_os = "linux")]
impl GrpcPostCopyDiskFetcher {
    fn new(source_addr: String, export_id: String) -> Self {
        Self {
            source_addr,
            export_id,
            client: tokio::sync::Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<engram_protocol::grpc_client::GrpcHostClient, String> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let channel = tonic::transport::Endpoint::from_shared(self.source_addr.clone())
            .map_err(|e| format!("bad source_addr: {e}"))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| format!("dial migration source: {e}"))?;
        let c = engram_protocol::grpc_client::GrpcHostClient::new(channel);
        *guard = Some(c.clone());
        Ok(c)
    }

    async fn fetch_once(&self, chunk_idx: u64) -> Result<bytes::Bytes, String> {
        let client = self.client().await?;
        let mut stream = client
            .migration_fetch(
                &self.export_id,
                vec![engram_core::types::snapshot::MigrationItem::DiskChunkAt(
                    chunk_idx,
                )],
            )
            .await
            .map_err(|e| format!("migration fetch: {e}"))?;
        use futures::StreamExt;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(frame) = stream.next().await {
            let frame = frame.map_err(|e| format!("fetch frame: {e}"))?;
            buf.extend_from_slice(&frame.data);
            if frame.last {
                return Ok(bytes::Bytes::from(buf));
            }
        }
        Err("stream ended before the last frame".into())
    }
}

#[cfg(target_os = "linux")]
impl crate::disk_daemon::PostCopyDiskFetcher for GrpcPostCopyDiskFetcher {
    fn fetch(
        &self,
        chunk_idx: u64,
    ) -> futures::future::BoxFuture<'_, Result<bytes::Bytes, String>> {
        Box::pin(async move {
            const ATTEMPTS: u32 = 4;
            let mut last = String::new();
            for attempt in 1..=ATTEMPTS {
                match self.fetch_once(chunk_idx).await {
                    Ok(b) => return Ok(b),
                    Err(e) => {
                        last = e;
                        // Drop the cached channel; the next attempt
                        // re-dials (covers a source pod hiccup without
                        // declaring the peer dead on one RST).
                        *self.client.lock().await = None;
                        if attempt < ATTEMPTS {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                250 * attempt as u64,
                            ))
                            .await;
                        }
                    }
                }
            }
            Err(format!(
                "disk chunk {chunk_idx} after {ATTEMPTS} attempts: {last}"
            ))
        })
    }
}

/// ADR 0045 C2: a minted-but-not-yet-captured post-copy export. The
/// capture (Linux-only) consumes it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone)]
struct PendingPresetup {
    export_id: String,
    peer_token: String,
    /// The chain ref the presetup advertised as the session manifest
    /// (the capture validates the chain hasn't moved underneath).
    chain_ref: engram_core::types::manifest::ManifestRef,
}

/// Issue #202: the unwind guard for a migration / snapshot capture's
/// "point of no return" window — between the guest `pause` and the
/// moment the export (or the snapshot finisher) takes ownership of the
/// drained state.
///
/// Inside that window the capture has frozen the guest, raised the
/// disk `migration_fence` (so background `flush()` no-ops), and DRAINED
/// the dirty disk buffer out into a local `PendingDiskFlush` (or a
/// post-copy seal). If a fallible `?` await returns `Err`, OR tonic
/// drops the handler future on a coordinator deadline / disconnect, no
/// error arm runs — so without a Drop-based guard the guest stays
/// paused forever, the fence stays raised (silent disk-RPO collapse),
/// and the drained chunks are lost from every future published manifest
/// (a silent disk rollback for the next restore). See the issue for the
/// three independently-confirmed code paths.
///
/// `Drop` performs the SAME recovery `migration_abort` does
/// (`requeue_pending` + `requeue_postcopy_seal` + `set_migration_fence
/// (false)` + best-effort `resume`), plus it re-inserts the consumed
/// post-copy presetup so a coordinator retry can succeed. Because the
/// dirty re-queue + resume are async, `Drop` spawns a task. `defuse` is
/// called once ownership transfers (export registered / capture handed
/// to the finisher), making the success path a zero-behavior-change
/// no-op.
struct CaptureUnwind {
    inner: Arc<dyn SandboxBackend>,
    id: SandboxId,
    /// The disk backend to unfence + requeue against. The disk types are
    /// cross-platform (`disk_daemon::backend`), so these fields are too —
    /// only the capture call sites that POPULATE them are Linux-gated.
    /// `None` when the sandbox has no NBD disk (non-Linux, memory-only).
    disk_backend: Option<Arc<crate::disk_daemon::ChunkedDiskBackend>>,
    /// Whether the fence was raised by the capture (so Drop knows to
    /// lower it). Tracked separately from `disk_backend` because the
    /// ordinary-snapshot path drains WITHOUT fencing.
    fenced: bool,
    /// Drained-but-unowned dirty chunks to re-queue on unwind.
    disk_pending: Option<crate::disk_daemon::PendingDiskFlush>,
    /// Post-copy seal to re-queue on unwind (C2 only).
    disk_seal: Option<Arc<crate::disk_daemon::PostCopyDiskSeal>>,
    /// The presetup consumed by `migration_capture_postcopy`, restored
    /// on unwind so the coordinator's retry finds a matching presetup
    /// instead of failing "no matching presetup" (C2 only).
    #[allow(clippy::type_complexity)]
    presetup_restore: Option<(
        SandboxId,
        PendingPresetup,
        Arc<DashMap<SandboxId, PendingPresetup>>,
    )>,
    defused: bool,
}

impl CaptureUnwind {
    /// Arm a guard that, on unwind, resumes the guest. Disk fields are
    /// attached separately so the caller can move drained state into the
    /// guard as it produces it.
    fn new(inner: Arc<dyn SandboxBackend>, id: SandboxId) -> Self {
        Self {
            inner,
            id,
            disk_backend: None,
            fenced: false,
            disk_pending: None,
            disk_seal: None,
            presetup_restore: None,
            defused: true,
        }
    }

    /// Mark the guard armed (the pause has happened; from here Drop must
    /// run the unwind unless `defuse` is called).
    fn arm(&mut self) {
        self.defused = false;
    }

    /// Ownership transferred (export registered / capture handed to the
    /// finisher). Drop becomes a no-op.
    fn defuse(&mut self) {
        self.defused = true;
    }
}

impl Drop for CaptureUnwind {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        let inner = self.inner.clone();
        let id = self.id;
        let disk_backend = self.disk_backend.take();
        let fenced = self.fenced;
        let disk_pending = self.disk_pending.take();
        let disk_seal = self.disk_seal.take();
        if let Some((pid, presetup, map)) = self.presetup_restore.take() {
            // Restore the consumed presetup so a coordinator retry finds
            // a match — unless a NEWER presetup already landed (last-
            // write-wins, matching `migration_presetup`'s own policy).
            map.entry(pid).or_insert(presetup);
        }
        tracing::warn!(
            sandbox_id = %id,
            "capture unwind: capture failed or was cancelled after pause+fence+drain; \
             requeueing drained disk state, clearing the migration fence, resuming the guest",
        );
        // The dirty re-queue + resume are async; Drop can't await, so
        // hand the SAME recovery `migration_abort` runs to a task. Drop
        // can run outside a runtime (process shutdown, sync test); fall
        // back to a synchronous fence clear so a fenced backend never
        // stays wedged even then.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            if let Some(backend) = &disk_backend {
                if fenced {
                    backend.set_migration_fence(false);
                }
            }
            tracing::warn!(
                sandbox_id = %id,
                "capture unwind: no tokio runtime in Drop; cleared fence synchronously, \
                 could not requeue/resume (best-effort)",
            );
            return;
        };
        handle.spawn(async move {
            if let Some(backend) = disk_backend {
                if let Some(pending) = disk_pending {
                    backend.requeue_pending(pending).await;
                }
                if let Some(sealed) = disk_seal.as_deref() {
                    backend.requeue_postcopy_seal(sealed).await;
                }
                if fenced {
                    backend.set_migration_fence(false);
                }
            }
            if let Err(e) = inner.resume(id).await {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "capture unwind: best-effort resume failed (guest may stay paused until destroy)",
                );
            } else {
                tracing::info!(sandbox_id = %id, "capture unwind: guest resumed in place");
            }
        });
    }
}

/// Wraps an inner [`SandboxBackend`] (FC or VZ) with host-side
/// resource resolution: image cache, chunk store, materialize-to-
/// file, optional NBD daemon, optional egress proxy.
///
/// Pre-warm-pool-deletion this also held a `Pool` of pre-created
/// sandboxes and a `pool_key`-based checkout dance. Both are gone;
/// `create` just resolves resources and forwards to `inner`.
/// Cold-create cost on each backend:
///
/// - **FC + canonical-memory-baked image**: ~100-500 ms via UFFD-
///   from-chunks restore. The "warm" of post-warm-pool-world.
/// - **FC, legacy bake**: ~500-700 ms full cold boot.
/// - **VZ**: ~1 s full cold boot. No memory-snapshot primitive
///   available on macOS arm64 Linux (Apple-side bug).
pub struct PooledBackend {
    inner: Arc<dyn SandboxBackend>,
    /// Phase 5+: if `Some`, `create()` resolves `spec.image_uri`
    /// through this cache before delegating to `inner`. The cache
    /// owns OCI pull + on-disk layout; `create` reads
    /// `cached.bundle` to decide between rootfs paths (legacy
    /// ext4, ADR 0007 chunked manifest, ADR 0008 chunked-OCI).
    image_cache: Option<ImageCache>,
    /// ADR 0006: per-host egress proxy. When attached, incoming
    /// `notify_session_policy` calls register against the local
    /// proxy registry and `destroy` unregisters. `None` is the
    /// "no egress filtering" path (dev or operator opt-out).
    egress: Option<Arc<HostEgress>>,
    /// `sandbox_id → session_id` index, the host-agent's
    /// canonical binding map. Two consumers:
    ///
    /// - ADR 0006: `destroy(sandbox_id)` resolves to a `session_id`
    ///   for `Registry::unregister(session_id)`.
    /// - ADR 0016 Phase B: the `LiveManifestPublisher`'s drain task
    ///   holds a clone for the sandbox→session lookup at publish
    ///   time; warm-pool sandboxes (future) share the same scheduler
    ///   API — the publisher returns "skip" for any sandbox not yet
    ///   bound to a session.
    ///
    /// Renamed from `egress_sessions` in ADR 0017 Phase C: the map
    /// is no longer an egress-only concern. `Arc` wrapping lets the
    /// publisher drain task and the destroy path share ownership.
    session_bindings: Arc<DashMap<SandboxId, SessionId>>,
    /// ADR 0090: survivors whose NBD slot this generation quarantined
    /// (rehydrate `RECONFIGURE` failed). Advertised in every heartbeat
    /// until the sandbox is destroyed; the coordinator drives
    /// `evict_local → resume` off it.
    quarantined_survivors: Arc<DashMap<SandboxId, SessionId>>,
    /// ADR 0091: guests whose control plane stopped answering (3/3
    /// socket probes refused after a checkpoint failure). Advertised in
    /// every heartbeat until cleared by a successful capture or destroy;
    /// the coordinator flips the session Active → Unreachable off it.
    unreachable_guests: Arc<DashMap<SandboxId, SessionId>>,
    /// ADR 0007: chunk-store-backed materialization. When set, the
    /// `bundle.json` on a cached image is the source of truth for
    /// the disk — chunks are fetched from `BlobStorage`, written to
    /// a content-addressed file under `materialize_dir`, and that
    /// path becomes `spec.rootfs_source`. When unset, we fall back
    /// to the OCI-pulled `rootfs.ext4` (transitional path; retired
    /// in Phase 6).
    chunk_store: Option<ChunkStore>,
    /// ADR 0095: requester-side peer health for the resume peer-fill
    /// arm — one per backend so a lost peer is skipped across every
    /// resume on this host for the lost-window.
    peer_health: std::sync::Arc<crate::peer_fill::PeerHealth>,
    /// Per-host directory where chunked manifests are materialized.
    /// `Some` iff `chunk_store` is. Files inside are named by
    /// `manifest_id`-`version` so two sessions hitting the same
    /// manifest share the materialized file (and VZ's per-sandbox
    /// APFS clonefile / FC's NBD-on-the-shared-path work on top).
    materialize_dir: Option<PathBuf>,
    /// ADR 0035/0062: staged-bundle dir (`<sha256>.squashfs` + the bake's
    /// `current.json`). NOT set independently — `new()` copies it from
    /// `inner.bundle_dir()`, so it always equals the dir the inner backend reads
    /// generations from (the single source of truth; `ENGRAM_BUNDLE_DIR` or the
    /// `SHARED_DIR` default).
    bundle_dir: PathBuf,
    /// Optional NVMe-backed LRU cache fronting the chunk store.
    /// When wired, chunk reads during materialization go through
    /// the cache — chunks shared across manifests (canonical-base
    /// images, fork lineage) stay local across rematerializes
    /// rather than re-fetching from `BlobStorage` every time.
    /// When unset, `materialize_chunked_rootfs` goes straight to
    /// the store.
    chunk_cache: Option<ChunkCache>,
    /// Single-flight gate: stops two concurrent `create()` calls
    /// from racing on the same manifest_id+version file. Held only
    /// for the materialize critical section, not the whole call.
    materialize_lock: Mutex<()>,
    /// ADR 0080 §C: scratch root for image materialization
    /// (`MaterializeImage` RPC) — per-run subdirs live under it and
    /// are scrubbed by the materializer's guard + the startup
    /// reconcile. `None` = this host can't materialize (no scratch
    /// wired; dev/in-process compositions).
    materialize_scratch: Option<PathBuf>,
    /// ADR 0080 §C: the ≤1-concurrent-materialize-per-host gate. A
    /// `try_lock` miss returns the retryable `Busy` failure — the
    /// coordinator re-picks a host instead of queueing image pulls
    /// behind each other on one NVMe.
    materialize_gate: Arc<tokio::sync::Mutex<()>>,
    /// ADR 0008 Phase 5: OCI client used as the *origin tier* of the
    /// tiered chunk-fault path. When `Some`, `resolve_rootfs` checks
    /// `cached.is_disk_chunked_oci()` and builds a
    /// `TieredChunkResolver` that consults BlobStorage first and
    /// falls through to OCI Range GET on miss, opportunistically
    /// write-filling BlobStorage with the fetched chunk (CDN-fill).
    /// `None` keeps the legacy BlobStorage-only path.
    oci_client: Option<engram_oci::OciClient>,
    /// ADR 0007 Phase 4: pool of `/dev/nbdN` device paths the
    /// host-agent allocates from when serving chunked disks via
    /// the NBD daemon. `None` = NBD disabled (the materialize-to-
    /// file fallback runs instead — that's the macOS path and the
    /// transitional Linux path before operators wire `nbds_max=N`).
    /// Cross-platform because the allocator itself is target-
    /// agnostic; the daemon that consumes a slot is Linux-only.
    nbd_pool: Option<Arc<crate::disk_daemon::NbdSlotAllocator>>,
    /// Live NBD daemon state, keyed by sandbox id. Each entry
    /// owns the daemon's kernel-binding handle, the data-plane
    /// backend (used for snapshot flush), and the slot lease
    /// (Drop returns `/dev/nbdN` to the pool). Linux-only — on
    /// macOS the materialize-to-file path is the only option for
    /// chunked disks.
    ///
    /// INVARIANT: never hold a `DashMap` `Ref`/`RefMut` guard (the
    /// value returned by `.get()` / `.iter()` / `.get_mut()`) across
    /// an `.await`. A guard pins the shard's **synchronous**
    /// `RwLock`; any contending writer (`destroy`'s `remove`,
    /// `create`/`restore`/`rehydrate`'s `insert`) hashing to the same
    /// shard then blocks its OS worker thread for the whole await,
    /// and dashmap-6's writer-preference subsequently blocks every
    /// new reader on that shard too. With ≥ worker_threads collisions
    /// (a busy host capturing/destroying/creating concurrently) all
    /// tokio workers park in sync lock waits, the guard-holding
    /// future can never be polled to release it, and the runtime
    /// deadlocks — heartbeats stop and the host is declared dead.
    /// Instead, clone the `Arc<ChunkedDiskBackend>` (and copy
    /// `device_path().to_path_buf()` etc.) out of the guard, drop the
    /// guard, then `.await` on the owned `Arc`. See `flush_sandbox`
    /// for the canonical pattern and `migration_fetch` for the rule
    /// restated inline. TOCTOU note: a `None`-after-clone (entry
    /// removed mid-op) is handled identically to "entry missing".
    #[cfg(target_os = "linux")]
    nbd_sandboxes: Arc<DashMap<SandboxId, crate::disk_daemon::NbdSandboxState>>,
    /// ADR 0014 issue #1/#2: per-sandbox in-flight snapshot tracking.
    /// `snapshot(sandbox_id)` records the produced snapshot_id here so
    /// (a) a retry from the same sandbox first aborts the prior attempt
    /// (overwrite-in-place semantics — prevents the 4-GiB-per-retry
    /// leak the prod incident on `engrams-fc-xngk` exhibited) and
    /// (b) the caller can `commit`/`abort` by sandbox_id without
    /// passing the snapshot_id back. Cleared by `commit_snapshot` and
    /// `abort_snapshot`; not persisted across host-agent restarts (a
    /// restart between snapshot and commit/abort leaves an orphan dir
    /// — small bounded leak we accept until a host-side janitor lands).
    inflight_snapshots: Arc<DashMap<SandboxId, engram_core::types::SnapshotId>>,
    /// ADR 0016 Phase A: unix-ms timestamp of the last successful
    /// `snapshot(sandbox_id)` per sandbox. Read by `cow_state` to
    /// populate the memory-tier RPO field. `0`/absent = never
    /// snapshotted; the diagnostic surface renders `0` as "never".
    /// Stamped at the tail of the `snapshot()` post-processing
    /// block, after BlobStorage uploads succeed but before the
    /// `inflight_snapshots` insert (which gates commit/abort).
    /// Cleared on `destroy(sandbox_id)` so the entry doesn't
    /// outlive its sandbox.
    last_snapshot_unix_ms: Arc<DashMap<SandboxId, i64>>,
    /// ADR 0101 B: per-sandbox pacing sample from the last completed
    /// capture (diff dirty bytes + epoch length) — the adaptive
    /// checkpoint controller's input. Written by the snapshot post
    /// phase, cleared on destroy alongside `last_snapshot_unix_ms`.
    checkpoint_pacing: Arc<DashMap<SandboxId, EpochPacingSample>>,
    /// ADR 0016 Phase B: continuous-flush scheduler config + the
    /// publisher impl the scheduler hands its outcomes to. Cloned
    /// into every cold-create / resume / restart-rehydration site
    /// that spawns a scheduler. Defaults to the
    /// [`crate::disk_daemon::NoOpLiveManifestPublisher`] in
    /// commit 2; commit 4 wires the real coord-bound publisher via
    /// [`Self::with_live_manifest_coord_publisher`].
    flush_config: crate::disk_daemon::FlushSchedulerConfig,
    live_manifest_publisher: Arc<dyn crate::disk_daemon::LiveManifestPublisher>,
    /// Owns the drain task spawned by the live-manifest publisher.
    /// Dropping aborts the task; held here so it shares
    /// PooledBackend's lifetime. `None` for the no-op publisher (no
    /// task to abort).
    live_manifest_publisher_handle: Option<crate::disk_daemon::LiveManifestPublisherHandle>,
    /// ADR 0028 Fix A: root for checkpoint state — `rolling/` (the
    /// per-sandbox rolling memory images, diff-apply targets) and
    /// `records/` (durable per-checkpoint records awaiting coord
    /// ack). `None` disables checkpoint chains entirely: `snapshot()`
    /// stays pure-Full and the periodic driver no-ops — the rollout
    /// gate, and the natural state for non-FC backends.
    checkpoint_dir: Option<PathBuf>,
    /// Durable mirror of the chain heads (survivor rehydrate after a
    /// pod roll) — every record mutation routes through this store so
    /// a cancelled persist's detached tail can never resurrect a record
    /// a later capture invalidated (see [`crate::checkpoint::ChainHeadStore`]).
    /// `Some` ⟺ `checkpoint_dir` is `Some`.
    chain_heads: Option<Arc<crate::checkpoint::ChainHeadStore>>,
    /// ADR 0028 Fix A: per-sandbox rolling chain state. Present once
    /// the first (Full, seeding) checkpoint completed; every
    /// subsequent `snapshot()` on that sandbox — periodic checkpoint
    /// OR eviction/drain/SIGTERM capture — rides the O(dirty) diff
    /// path against it. Cleared on `destroy`.
    checkpoint_chains: Arc<DashMap<SandboxId, crate::checkpoint::CheckpointChain>>,
    /// ADR 0028 Fix A: serializes captures per sandbox — a periodic
    /// checkpoint must never interleave with an eviction snapshot
    /// (double-pause is idempotent but concurrent NBD flushes +
    /// chain advances are not).
    capture_locks: Arc<DashMap<SandboxId, Arc<tokio::sync::Mutex<()>>>>,
    /// ADR 0045 D5: per-sandbox background upload tasks spawned by
    /// `migration_finish_restore`, awaited by `snapshot_wait`. Issue
    /// #529: `snapshot_begin`'s eviction flavor no longer inserts here —
    /// it doesn't use the wait/RPC-routing shape at all anymore (see
    /// `pending_finalizes` below); this map now serves the migration
    /// restore flavor exclusively.
    ///
    /// Issue #221: the result is stored as a cloneable, retryable
    /// [`SharedSnapshotResult`] (not a raw `JoinHandle`). The
    /// coordinator's finalize RPC is at-least-once — deadlines fire and
    /// pods restart mid-call — so the wait must tolerate a cancelled
    /// await and a retry, and two concurrent waiters. The entry is
    /// removed only on successful consumption / supersession / destroy.
    snapshot_waits: Arc<DashMap<SandboxId, SnapshotWait>>,
    /// Issue #529: `sandbox_id → snapshot_id` for an in-flight (durably
    /// persisted, not-yet-terminal) eviction finalize job. Seeded from
    /// disk at startup (`resume_pending_finalizes`) and on every fresh
    /// `snapshot_begin`; cleared only when the job reaches its terminal
    /// stage or is quarantined. Makes `snapshot_begin` idempotent under
    /// an eviction-scanner retry storm: re-observe the pending
    /// `snapshot_id` instead of re-capturing.
    pending_finalizes: Arc<DashMap<SandboxId, engram_core::types::SnapshotId>>,
    /// Issue #529: a weak self-reference, set once via `set_self_ref`
    /// right after construction (see `lib.rs`, alongside
    /// `Arc::new(p)`) — so a detached eviction finalize job, which only
    /// has `EvictionFinalizer` (a bundle of Arc-cloned fields, built from
    /// `&self`), can still reach the FULL `PooledBackend::destroy` (egress
    /// unregister, NBD slot release, checkpoint-chain teardown — not just
    /// the inner backend's VM teardown) at its terminal stage, without
    /// threading an owned `Arc<PooledBackend>` through `snapshot_begin`'s
    /// `&self` signature. `Weak` so holding a clone can never keep the
    /// backend alive past its natural lifetime.
    self_ref: Arc<std::sync::OnceLock<std::sync::Weak<PooledBackend>>>,
    /// ADR 0045 C1: open live-migration exports (frozen sandboxes
    /// serving a move). See `crate::migration`.
    migrations: Arc<crate::migration::MigrationRegistry>,
    /// ADR 0045 C1 (destination): inline, not-yet-durable disk
    /// manifests staged by a migration pull, consumed by
    /// `prepare_resume_nbd_attach` (keyed by the provisional ref).
    inline_disk_manifests:
        Arc<DashMap<engram_core::types::manifest::ManifestRef, engram_chunk_store::Manifest>>,
    /// ADR 0045 C2 (source): the post-copy page server. Set once at
    /// host-agent startup when the 9102 listener is configured; the C2
    /// capture registers `PeerExport`s here after its pagemap scan.
    migrate_peer: Arc<std::sync::OnceLock<Arc<crate::migrate_peer::PeerServer>>>,
    /// ADR 0045 C2: presetups awaiting their capture (export identity
    /// minted pre-pause; `migration_capture_postcopy` consumes it).
    pending_presetups: Arc<DashMap<SandboxId, PendingPresetup>>,
    /// ADR 0045 C2 (destination): staged post-copy restores awaiting
    /// their fetch-poller spawn inside `restore_with`.
    postcopy_dests: Arc<DashMap<engram_core::types::SnapshotId, PostCopyDestPending>>,
    /// ADR 0045 C2: sandboxes currently playing a post-copy role. Both
    /// roles fence the normal lifecycle drivers (idle-evict nomination
    /// skips them; checkpoints/flush are already fenced by the capture
    /// guard + migration fence). Mirrored into the FC sandbox manifest
    /// for reattach (ADR 0044 K2).
    migration_roles: Arc<DashMap<SandboxId, crate::migration::MigrationRole>>,
    /// ADR 0044 K2 (issue #224): terminal-mode flag for graceful
    /// shutdown. `abandon_nbd_data_planes_for_shutdown` sets this
    /// `true` (SeqCst) BEFORE draining `nbd_sandboxes`, turning
    /// abandonment from a one-shot sweep of the map's current
    /// contents into a sticky terminal mode. Every NBD insert site
    /// (`create`, `restore_with`, `rehydrate_sandbox`) checks it
    /// immediately before the `insert`: if set, the freshly-built
    /// `NbdSandboxState` is `abandon_for_shutdown()`-ed (kernel
    /// config left alive for the successor) rather than inserted —
    /// so a still-running registration/rehydrate task or an
    /// in-flight gRPC handler that finishes AFTER the sweep can no
    /// longer leak a live data plane into the map only to have
    /// `NbdHandle::Drop` netlink-disconnect a surviving FC's device
    /// at process exit. `Arc` so the spawned create/restore insert
    /// tasks (issue #223) can move a clone in. Linux-only — the
    /// abandon contract exists only where NBD data planes do.
    #[cfg(target_os = "linux")]
    abandoning: Arc<std::sync::atomic::AtomicBool>,
    /// Issue #225: the coord client + host id used to SYNCHRONOUSLY
    /// publish a survivor's freshly-flushed `live_disk_manifest`
    /// during the SIGTERM final-flush pass. The normal flush path
    /// publishes through the async `live_manifest_publisher`'s
    /// coalescing drain task — but that task is aborted on process
    /// exit, so a manifest queued during shutdown would never reach
    /// coord and the successor would rehydrate from the stale ref.
    /// The shutdown pass therefore POSTs directly here, before the
    /// process exits. Set by `with_live_manifest_coord_publisher`
    /// (the same wiring that builds the async publisher); `None` for
    /// the no-op / test publishers, in which case the shutdown flush
    /// still drains chunks to GCS but skips the coord publish.
    shutdown_manifest_publish: Option<(
        Arc<dyn engram_host_core::CoordControlPlane>,
        engram_core::HostId,
    )>,
    /// ADR 0098 D1: wall clock is an injected world input (record
    /// timestamps, the pause mark, the migration TTL). P8 closed the
    /// flow-extraction arc: every seam reaches its flow through its own
    /// field (`clock`, `host_fs`, `shutdown_manifest_publish`,
    /// `DeviceSync`/`NbdKernel` at their entry points) — the loose fields
    /// ARE the end state; `HostEffects::production` remains the sim's
    /// assembly point, not a prod indirection.
    clock: Arc<dyn engram_core::traits::Clock>,
    /// ADR 0098 P5: the durable-fs seam. Prod is [`TokioFs`]; Flow D's
    /// durable records + the shutdown spool perform every fs op through
    /// it (the host-internal simulator's `CrashFs` intercepts at op
    /// boundaries). Remaining loose-field seams consolidate into the
    /// full `HostEffects` bundle with the last flow-extraction PRs.
    host_fs: Arc<dyn engram_host_core::HostFs>,
}

/// ADR 0098 P5: the prod [`crate::eviction_finalize::EvictionSandbox`] —
/// upgrades the weak `PooledBackend` ref at call time so the detached
/// finalize job reaches the FULL `PooledBackend::destroy` (egress
/// unregister, NBD slot release, checkpoint-chain teardown). A gone
/// backend (process shutting down) is success: nothing left to destroy
/// here, and `orphan_reap` backstops the sandbox itself.
pub(crate) struct PooledDestroyer {
    pub(crate) self_ref: Arc<std::sync::OnceLock<std::sync::Weak<PooledBackend>>>,
}

#[async_trait]
impl crate::eviction_finalize::EvictionSandbox for PooledDestroyer {
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        let Some(pooled) = self.self_ref.get().and_then(std::sync::Weak::upgrade) else {
            return Ok(());
        };
        use engram_core::traits::SandboxBackend as _;
        pooled.destroy(id).await
    }
}

/// Human-readable message for a [`crate::warm_progress::WarmViolation`] —
/// shared by `run_warm_hook`'s three watchdog-triggered failure sites.
fn warm_violation_message(v: crate::warm_progress::WarmViolation) -> String {
    use crate::warm_progress::WarmViolation;
    match v {
        WarmViolation::Stall => format!(
            "[warm] hook went silent (no output or progress line) for at least the \
             stall budget (ENGRAM_WARM_STALL_SECS, default {}s)",
            crate::warm_progress::DEFAULT_STALL_SECS,
        ),
        WarmViolation::StageDeadline => {
            "[warm] hook stage exceeded its declared deadline_secs".into()
        }
        WarmViolation::GlobalTimeout => "[warm] hook exceeded its global WarmConfig timeout".into(),
    }
}

/// Pull the free-text `msg=`/heartbeat detail (if any) out of a parsed
/// progress line, for the `CaptureProgress.detail` field.
fn progress_line_detail(line: &crate::warm_progress::WarmProgressLine) -> Option<String> {
    use crate::warm_progress::WarmEvent;
    match &line.event {
        WarmEvent::Start { msg, .. } | WarmEvent::Heartbeat { msg, .. } => msg.clone(),
        WarmEvent::Done { .. } => None,
    }
}

/// Record `engram_warm_hook_stage_seconds` for every CLOSED stage in a
/// `[warm]`-hook stage history (the still-open stage, if any — `outcome:
/// Running` — has no duration to record).
/// ADR 0088 addendum (adversarial-review fix): the pre-seed balloon
/// inflate with rollback-safe cleanup. Returns:
///
/// - `Ok(true)` — the inflate PATCH landed (whatever the guest granted,
///   including 0 MiB): the caller OWES a confirmed `balloon_release`
///   before any guest workload runs.
/// - `Ok(false)` — no balloon in play (target 0, the TYPED no-device
///   `InvalidSpec`, or a failed reclaim that was successfully
///   normalized back to deflated): dense seed, nothing to release.
/// - `Err` — the reclaim failed AND the balloon could not be confirmed
///   deflated: the capture must fail rather than run a warm hook in a
///   possibly-starved guest.
///
/// The reclaim's `InvalidSpec` is the only fail-open path; every other
/// reclaim error is treated as "the inflate target may have landed"
/// (it is PATCHed before the first statistics poll) and normalized via
/// release-and-confirm.
async fn balloon_inflate_for_seed(
    inner: &dyn SandboxBackend,
    id: SandboxId,
    target_mib: u64,
    deadline: std::time::Duration,
) -> Result<bool, SandboxError> {
    if target_mib == 0 {
        return Ok(false);
    }
    match inner.balloon_reclaim(id, target_mib, deadline).await {
        // Even a 0-MiB grant means the target PATCH landed — release.
        Ok(_granted_mib) => Ok(true),
        Err(SandboxError::InvalidSpec(msg)) => {
            tracing::info!(
                sandbox_id = %id,
                detail = %msg,
                "no balloon available; taking a dense cold-base seed",
            );
            Ok(false)
        }
        Err(reclaim_err) => {
            tracing::warn!(
                sandbox_id = %id,
                error = %reclaim_err,
                "balloon reclaim failed after the inflate may have landed; normalizing via release",
            );
            match inner.balloon_release(id).await {
                // Confirmed deflated: safe to proceed with a dense seed.
                Ok(()) => Ok(false),
                // No device ⇒ the inflate PATCH never landed either.
                Err(SandboxError::InvalidSpec(_)) => Ok(false),
                Err(release_err) => Err(SandboxError::Snapshot(format!(
                    "balloon reclaim failed ({reclaim_err}) and the normalizing release also \
                     failed ({release_err}); balloon state unknown"
                ))),
            }
        }
    }
}

fn record_warm_stage_metrics(stages: &[engram_core::types::WarmStageRecord]) {
    use engram_core::types::WarmStageOutcome;
    for stage in stages {
        let Some(ended_at) = stage.ended_at else {
            continue;
        };
        let outcome = match stage.outcome {
            WarmStageOutcome::Done => "done",
            WarmStageOutcome::Failed => "failed",
            WarmStageOutcome::Running => continue,
        };
        let secs = (ended_at - stage.started_at).num_milliseconds().max(0) as f64 / 1000.0;
        metrics::histogram!(
            crate::metrics::WARM_HOOK_STAGE_SECONDS,
            "stage" => stage.name.clone(),
            "outcome" => outcome,
        )
        .record(secs);
    }
}

/// Record `engram_warm_hook_failures_total{kind}` for a capture failure.
fn record_warm_hook_failure_metric(kind: engram_core::types::CaptureFailureKind) {
    metrics::counter!(crate::metrics::WARM_HOOK_FAILURES_TOTAL, "kind" => kind.as_str())
        .increment(1);
}

/// RAII: aborts the wrapped keepalive task on drop. A leg wrapped by
/// [`spawn_leg_keepalive`] stops resending stale progress the moment
/// the guard goes out of scope — on every path, including an early
/// `?`-return, since `Drop` runs during unwind too.
struct KeepaliveGuard(tokio::task::JoinHandle<()>);

impl Drop for KeepaliveGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Issue #539 finding 1: `run_warm_hook`'s own >=30s keepalive only
/// covers the WARM phase. The BOOT leg (cold boot + chunk materialize to
/// agentd-ready) and the SNAPSHOT leg (pause/flush/chunk memory + upload
/// a multi-GB state blob to BlobStorage) can each run many minutes with
/// no `[warm]`-hook progress traffic to drive a keepalive — and the
/// coordinator's fenced `enable_jobs` write IS the capture-claim lease
/// renewal (`enable_scanner.rs` deleted the separate blind ticker on
/// exactly this premise). Silence past `lease_secs` during either leg
/// lets a peer coordinator re-claim and spawn a SECOND concurrent
/// capture — the regression the deleted ticker existed to prevent.
///
/// Wrap a leg that has no progress source of its own in this: it
/// resends `event` unchanged every
/// [`capture_keepalive_secs_from_env`][crate::warm_progress::capture_keepalive_secs_from_env]
/// (30s in production; test-shrinkable) until the returned guard is
/// dropped.
fn spawn_leg_keepalive(
    progress: tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    event: engram_core::types::CaptureProgress,
) -> KeepaliveGuard {
    KeepaliveGuard(tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(crate::warm_progress::capture_keepalive_secs_from_env());
        interval.tick().await; // consume the immediate first tick
        loop {
            interval.tick().await;
            let _ = progress.try_send(event.clone());
        }
    }))
}

impl PooledBackend {
    /// ADR 0019 / telemetry restoration (#526): read the uffd-handler's
    /// per-jail prefault-effectiveness snapshot and emit the
    /// `engram_resume_prefault_*` counters. Detached into a bounded,
    /// backgrounded poll (spawned, non-blocking — restore must never wait
    /// on it): `prefault_from_trace` runs on the handler's dedicated
    /// background thread and only writes the file at the END of that
    /// fetch, so a synchronous read raced the write and chronically
    /// misclassified healthy replayed resumes as `stats_missing`. A file
    /// that still isn't there after `PREFAULT_STATS_POLL_TIMEOUT` really
    /// does mean the handler died or the wiring didn't fire — the alarm
    /// the counter exists for. Outcome lands as attributes on a dedicated
    /// `resume.prefault_stats` span carrying `sandbox_id` — detached work
    /// gets its own span, correlated by attribute.
    fn spawn_prefault_stats_probe(&self, id: SandboxId) {
        let Some(stats_path) = self.prefault_stats_path(id) else {
            return;
        };
        tokio::spawn(tracing::Instrument::instrument(
            async move {
                let stats_bytes = read_prefault_stats_with_retry(&stats_path).await;
                let outcome = classify_prefault_outcome(stats_bytes.as_deref());
                let span = tracing::Span::current();
                span.record("outcome", outcome.label());
                if let PrefaultOutcome::Replayed { installed, skipped } = outcome {
                    span.record("installed", installed as u64);
                    span.record("skipped", skipped as u64);
                }
                // The peer-fill snapshot (post-copy migration destinations
                // only) — span attributes only, not a counter; only
                // recorded when nonzero so the common non-peer resume
                // doesn't carry four always-0 fields.
                if let Some(peer) = parse_peer_fill_snapshot(stats_bytes.as_deref())
                    .filter(PeerFillSnapshot::is_nonzero)
                {
                    span.record("peer_pulled", peer.peer_pulled);
                    span.record("peer_alt_sourced", peer.peer_alt_sourced);
                    span.record("peer_zero_chunks", peer.peer_zero_chunks);
                    span.record("peer_live_faults", peer.peer_live_faults);
                }
                tracing::info!(outcome = outcome.label(), "resume prefault effectiveness");
                emit_prefault_metrics(outcome);
            },
            tracing::info_span!(
                "resume.prefault_stats",
                sandbox_id = %id,
                outcome = tracing::field::Empty,
                installed = tracing::field::Empty,
                skipped = tracing::field::Empty,
                peer_pulled = tracing::field::Empty,
                peer_alt_sourced = tracing::field::Empty,
                peer_zero_chunks = tracing::field::Empty,
                peer_live_faults = tracing::field::Empty,
            ),
        ));
    }

    /// Capture-time egress (ADR 0080, wire v13): the capture VM gets a tap +
    /// guest IP like any sandbox, but no egress policy is registered for it,
    /// so the proxy denies its traffic as `UnknownGuest`. When the coordinator
    /// shipped a capture policy (assembled from the config's `warm.network` —
    /// the posture decision is coordinator-side now; this host never derives
    /// egress from `WarmConfig` itself), stamp the sandbox-dependent identity
    /// (this `sandbox_id` + the VM's guest IP) onto it and register it for the
    /// duration of the capture. Returns the policy's synthetic [`SessionId`],
    /// which the caller MUST pass to [`Self::unregister_capture_egress`] after
    /// teardown. No-op (returns `None`) when no policy was shipped, no local
    /// proxy is wired, or the guest has no IP — the capture then stays
    /// egress-less.
    async fn register_capture_egress(
        &self,
        id: SandboxId,
        policy: Option<SessionEgressPolicy>,
    ) -> Option<SessionId> {
        // No coordinator-granted egress → egress-less (the common case);
        // skip the guest-IP lookup entirely.
        let mut policy = policy?;
        let egress = self.egress.as_ref()?;
        let Some(guest_ip) = self
            .inner
            .guest_endpoints(id)
            .await
            .map(|ep| ep.egress_identity)
        else {
            tracing::warn!(
                sandbox_id = %id,
                "capture egress: capture VM has no guest IP; [warm] hook runs egress-less",
            );
            return None;
        };
        // The coordinator assembled the posture half; the sandbox-dependent
        // identity half only exists here (the VM was created inside this
        // call), so stamp it now.
        policy.sandbox_id = id;
        policy.guest_ip = guest_ip;
        let session_id = policy.session_id;
        let allow_all = policy.allow_all;
        let allow_hosts = policy.network_allow_hosts.clone();
        let allow_host_patterns = policy.network_allow_host_patterns.clone();
        match crate::egress::register_policy(&egress.registry, policy) {
            Ok(()) => {
                tracing::info!(
                    sandbox_id = %id,
                    %guest_ip,
                    allow_all,
                    ?allow_hosts,
                    ?allow_host_patterns,
                    "capture egress: registered policy for the [warm] hook",
                );
                Some(session_id)
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "capture egress: policy translate failed; [warm] hook runs egress-less",
                );
                None
            }
        }
    }

    /// Tear down a capture-egress registration made by
    /// [`Self::register_capture_egress`]. Safe to call with `None`.
    fn unregister_capture_egress(&self, session_id: Option<SessionId>) {
        let (Some(sid), Some(egress)) = (session_id, self.egress.as_ref()) else {
            return;
        };
        egress.registry.unregister(sid);
        tracing::debug!(
            session_id = %sid,
            "capture egress: unregistered policy after capture teardown",
        );
    }

    /// Run `sync` in the guest — the base-capture quiesce (see the call
    /// site in `build_base_snapshot`). Bounded: sync of a warm image's
    /// dirty set is seconds; 120 s covers a slow chunked-NBD writeback
    /// without letting a wedged guest hang the capture forever.
    async fn sync_guest_fs(&self, id: SandboxId) -> Result<(), SandboxError> {
        use engram_core::types::sandbox::ExecEvent;
        use futures::StreamExt;

        // Via `sh -c` so PATH/applet resolution finds sync on both
        // full images and the busybox test fixtures (no bare /bin/sync
        // there).
        let req = ExecRequest {
            command: vec!["/bin/sh".into(), "-c".into(), "sync".into()],
            stdin: None,
            env: std::collections::HashMap::new(),
            workdir: None,
            timeout: Some(std::time::Duration::from_secs(120)),
        };
        let started = crate::time_source::metrics_now();
        let stream = self
            .exec_stream(id, req)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("pre-capture guest sync exec: {e}")))?;
        let mut events = stream.events;
        while let Some(ev) = events.next().await {
            if let ExecEvent::Exit(status) = ev {
                return if status == Some(0) {
                    tracing::info!(
                        sandbox_id = %id,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "pre-capture guest sync complete",
                    );
                    Ok(())
                } else {
                    Err(SandboxError::Snapshot(format!(
                        "pre-capture guest sync exited {status:?}"
                    )))
                };
            }
        }
        Err(SandboxError::Snapshot(
            "pre-capture guest sync: exec stream ended without an exit status".into(),
        ))
    }

    /// Run an image's capture-time `[warm]` hook ([`WarmConfig`]) in the
    /// live capture VM, just before the base snapshot is frozen. The warm
    /// command starts a long-lived process detached (e.g. `gradle
    /// --daemon`) and exits; that process is then captured into the base
    /// snapshot so every restored session inherits it warm.
    ///
    /// Issue #539: replaces the old buffered `self.exec` + single opaque
    /// `WarmConfig::timeout()` with a streaming watchdog. A hook that
    /// emits the `::engram-warm::` progress protocol (see
    /// `warm_progress`) is killed within `ENGRAM_WARM_STALL_SECS` of
    /// going silent, or at a declared stage's `deadline_secs`, whichever
    /// is sooner; a hook that never emits a progress line keeps today's
    /// behavior verbatim (only the global timeout applies — no
    /// regression). Every failure path returns a structured
    /// `SandboxError::CaptureFailed` carrying the failing stage and the
    /// hook's last 16 KiB of combined stdout+stderr — no more "(see host
    /// logs for stderr)". On success the [`crate::warm_progress::OutputTail`]
    /// is returned too, so a LATER snapshot-phase failure can still report
    /// the warm hook's last output (the diagnosis a `status None` / a
    /// vsock-lost failure used to lose entirely).
    async fn run_warm_hook(
        &self,
        id: SandboxId,
        warm: &WarmConfig,
        env: &std::collections::HashMap<String, String>,
        progress: &tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    ) -> Result<crate::warm_progress::OutputTail, SandboxError> {
        use crate::warm_progress::{
            parse_progress_line, warm_stall_secs_from_env, OutputTail, WarmWatchdog,
            WarmWatchdogConfig, WatchdogInput,
        };
        use engram_core::types::sandbox::ExecEvent;
        use engram_core::types::{CaptureFailure, CaptureFailureKind, CapturePhase};
        use futures::StreamExt;

        // The capture VM's agentd holds NO durable session env: a capture VM
        // never gets a session bind, and `merge_session_env` is a no-op on FC
        // (the guest is already running from the snapshot, so its env can't be
        // rewritten host-side). So the manifest `[env]` must ride in the
        // ExecRequest — agentd layers `req.env` onto the exec'd child
        // (handler.rs), giving the hook JAVA_HOME/PATH/etc. Without it a
        // gradle/node warmup fails fast (the dev-brain hook exited 1 in ~40 ms
        // with no JAVA_HOME).
        let req = ExecRequest {
            command: warm.command.clone(),
            stdin: None,
            env: env.clone(),
            workdir: warm.workdir.clone(),
            // In-guest backstop unchanged: agentd SIGKILLs the child at this
            // deadline regardless of what the host-side watchdog decides.
            timeout: Some(warm.timeout()),
        };
        tracing::info!(
            sandbox_id = %id,
            command = ?warm.command,
            timeout_secs = warm.timeout().as_secs(),
            "running capture-time [warm] hook before base-snapshot capture",
        );

        let mut stream = self.exec_stream(id, req).await.map_err(|e| {
            SandboxError::CaptureFailed(CaptureFailure {
                kind: CaptureFailureKind::WarmExecTransport,
                stage: None,
                tail: String::new(),
                message: format!(
                    "[warm] hook exec_stream failed before base-snapshot capture: {e}"
                ),
            })
        })?;

        let watchdog_cfg = WarmWatchdogConfig {
            stall: warm_stall_secs_from_env(),
            global_timeout: warm.timeout(),
        };
        let started = crate::time_source::metrics_now();
        let mut watchdog = WarmWatchdog::new(watchdog_cfg, started);
        let mut tail = OutputTail::default();
        let mut pending_stdout: Vec<u8> = Vec::new();
        let mut last_detail: Option<String> = None;

        let mut keepalive =
            tokio::time::interval(crate::warm_progress::capture_keepalive_secs_from_env());
        keepalive.tick().await; // consume the immediate first tick

        let send_progress =
            |watchdog: &WarmWatchdog, tail: &OutputTail, detail: &Option<String>| {
                let event = engram_core::types::CaptureProgress {
                    phase: CapturePhase::Warm,
                    sandbox_id: Some(id),
                    warm_stage: watchdog.current_stage_name().map(str::to_string),
                    detail: detail.clone(),
                    output_tail: tail.render(),
                    warm_stages: watchdog.stage_history(),
                };
                let _ = progress.try_send(event);
            };

        let violation_failure = |kind: CaptureFailureKind,
                                 watchdog: WarmWatchdog,
                                 tail: &OutputTail,
                                 detail: &Option<String>,
                                 message: String| {
            let stage = watchdog.current_stage_name().map(str::to_string);
            let stages = watchdog.clone().finish_failed(self.clock.now_utc());
            record_warm_stage_metrics(&stages);
            record_warm_hook_failure_metric(kind);
            let output_tail = tail.render();
            // Issue #563 review correction: this is the TERMINAL event — the
            // one carrying the failing stage + output tail an operator
            // actually needs. Unlike the routine per-line `send_progress`
            // sends, a dropped one here is worth surfacing: warn loudly
            // (rather than the usual silent `let _ =`) so a full channel
            // doesn't quietly eat the one frame that mattered.
            if let Err(e) = progress.try_send(engram_core::types::CaptureProgress {
                phase: CapturePhase::Warm,
                sandbox_id: Some(id),
                warm_stage: stage.clone(),
                detail: detail.clone(),
                output_tail: output_tail.clone(),
                warm_stages: stages,
            }) {
                tracing::warn!(
                    error = %e,
                    stage = ?stage,
                    "run_warm_hook: dropped the TERMINAL capture-progress event \
                     (progress channel full) — operator loses the live stage/tail \
                     for this failure, falling back to the CaptureFailed message",
                );
            }
            SandboxError::CaptureFailed(CaptureFailure {
                kind,
                stage,
                tail: output_tail,
                message,
            })
        };

        let exit_status = loop {
            let deadline = tokio::time::Instant::from_std(watchdog.next_deadline());
            tokio::select! {
                ev = stream.events.next() => {
                    match ev {
                        Some(ExecEvent::Stdout(bytes)) => {
                            tail.push(&bytes);
                            let now = crate::time_source::metrics_now();
                            let wall_now = self.clock.now_utc();
                            if let Some(v) = watchdog.on_event(WatchdogInput::OutputBytes, now, wall_now) {
                                return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                            }
                            pending_stdout.extend_from_slice(&bytes);
                            while let Some(pos) = pending_stdout.iter().position(|&b| b == b'\n') {
                                let line_bytes: Vec<u8> = pending_stdout.drain(..=pos).collect();
                                let line = String::from_utf8_lossy(&line_bytes);
                                let line = line.trim_end_matches(['\r', '\n']);
                                let Some(parsed) = parse_progress_line(line) else {
                                    continue;
                                };
                                last_detail = progress_line_detail(&parsed);
                                let now = crate::time_source::metrics_now();
                                let wall_now = self.clock.now_utc();
                                if let Some(v) = watchdog.on_event(WatchdogInput::Progress(parsed), now, wall_now) {
                                    return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                                }
                                send_progress(&watchdog, &tail, &last_detail);
                            }
                            // A conforming `::engram-warm::` line is always
                            // short. Newline-free output (gradle rich-console
                            // `\r` redraws, binary noise) would otherwise grow
                            // this buffer unbounded for the hook's whole
                            // 10-33 min runtime, and the `position(b'\n')`
                            // scan above re-walks it on every chunk
                            // (quadratic). Cap it at the same bound as the
                            // `OutputTail` ring buffer: past that with no
                            // newline in sight, it can't be a valid protocol
                            // line, so drop it — nothing valid is lost, and
                            // parsing resumes cleanly at the next `\n`.
                            if pending_stdout.len() > OutputTail::DEFAULT_CAP_BYTES {
                                pending_stdout.clear();
                            }
                        }
                        Some(ExecEvent::Stderr(bytes)) => {
                            tail.push(&bytes);
                            let now = crate::time_source::metrics_now();
                            let wall_now = self.clock.now_utc();
                            if let Some(v) = watchdog.on_event(WatchdogInput::OutputBytes, now, wall_now) {
                                return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                            }
                        }
                        Some(ExecEvent::Exit(status)) => break status,
                        None => {
                            return Err(violation_failure(
                                CaptureFailureKind::WarmExecTransport,
                                watchdog,
                                &tail,
                                &last_detail,
                                "[warm] hook exec stream ended before an Exit event (vsock/gRPC transport lost)".into(),
                            ));
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = crate::time_source::metrics_now();
                    let wall_now = self.clock.now_utc();
                    if let Some(v) = watchdog.on_event(WatchdogInput::Tick, now, wall_now) {
                        return Err(violation_failure(v.kind(), watchdog, &tail, &last_detail, warm_violation_message(v)));
                    }
                }
                _ = keepalive.tick() => {
                    send_progress(&watchdog, &tail, &last_detail);
                }
            }
        };

        match exit_status {
            Some(0) => {
                tracing::info!(sandbox_id = %id, "[warm] hook completed cleanly");
                record_warm_stage_metrics(&watchdog.stage_history());
                send_progress(&watchdog, &tail, &last_detail);
                Ok(tail)
            }
            other => {
                tracing::error!(
                    sandbox_id = %id,
                    exit_status = ?other,
                    tail = %tail.render(),
                    "[warm] hook failed; aborting base-snapshot capture",
                );
                // A `None` status means the child died to a signal — but
                // agentd's `timeout_ms` in-guest backstop (`handler.rs`)
                // is only ONE producer of that. A guest-OOM kill (one of
                // the failure causes `WarmConfig`'s own docs name) at
                // minute 2 of a 55-minute budget also reports
                // `Exit(None)`; labeling it `WarmGlobalTimeout` steers an
                // operator to raise `timeout_secs` instead of fixing
                // memory. Gate the timeout label on actually having
                // reached (near) the declared budget — some slack for
                // scheduling jitter between agentd's kill and this
                // observing it — else classify as an unattributed signal
                // kill. A stream that dies WITHOUT ever producing an Exit
                // event (handled above, `None` from
                // `stream.events.next()`) is the distinct, retryable
                // `WarmExecTransport` case.
                const TIMEOUT_SLACK: std::time::Duration = std::time::Duration::from_secs(2);
                let kind = match other {
                    Some(_) => CaptureFailureKind::WarmExitNonZero,
                    None if started.elapsed() + TIMEOUT_SLACK >= warm.timeout() => {
                        CaptureFailureKind::WarmGlobalTimeout
                    }
                    None => CaptureFailureKind::WarmKilled,
                };
                Err(violation_failure(
                    kind,
                    watchdog,
                    &tail,
                    &last_detail,
                    format!("[warm] hook exited with status {other:?} (expected 0)"),
                ))
            }
        }
    }

    /// Shared body of `restore` (resume flavor) and `restore_fresh`
    /// (fresh-create flavor) — see ADR 0035 §3 for the split.
    async fn restore_with(
        &self,
        metadata: SnapshotMetadata,
        fresh: bool,
        // ADR 0077 phase 2: fork the disk manifest identity at NBD attach.
        // True whenever this restore attaches a SHARED disk manifest (a
        // fresh create off a base snapshot, or a capture VM restoring a
        // shared cold base) — each consumer's flush chain must own a
        // private lineage, exactly like the cold-create path
        // (`try_spawn_nbd` fork=true). False for resume/rehydrate/
        // migration, which re-attach the session's OWN already-forked id
        // and must tick it, not fork again. Distinct from `fresh`: a
        // Hit-capture restore is fork=true but fresh=false (it keeps the
        // snapshot's pinned mounts and resume memory semantics).
        fork_disk_at_attach: bool,
        // ADR 0055: per-session skills to patch into reserved slots (fresh only;
        // ignored on resume, which keeps the snapshot's pinned mounts).
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        // ADR 0014 M1.13: eager parallel prefetch of memory chunks
        // into local NVMe BEFORE we hand off to materialize +
        // inner.restore. Without this, materialize_to_file_cached's
        // serial chunk iteration pays N×GCS-RTT (~300 ms each) on
        // a cold-cache host — a 512 MiB template's 32 chunks come
        // out as ~10 s of serial fetch. Parallel prefetch with
        // bounded concurrency reduces that to roughly ~2 s.
        // Subsequent restores against the same template hit NVMe
        // and the prefetch is a no-op.
        // ADR 0020 P3: span the pre-`restore_in_jail` prep legs (memory
        // prefetch / state materialize / NBD attach) alongside the
        // `fc.restore_in_jail` legs, so one trace shows where the whole
        // `host.restore_base_for_session` window goes. Pure observability.
        // ADR 0022 / ADR 0045: does this restore serve memory lazily (UFFD
        // faults / peer), or does it need a contiguous `memory.bin`
        // materialized for `load_snapshot` (File)? Lazy iff
        // `restore_memory_is_lazy_for(fresh)` — a fresh base-create under the
        // ADR-0045 substrate, or any UFFD resume — OR this is a staged
        // migration restore, which `restore_in_jail` forces to UFFD regardless
        // of `config.restore_mode`, keyed on the migration manifest's presence.
        // Compute it ONCE so the prefetch-block gate here and the
        // memory.bin-materialize gate below both mirror the actual load
        // decision and can't drift from it — or from each other. (The old
        // `if fresh` gate mis-BLOCKED a substrate base-create on a full
        // memory-manifest prefetch even though UFFD serves it lazily; and a
        // File-mode host picked as a migration dest mis-materialized a
        // `memory.bin` that `inner.restore` never reads — both on the restore
        // critical path.)
        let memory_is_lazy = self.inner.restore_memory_is_lazy_for(fresh)
            || self
                .inner
                .snapshot_path_for(metadata.id)
                .join("migration-session-manifest.json")
                .exists();

        // ADR 0043 P1: warm the memory-chunk cache before the handler faults
        // from it. A NON-LAZY restore (File base-create / File resume) feeds the
        // warmed cache to the serial `materialize_memory_if_missing` below, so
        // the prefetch stays on the critical path (awaited). A LAZY restore
        // consumes the chunks only via the handler's lazy faults AFTER
        // `inner.restore`, so warming need not block: spawn it and let restore
        // proceed immediately. The handler faults from the same cancel-safe
        // single-flight cache, so a fault that races ahead of the background
        // prefetch just fetches its one chunk itself. (Pairs with the
        // handler-side background prefault — ADR 0043 P1 / ADR 0039 #19.)
        if !memory_is_lazy {
            let prefetched_chunks = tracing::Instrument::instrument(
                self.prefetch_memory_chunks(&metadata),
                tracing::info_span!("restore.prefetch_memory"),
            )
            .await;
            if let Err(e) = prefetched_chunks.as_ref() {
                tracing::warn!(
                    error = %e,
                    snapshot_id = %metadata.id,
                    "chunked restore memory chunk prefetch failed; falling back to serial fault path",
                );
            }
        } else if let (Some(cs), Some(cache)) = (self.chunk_store.clone(), self.chunk_cache.clone())
        {
            let md = metadata.clone();
            tokio::spawn(tracing::Instrument::instrument(
                async move {
                    if let Err(e) = Self::prefetch_memory_chunks_inner(&cs, &cache, &md).await {
                        tracing::warn!(
                            error = %e,
                            snapshot_id = %md.id,
                            "background memory prefetch failed; UFFD faults serve on-demand",
                        );
                    }
                },
                tracing::info_span!("restore.prefetch_memory_bg"),
            ));
        }

        // ADR 0007 Phase 5+6: cross-host memory.bin materialization.
        // The backend owns its staging dir layout (Phase 6); we ask
        // it where this snapshot would live, then ensure the
        // memory.bin file is present before delegating to inner —
        // either because we're on the same host where it was
        // written, or because we need to rebuild it from chunks
        // (cross-host migration). With M1.13's prefetch above, the
        // serial materialize loop now hits the NVMe-warm cache for
        // every chunk.
        let src = self.inner.snapshot_path_for(metadata.id);

        // ADR 0014: cross-host materialization is order-sensitive.
        // `materialize_memory_if_missing` reads the local FC sidecar
        // (`manifest.json`) to find the memory_manifest ref before
        // it can rebuild memory.bin from chunks. On a cross-host
        // restore that sidecar is in BlobStorage, not on disk, so
        // the sidecar download (`materialize_state_if_missing`)
        // MUST run first — otherwise memory.bin materialization
        // silently no-ops and FC restore then errors with
        // "snapshot memory.bin missing".
        if let Some(chunk_store) = self.chunk_store.as_ref() {
            let blob = chunk_store.blob_storage();
            if let Err(e) = tracing::Instrument::instrument(
                materialize_state_if_missing(
                    blob.as_ref(),
                    &src,
                    metadata.state_blob_key.as_deref(),
                    metadata.sidecar_blob_key.as_deref(),
                ),
                tracing::info_span!("restore.materialize_state"),
            )
            .await
            {
                tracing::warn!(
                    error = %e,
                    src = %src.display(),
                    "state.bin/sidecar materialization failed; inner.restore will see whatever's there",
                );
            }
        }

        // ADR 0016 Phase B commit 5: rebuild chunked-disk tracking
        // for the resumed sandbox.
        //
        // Branching mirrors the cold-create path's `resolve_rootfs`:
        //
        // - Linux + nbd_pool + chunk_store + chunk_cache + a
        //   chunked `disk_manifest` on the snapshot row → take the
        //   NBD attach path. ChunkedDiskBackend is rebased on the
        //   manifest, an NBD daemon serves it at /dev/nbdN, and FC's
        //   restore reads its rootfs through that block device. The
        //   resumed sandbox enters `nbd_sandboxes` after
        //   `inner.restore` returns the new sandbox_id; the
        //   FlushScheduler spawns immediately. Closes ADR 0016 §
        //   "Phase B failure mode to close" — both the COW-diagnostic-
        //   silent symptom and the second-eviction-can't-snapshot
        //   symptom.
        //
        // - Other configurations (macOS dev, hosts without NBD wired,
        //   legacy snapshots whose `disk_manifest` is None) → fall
        //   back to `materialize_disk_if_missing`, which writes the
        //   bytes to a flat file and patches the sidecar. This path
        //   doesn't enter `nbd_sandboxes` and doesn't get a
        //   scheduler; it's the pre-Phase-B behaviour kept for
        //   backwards compatibility on non-NBD configurations.
        //
        // The pending state is captured here (pre-restore) so the
        // sidecar's `spec.rootfs_source` is patched to /dev/nbdN
        // BEFORE `inner.restore` reads the sidecar to install the
        // canonical-rootfs symlinks.
        #[cfg(target_os = "linux")]
        let pending_nbd_state = tracing::Instrument::instrument(
            self.prepare_resume_nbd_attach(&metadata, &src, fork_disk_at_attach),
            tracing::info_span!("restore.prepare_nbd"),
        )
        .await?;
        // Non-Linux has no NBD data plane — the fork decision has no seam
        // to apply to (the materialize-to-file fallback below has no
        // manifest chain).
        #[cfg(not(target_os = "linux"))]
        let _ = fork_disk_at_attach;
        // ADR 0045 C2: a staged post-copy destination arms its fetch
        // poller HERE — the earliest point the NBD handle exists. The
        // attach above used the presetup's published disk ref; the
        // poller rebases to the capture-time base + arms the sealed-
        // chunk peer overlay before landing state.bin (the FC load
        // gate), and the publisher stays fenced until the disk drain
        // makes the dest self-sufficient.
        #[cfg(target_os = "linux")]
        if let Some((_, pending)) = self.postcopy_dests.remove(&metadata.id) {
            if let Some(nbd) = pending_nbd_state.as_ref() {
                nbd.backend.set_migration_fence(true);
            }
            self.spawn_postcopy_fetch_poller(
                pending,
                self.inner.snapshot_path_for(metadata.id),
                pending_nbd_state
                    .as_ref()
                    .map(|n| (n.backend.clone(), n.device_path().to_path_buf())),
            );
        }

        // Non-NBD fallback runs only when the NBD path didn't take.
        // On macOS this is the only path; on Linux it covers hosts
        // without nbd_pool/chunk_store/chunk_cache wired or
        // snapshots with disk_manifest=None.
        let took_nbd_path: bool;
        #[cfg(target_os = "linux")]
        {
            took_nbd_path = pending_nbd_state.is_some();
        }
        #[cfg(not(target_os = "linux"))]
        {
            took_nbd_path = false;
        }
        if !took_nbd_path {
            // The 2026-07-17 corruption path (session 03e6535e): a chunked
            // snapshot whose `disk_manifest` is None (manufactured by the D5
            // silent-skip one hop upstream, or a torn checkpoint) takes NO
            // NBD attach and NO sidecar patch — so FC restores against the
            // capture-time LITERAL `/dev/nbdN` still named in the sidecar's
            // `spec.rootfs_source`. On the receiving host that device is at
            // best dead (every vda read → EIO → the guest SIGBUSes on its
            // first mmap page-in) and at worst ANOTHER session's live disk
            // (the cross-session hazard `prepare_resume_nbd_attach`'s
            // migration arm already documents). On a host that runs the NBD
            // data plane, refuse the restore rather than boot onto a
            // stale/foreign literal device: the resume op requeues, and the
            // poisoned lineage surfaces loudly for operator remediation
            // instead of silently corrupting. (The materialize-to-file
            // fallback below is only legitimate when the rootfs is NOT a
            // block device — a flat-file rootfs, macOS/dev — which the
            // sidecar reports as a non-`/dev/nbd` `rootfs_source`.) The
            // verdict is the pure `plan_resume_attach` (ADR 0098 G2 — the
            // survivor-invisibility family's resume leg, which the host
            // simulator drives).
            let sidecar_dev = read_sidecar_rootfs_source(&src).await;
            let sidecar_is_nbd_literal = sidecar_dev
                .as_deref()
                .is_some_and(|d| d.starts_with("/dev/nbd"));
            if matches!(
                engram_host_core::plan_resume_attach(
                    false,
                    self.host_runs_nbd_data_plane(),
                    sidecar_is_nbd_literal,
                ),
                engram_host_core::ResumeAttachPlan::RefuseStaleLiteral
            ) {
                let dev = sidecar_dev.expect("RefuseStaleLiteral implies a sidecar device");
                return Err(SandboxError::Snapshot(format!(
                    "resume of {} would reopen the capture-time literal rootfs device {dev} \
                     (sidecar spec.rootfs_source) because no NBD attach happened \
                     (disk_manifest={:?}) — that device is dead or owned by another \
                     session on this host. Refusing to boot onto a stale/foreign \
                     /dev/nbdN; the snapshot's disk lineage must be repaired.",
                    metadata.id, metadata.disk_manifest,
                )));
            }
            if let Some(chunk_store) = self.chunk_store.as_ref() {
                if let Err(e) = materialize_disk_if_missing(
                    chunk_store,
                    self.chunk_cache.as_ref(),
                    &src,
                    metadata.disk_manifest,
                    &self.materialize_lock,
                )
                .await
                {
                    tracing::warn!(
                        error = %e,
                        src = %src.display(),
                        "rootfs materialization failed; inner.restore will see whatever's there",
                    );
                }
            }
        }

        // ADR 0020 Route B / ADR 0022 Option A: whether to rebuild the
        // contiguous memory.bin is the SAME per-restore laziness decision the
        // prefetch gate above used (`memory_is_lazy`). A lazy restore — every
        // UFFD resume, a substrate base-create, and any staged migration
        // restore — serves memory from chunks/peer; the handler faults straight
        // from the cache the prefetch warmed, so materializing memory.bin would
        // be pure overhead (and on a migration dest, a wasted rebuild of a file
        // `inner.restore` never reads). A non-lazy File restore needs the
        // memfile present for `load_snapshot`; `materialize_memory_if_missing`
        // is an idempotent no-op once the residency prefetch (image_prefetch)
        // wrote the *per-template* file at this same snapshot-id-keyed path —
        // which keeps siblings sharing one inode rather than each rebuilding a
        // divergent copy.
        if memory_is_lazy {
            tracing::debug!(
                snapshot_id = %metadata.id,
                "lazy memory restore (UFFD); skipping memory.bin materialization",
            );
        } else if let Err(e) = self.materialize_memory_if_missing(&src).await {
            tracing::warn!(
                error = %e,
                src = %src.display(),
                "memory.bin materialization failed; inner.restore will see whatever's there",
            );
        }

        // ADR 0035 §4: every generation the snapshot pins must be staged
        // before FC's `load_snapshot` opens the embedded path. Cross-host
        // (post-roll) restores materialize from BlobStorage here; the
        // common case is a no-op (baked or prefetched).
        if !metadata.aux_bundles.is_empty() {
            if let Some(cs) = self.chunk_store.as_ref() {
                crate::bundles::BundleStore::new(
                    cs.blob_storage().clone(),
                    self.bundle_dir.clone(),
                    self.bundle_file_ext(),
                )
                .materialize_if_missing(&metadata.aux_bundles)
                .await?;
            } else {
                tracing::debug!(
                    "snapshot pins aux bundles but no chunk store is wired; \
                     relying on locally staged generations"
                );
            }
        }
        // ADR 0062: a fresh create's SELECTED mounts (the catalog harness on
        // dyn_0, per-session skills) are pins too, but they are NOT in the
        // snapshot's `aux_bundles`, so the block above doesn't cover them. The
        // baked skills/sentinel are pre-staged on the host image, but a
        // catalog-PUBLISHED generation (the harness, ADR 0062) has no local
        // stage on a host that hasn't prefetched it yet — materialize it here
        // too, or FC's `load_snapshot` opens a `dyn_*` path the host never
        // staged ("selected skill <sha> … is not staged on this host (catalog
        // materialize gap?)"). Symbolic mounts (`sha256 == None`) are resolved
        // inside the backend, not staged from blob — skip them. Empty for
        // resumes (they carry no fresh selections), so this is a no-op there.
        let selected_refs: Vec<AuxBundleRef> = selected_mounts
            .iter()
            .filter_map(|m| {
                m.sha256.as_ref().map(|sha| AuxBundleRef {
                    drive_id: m.drive_id.clone(),
                    sha256: sha.clone(),
                })
            })
            .collect();
        if !selected_refs.is_empty() {
            if let Some(cs) = self.chunk_store.as_ref() {
                crate::bundles::BundleStore::new(
                    cs.blob_storage().clone(),
                    self.bundle_dir.clone(),
                    self.bundle_file_ext(),
                )
                .materialize_if_missing(&selected_refs)
                .await?;
            }
        }
        // ADR 0035 §3: fresh creates swap aux bundles to the host's
        // current generation inside the backend; resumes keep the pin.
        //
        // Issue #223 — CANCELLATION SAFETY. On the NBD-attach path the
        // sidecar was already patched to `/dev/nbdN` (above, in
        // `prepare_resume_nbd_attach`), so the FC `inner.restore`/`restore_fresh`
        // below boots reading that device. The `NbdSandboxState` (daemon +
        // slot lease) only enters `nbd_sandboxes` AFTER the restore returns.
        // If the gRPC handler future is dropped (client deadline, coordinator
        // pod restart) in the window between the restore and the insert, the
        // local `pending_nbd_state` drops mid-flight UNDER THE LIVE FC:
        // `NbdHandle::Drop` netlink-disconnects the device the just-restored
        // guest is reading (guest parks under `dead_conn_timeout`, VM orphaned
        // — its id never delivered to anyone), and `NbdSlot::Drop` returns the
        // path to the pool. The disconnect clears the busy probe, so a
        // different session's `create` can CONNECT its own backend onto the
        // device the orphan FC still holds open → cross-session disk I/O
        // (prod canaries 5fa742b7/4391e591). `orphan_reap` is eventual and can
        // lose the race.
        //
        // Fix (mirrors the coordinator detach in #210 / the FC `ResumeOnDrop`
        // family): run the restore + the map insert inside a `tokio::spawn`ed
        // task that MOVES `pending_nbd_state` into itself, so the state always
        // lands in `nbd_sandboxes` regardless of the request's fate — then
        // normal `destroy()` teardown owns it (kill FC first, THEN disconnect).
        // The handler awaits the JoinHandle only to observe the result for the
        // connected client; a disconnect drops that await, not the work. The
        // non-NBD path (macOS, no-NBD hosts) has nothing to lose on a dropped
        // drop chain, so it stays inline.
        #[cfg(target_os = "linux")]
        if let Some(mut state) = pending_nbd_state {
            let inner = self.inner.clone();
            let nbd_sandboxes = self.nbd_sandboxes.clone();
            let publisher = self.live_manifest_publisher.clone();
            let flush_config = self.flush_config.clone();
            let abandoning = self.abandoning.clone();
            let join = tokio::spawn(async move {
                let new_id = if fresh {
                    inner.restore_fresh(metadata, selected_mounts).await?
                } else {
                    inner.restore(metadata).await?
                };
                // ADR 0016 Phase B commit 5: post-restore wiring. The new
                // sandbox_id is only known here; install it into
                // `nbd_sandboxes` together with the FlushScheduler so the
                // resumed sandbox is first-class in the COW diagnostic and
                // in the continuous-flush pipeline. Field-ordered Drop
                // ensures scheduler-cancel → NBD-disconnect → slot-release
                // on subsequent destroy.
                state.install_flush_scheduler(new_id, publisher, flush_config);
                // ADR 0019: open the resume operation window — restore-time disk
                // reads (load_snapshot + the resumed guest's working set) attach
                // `chunk.fetch` spans to this trace. Covers idle→resume AND
                // evac-dest; the coord parent trace distinguishes them. Closed by
                // `start_agent` (finish_resume_to_active calls it). Memory page-in
                // is the UFFD side, on the spawn-TRACEPARENT path.
                state.backend.operation_scope().begin("resume");
                // Issue #224: terminal-mode check. This task outlives a
                // dropped HANDLER future by design (issue #223), so it
                // also outlives a SIGTERM that cancels the handler — and
                // its `inner.restore` window is multi-second. If the
                // abandon sweep ran while we were restoring, inserting
                // here would leak a live data plane the sweep already
                // passed; process exit would then `NbdHandle::Drop` →
                // netlink-disconnect the just-restored guest's device.
                // Abandon in-place instead (kernel config persists for
                // the successor); the restore itself succeeded, so the
                // sandbox_id is still returned to the caller.
                if abandoning.load(std::sync::atomic::Ordering::SeqCst) {
                    tracing::warn!(
                        %new_id,
                        "restore completed during SIGTERM abandon; abandoning the \
                         NBD data plane in-place instead of inserting after the sweep",
                    );
                    state.abandon_for_shutdown();
                    return Ok::<SandboxId, SandboxError>(new_id);
                }
                nbd_sandboxes.insert(new_id, state);
                Ok::<SandboxId, SandboxError>(new_id)
            });
            // A JoinError here means the spawned task panicked; the FC restore
            // either never completed or panicked mid-flight — surface it as a
            // VM error (the task did NOT insert, so there is no orphan to own).
            // A cancelled HANDLER future drops THIS await, not the task.
            return join
                .await
                .map_err(|e| SandboxError::Vm(format!("restore task panicked: {e}").into()))?;
        }

        let new_id = if fresh {
            self.inner.restore_fresh(metadata, selected_mounts).await?
        } else {
            self.inner.restore(metadata).await?
        };
        Ok(new_id)
    }

    pub fn new(inner: Arc<dyn SandboxBackend>) -> Self {
        // ADR 0062: the bundle dir is the INNER backend's — never set
        // independently. The PooledBackend materializes pinned generations into
        // the exact dir the inner backend reads them from, so there's a single
        // source of truth (the backend config, set once from ENGRAM_BUNDLE_DIR /
        // the SHARED_DIR default) and no way to make them disagree.
        let bundle_dir = inner.bundle_dir().to_path_buf();
        Self {
            inner,
            image_cache: None,
            egress: None,
            session_bindings: Arc::new(DashMap::new()),
            quarantined_survivors: Arc::new(DashMap::new()),
            unreachable_guests: Arc::new(DashMap::new()),
            chunk_store: None,
            peer_health: crate::peer_fill::PeerHealth::new(),
            materialize_dir: None,
            bundle_dir,
            chunk_cache: None,
            materialize_lock: Mutex::new(()),
            materialize_scratch: None,
            materialize_gate: Arc::new(tokio::sync::Mutex::new(())),
            oci_client: None,
            nbd_pool: None,
            #[cfg(target_os = "linux")]
            nbd_sandboxes: Arc::new(DashMap::new()),
            inflight_snapshots: Arc::new(DashMap::new()),
            last_snapshot_unix_ms: Arc::new(DashMap::new()),
            checkpoint_pacing: Arc::new(DashMap::new()),
            // ADR 0016 Phase B: scheduler defaults come from env at
            // host-agent startup; the builder method
            // `with_flush_scheduler` can override.
            flush_config: crate::disk_daemon::FlushSchedulerConfig::from_env(),
            live_manifest_publisher: Arc::new(crate::disk_daemon::NoOpLiveManifestPublisher),
            live_manifest_publisher_handle: None,
            checkpoint_dir: None,
            chain_heads: None,
            checkpoint_chains: Arc::new(DashMap::new()),
            capture_locks: Arc::new(DashMap::new()),
            snapshot_waits: Arc::new(DashMap::new()),
            pending_finalizes: Arc::new(DashMap::new()),
            self_ref: Arc::new(std::sync::OnceLock::new()),
            migrations: Arc::new(crate::migration::MigrationRegistry::default()),
            inline_disk_manifests: Arc::new(DashMap::new()),
            migrate_peer: Arc::new(std::sync::OnceLock::new()),
            pending_presetups: Arc::new(DashMap::new()),
            postcopy_dests: Arc::new(DashMap::new()),
            migration_roles: Arc::new(DashMap::new()),
            #[cfg(target_os = "linux")]
            abandoning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown_manifest_publish: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            host_fs: Arc::new(engram_host_core::TokioFs),
        }
    }

    /// Whether this host runs the chunked-disk NBD data plane — the same
    /// `(nbd_pool, chunk_store, chunk_cache)` triple `prepare_resume_nbd_attach`
    /// gates the NBD attach path on. When true, a sandbox with a `/dev/nbd*`
    /// rootfs is EXPECTED to have live NBD state (an `nbd_sandboxes` entry on
    /// Linux); its absence is the post-roll-survivor corruption class the D4/D5
    /// guards refuse to snapshot/resume through. When false (macOS/dev, or a
    /// Linux host before `nbds_max` is wired) the materialize-to-file fallback
    /// is the legitimate path and a missing entry is expected.
    fn host_runs_nbd_data_plane(&self) -> bool {
        self.nbd_pool.is_some() && self.chunk_store.is_some() && self.chunk_cache.is_some()
    }

    /// ADR 0045 C2: install the page server handle (startup wiring; a
    /// second call is a startup-order bug and is ignored with a warn).
    pub fn set_migrate_peer_server(&self, server: Arc<crate::migrate_peer::PeerServer>) {
        if self.migrate_peer.set(server).is_err() {
            tracing::warn!("migrate-peer server already set; ignoring duplicate");
        }
    }

    /// ADR 0045 C2: the page server, when the host runs one.
    pub fn migrate_peer_server(&self) -> Option<Arc<crate::migrate_peer::PeerServer>> {
        self.migrate_peer.get().cloned()
    }

    /// ADR 0045 C2: a sandbox's in-flight post-copy role, if any.
    /// Lifecycle drivers (idle-evict nomination) skip roled sandboxes.
    pub fn migration_role(&self, id: SandboxId) -> Option<crate::migration::MigrationRole> {
        self.migration_roles.get(&id).map(|r| *r)
    }

    /// In-memory-only role note (reattach re-arming: the manifest
    /// already carries the role; no rewrite needed).
    pub fn note_migration_role(
        &self,
        id: SandboxId,
        role: Option<crate::migration::MigrationRole>,
    ) {
        match role {
            Some(r) => {
                self.migration_roles.insert(id, r);
            }
            None => {
                self.migration_roles.remove(&id);
            }
        }
    }

    /// Set/clear a sandbox's post-copy role, mirroring it into the FC
    /// sandbox manifest (best-effort) so a host-agent restart's
    /// reattach pass re-learns it.
    pub async fn set_migration_role(
        &self,
        id: SandboxId,
        role: Option<crate::migration::MigrationRole>,
    ) {
        self.note_migration_role(id, role);
        if let Err(e) = self
            .inner
            .set_manifest_migration_role(id, role.map(|r| r.as_str()))
            .await
        {
            tracing::warn!(sandbox_id = %id, error = %e,
                "persisting migration role to the sandbox manifest failed (reattach blind spot)");
        }
    }

    /// ADR 0045 C2: the split-brain flag of a sandbox's open export
    /// (the TTL sweep's third verdict input).
    pub fn migration_state_served(&self, id: SandboxId) -> bool {
        self.migrations.state_served(id)
    }

    /// ADR 0028 Fix A: enable checkpoint chains, rooted at `dir`
    /// (`rolling/` + `records/` subdirs are created lazily). Without
    /// this, `snapshot()` stays pure-Full and the periodic driver
    /// no-ops.
    pub fn with_checkpoint_dir(mut self, dir: PathBuf) -> Self {
        self.chain_heads = Some(Arc::new(crate::checkpoint::ChainHeadStore::new(&dir)));
        self.checkpoint_dir = Some(dir);
        self
    }

    /// Record the sandbox→session binding directly. Production
    /// populates the map via `notify_session_policy` / registration
    /// rehydration; host-local flows with no egress policy to deliver
    /// (the checkpoint integration tests) use this.
    pub fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        self.session_bindings.insert(sandbox_id, session_id);
    }

    /// The per-sandbox capture serialization lock (periodic
    /// checkpoint vs eviction/drain/SIGTERM snapshot).
    fn capture_lock(&self, id: SandboxId) -> Arc<tokio::sync::Mutex<()>> {
        self.capture_locks
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// ADR 0038 B1: is a capture (eviction / evac / drain / a prior
    /// checkpoint) currently holding this sandbox's capture lock? The
    /// periodic checkpoint driver probes this and SKIPS the tick when
    /// it returns true — a best-effort checkpoint must never *queue*
    /// behind another capture, which is how one slow/hung capture
    /// gridlocked the fleet (the 52–151 s dark `host.snapshot` waits in
    /// the 5fadd364 incident). `try_lock` is the probe: a held lock
    /// means a capture is in flight.
    ///
    /// A benign sub-ms race remains — a capture can start between this
    /// probe and `snapshot()`'s own blocking acquire, in which case the
    /// periodic tick queues behind that *one* fresh capture rather than
    /// skipping. That's harmless: the gridlock we're killing is queuing
    /// behind a HUNG capture, which the probe catches (try_lock fails)
    /// and skips outright; the next tick retries regardless.
    pub fn capture_in_flight(&self, id: SandboxId) -> bool {
        self.capture_lock(id).try_lock().is_err()
    }

    /// ADR 0028 Fix A: where un-acked durable checkpoint records live.
    /// ADR 0045 D5: the owned, 'static bundle of everything the snapshot
    /// POST phase (chunk + upload + chain bookkeeping) needs — so
    /// `snapshot_begin` can run it as a background task while the
    /// coordinator marks the session Idle. All fields are Arc-backed
    /// clones of the PooledBackend's.
    pub(crate) fn finisher(&self) -> SnapshotFinisher {
        SnapshotFinisher {
            #[cfg(target_os = "linux")]
            nbd_sandboxes: self.nbd_sandboxes.clone(),
            chunk_store: self.chunk_store.clone(),
            chunk_cache: self.chunk_cache.clone(),
            bundle_dir: self.bundle_dir.clone(),
            bundle_file_ext: self.bundle_file_ext(),
            inflight_snapshots: self.inflight_snapshots.clone(),
            last_snapshot_unix_ms: self.last_snapshot_unix_ms.clone(),
            checkpoint_pacing: self.checkpoint_pacing.clone(),
            checkpoint_chains: self.checkpoint_chains.clone(),
            checkpoint_dir: self.checkpoint_dir.clone(),
            chain_heads: self.chain_heads.clone(),
            session_bindings: self.session_bindings.clone(),
            clock: self.clock.clone(),
        }
    }

    /// ADR 0088 addendum: the deferred-finish flavor of `snapshot()`,
    /// used by the cold-base seed so its multi-GiB upload runs
    /// CONCURRENTLY with the warm hook instead of blocking it. The
    /// capture_phase (pause → dump to local NVMe → resume) stays
    /// synchronous — the FC dirty bitmap is consumed inside it, which
    /// is exactly what makes hook-dirtied pages the final Diff's dirty
    /// set — then `finish()` (disk flush upload, memory chunk upload,
    /// portable blobs, chain seed) runs on a spawned task holding the
    /// capture-lock guard, so the final snapshot's own `capture_phase`
    /// naturally serializes behind it even if the hook is instant.
    ///
    /// Callers MUST `join()` (never drop/abort) before taking another
    /// snapshot of `id` and before reporting the capture durable:
    /// - join-before-final-snapshot ⇒ the chain is seeded
    ///   (`advance_checkpoint_state` is finish()'s last step) so the
    ///   final capture stays a Diff;
    /// - aborting mid-finish could leak a snapshot dir the
    ///   `inflight_snapshots` bookkeeping no longer tracks;
    /// - a joined `Ok` is the SAME durability barrier `snapshot()`
    ///   provides, moved in wall-clock only.
    pub(crate) async fn snapshot_deferred(
        &self,
        id: SandboxId,
    ) -> Result<DeferredSnapshot, SandboxError> {
        let (capture_guard, cap) = self.capture_phase(id).await?;
        self.spawn_trace_publish(id);
        let finisher = self.finisher();
        Ok(DeferredSnapshot {
            handle: tokio::spawn(async move {
                let _guard = capture_guard;
                finisher.finish(id, cap).await
            }),
        })
    }

    /// Issue #529: install the weak self-reference `snapshot_begin`'s
    /// spawned finalize job upgrades to reach the full `destroy()` at its
    /// terminal stage. MUST be called exactly once, immediately after
    /// `Arc::new(p)` — before that, `eviction_finalizer()` builds a bundle
    /// whose `self_ref` can never upgrade, and any finalize job it drives
    /// silently skips its own destroy call (logged, not fatal — the
    /// teardown reconcile / orphan_reap backstop it, but it's a real gap).
    /// A second call is a startup-order bug; logged and ignored rather
    /// than panicking (mirrors `set_migrate_peer_server`).
    pub fn set_self_ref(&self, arc: &Arc<PooledBackend>) {
        if self.self_ref.set(Arc::downgrade(arc)).is_err() {
            tracing::warn!("PooledBackend self_ref already set; ignoring duplicate");
        }
    }

    /// Issue #529: the eviction finalize bundle, or `None` when
    /// checkpointing is disabled (`checkpoint_dir` unset) — the same gate
    /// `snapshot_begin` checks before ever constructing one.
    pub(crate) fn eviction_finalizer(&self) -> Option<crate::eviction_finalize::EvictionFinalizer> {
        let checkpoint_dir = self.checkpoint_dir.clone()?;
        Some(crate::eviction_finalize::EvictionFinalizer::new(
            self.chunk_store.clone(),
            self.chunk_cache.clone(),
            self.bundle_dir.clone(),
            self.bundle_file_ext(),
            checkpoint_dir,
            self.pending_finalizes.clone(),
            Arc::new(PooledDestroyer {
                self_ref: self.self_ref.clone(),
            }),
            self.host_fs.clone(),
            crate::eviction_finalize::max_attempts(),
        ))
    }

    /// Issue #529: re-drive every un-acked eviction finalize record at
    /// host-agent startup — the whole crash story. A host-agent pod roll
    /// mid-upload now DELAYS the commit by one restart; it cannot lose
    /// it. Called from `lib.rs` alongside the checkpoint driver spawn,
    /// with the SAME `Arc<PooledBackend>` used there (so `set_self_ref`
    /// must have run first).
    pub async fn resume_pending_finalizes(self: &Arc<Self>) {
        let Some(finalizer) = self.eviction_finalizer() else {
            return;
        };
        let records = crate::eviction_finalize::EvictionFinalizeRecord::load_all(
            self.host_fs.as_ref(),
            &finalizer.finalize_dir(),
        )
        .await;
        if records.is_empty() {
            return;
        }
        tracing::info!(
            count = records.len(),
            "re-driving eviction finalize records from disk (host-agent startup, issue #529)",
        );
        for record in records {
            finalizer
                .pending_finalizes
                .insert(record.sandbox_id, record.snapshot_id);
            metrics::counter!(crate::metrics::EVICTION_FINALIZE_REDRIVEN_TOTAL).increment(1);
            let capture_lock = self.capture_lock(record.sandbox_id);
            let f = finalizer.clone();
            tokio::spawn(async move {
                let guard = capture_lock.lock_owned().await;
                // Startup redrive: a fresh process has no hot disk bytes
                // by definition — the disk leg reads the journal.
                crate::eviction_finalize::run_eviction_finalize(f, record, guard, None).await;
            });
        }
    }

    /// Publish this sandbox's per-jail working-set trace to the blob store
    /// under the session-stable **canonical** key, so the session's NEXT
    /// resume finds it and prefaults its working set (the replay side landed
    /// with the Tier 2 `--trace-key` fix in #517).
    ///
    /// Why the host-agent publishes it (not the handler): the `engram-uffd-
    /// handler` only `put_trace`s on a clean fault-loop exit, but eviction
    /// **SIGKILLs** the handler (`engram-sandbox-firecracker` `destroy`,
    /// ADR 0044 K2 — no `kill_on_drop`) before that runs, so in prod NO trace
    /// was ever published and prefault-on-resume was inert regardless of the
    /// key. The handler *does* write the per-jail `working-set-trace.json`
    /// periodically (the ADR 0045 C2 migration rider dump); we lift that
    /// same file into the blob store at capture time, when the handler is
    /// still alive and the file is present.
    ///
    /// **Detached + best-effort:** spawned off the (latency-critical, ADR
    /// 0045 D5) capture path so a GCS `put_trace` never blocks the
    /// user-visible eviction. A no bound session / no chunk store / absent /
    /// empty / corrupt trace is a silent no-op — the handler's on-demand
    /// fault path is the backstop. Idempotent: re-publishing the same
    /// canonical key is a harmless overwrite (a session runs one place at a
    /// time, so there are no concurrent writers).
    fn spawn_trace_publish(&self, id: SandboxId) {
        let Some(session_id) = self.session_bindings.get(&id).map(|e| *e) else {
            return; // no bound session → no canonical key to publish under
        };
        let Some(trace_path) = self.working_set_trace_path(id) else {
            return; // backend has no per-jail trace (VZ / process)
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return;
        };
        tokio::spawn(async move {
            let bytes = match tokio::fs::read(&trace_path).await {
                Ok(b) => b,
                Err(_) => return, // handler never wrote it / already torn down
            };
            let trace: engram_chunk_store::working_set::WorkingSetTrace =
                match serde_json::from_slice(&bytes) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!(sandbox_id = %id, error = %e, "working-set trace parse failed; not published");
                        return;
                    }
                };
            if trace.chunks.is_empty() {
                return; // nothing to prefault — don't publish an empty trace
            }
            let n = trace.chunks.len();
            let trace_ref =
                engram_chunk_store::working_set::TraceRef::canonical(session_id.as_uuid());
            match chunk_store.put_trace(trace_ref, &trace).await {
                Ok(()) => tracing::info!(
                    sandbox_id = %id,
                    session_id = %session_id,
                    chunks = n,
                    "published session working-set trace (resume prefault)",
                ),
                Err(e) => tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "publish session working-set trace failed (best-effort)",
                ),
            }
        });
    }

    /// ADR 0045 D5: pause + drain + FC capture. Returns the held capture
    /// lock (the caller decides whether the post phase runs inline or in
    /// a background task — the lock must span it either way, so a
    /// concurrent periodic checkpoint's `capture_in_flight` try_lock
    /// keeps skipping until the upload completes) and the capture
    /// artifacts the post phase consumes.
    async fn capture_phase(
        &self,
        id: SandboxId,
    ) -> Result<(tokio::sync::OwnedMutexGuard<()>, SnapshotCapture), SandboxError> {
        let capture_lock = self.capture_lock(id);
        // ADR 0038 B0: time the lock wait — the gridlock signal. With
        // B1 periodic checkpoints skip rather than queue, so a long tail
        // here is an eviction/drain blocked on an in-flight capture.
        let lock_wait = crate::time_source::metrics_now();
        let capture_guard = capture_lock.lock_owned().await;
        metrics::histogram!(crate::metrics::SNAPSHOT_CAPTURE_LOCK_WAIT_SECONDS)
            .record(lock_wait.elapsed().as_secs_f64());
        // Diff-mode when a checkpoint chain exists: same coherent
        // (memory, disk) capture contract, O(dirty set) cost. The
        // chain seeds on the first (Full) capture below — so an
        // eviction on a long-running session is just the final diff
        // in its checkpoint chain, collapsing the multi-GiB-dump
        // window the cf4d4afd incident sat in.
        let chain_prev = self
            .checkpoint_chains
            .get(&id)
            .map(|c| (c.manifest_ref, c.manifest.clone()));

        // Write-ahead invalidate of the durable chain-head record: BOTH
        // snapshot flavors below consume+reset the KVM dirty bitmap the
        // moment FC runs `PUT /snapshot/create`, and from that instant
        // the on-disk record — the seed source for survivor rehydrate
        // after a pod roll — would describe a baseline the bitmap no
        // longer has. Remove it BEFORE the create (under the capture
        // lock, before the pause); it is re-written only after the chain
        // durably advances (`advance_checkpoint_state`). A crash
        // anywhere between leaves no record → the survivor's next
        // capture is a safe Full. The 2026-07-13 roll tore a capture in
        // exactly this window (post-processing died at 22:23:46 with the
        // bitmap consumed), which is why the coordinator's snapshots
        // rows can never be the rehydrate seed source. A failed unlink
        // (non-NotFound) aborts the capture: nothing is paused or
        // consumed yet, and proceeding would leave a stale record the
        // create is about to falsify.
        if let Some(store) = &self.chain_heads {
            store.invalidate(id).map_err(|e| {
                SandboxError::Snapshot(format!("chain-head write-ahead invalidate: {e}"))
            })?;
        }

        // ADR 0014 issue #1/#2: if a prior snapshot for this sandbox
        // was produced but never committed (caller's downstream
        // pipeline failed or the coord pod crashed between snapshot
        // and commit), tear down its artifacts BEFORE we mint a fresh
        // SnapshotId. This is the overwrite-in-place semantic that
        // keeps the host-side disk bounded under coord-side retry
        // storms — the prod incident on `engrams-fc-xngk` leaked
        // ~25 dirs × 4 GiB in 13 min because each retry minted a fresh
        // id and left the prior dir on disk.
        if self.inflight_snapshots.contains_key(&id) {
            if let Err(e) = self.abort_prior_inflight_snapshot(id).await {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "snapshot retry: best-effort abort of prior in-flight snapshot failed; \
                     proceeding with fresh attempt anyway",
                );
            }
        }

        // ADR 0021 P1.6 follow-up: gate snapshot on agentd readiness.
        //
        // The pre-flush pause below freezes vCPUs. If the guest is still
        // in early boot (kernel → engram-init → agentd start) at that
        // moment, the snapshot captures a half-initialised kernel — in
        // particular, agentd may not have completed `bind(AF_VSOCK)` —
        // and the resumed kernel comes up with a half-initialised vsock
        // driver. `panic=1 reboot=k` then trips `KVM_EXIT_SHUTDOWN`
        // ~1 s after `load_snapshot` returns; every subsequent
        // host→guest dial fails ECONNREFUSED.
        //
        // Reachable in prod via:
        //   1. SIGTERM-during-cold-boot. `shutdown.rs` checkpoints all
        //      sandboxes from `backend.list()` after a 5 s drain — a
        //      sandbox still booting at SIGTERM gets snapshotted mid-
        //      bind without this gate.
        //   2. coord-driven `snapshot(id)` fired quickly after create
        //      (any fast-rebake / probe path).
        //
        // `wait_agent_ready` blocks until agentd has dialled the host's
        // ready port (port 1027) — proving the guest's vsock stack is
        // operational end-to-end. On warm-restored sandboxes the watch
        // is pre-set true (FC's `restore_in_jail`), so this is a no-op
        // for warm re-snapshot paths. The FC backend is the only one
        // with the per-sandbox agent_ready watch; non-FC backends
        // (Process, future) return `InvalidSpec` from the default trait
        // impl, which we treat as "no readiness concept here, proceed."
        match self.inner.wait_agent_ready(id).await {
            Ok(()) => {}
            Err(SandboxError::InvalidSpec(_)) => {}
            Err(e) => return Err(SandboxError::Snapshot(format!("wait_agent_ready: {e}"))),
        }

        // ADR 0018 commit 12m + ADR 0038 B3: snapshot ordering is
        //   pause → wait_idle → flush_local(drain) → inner.snapshot
        //   → [resume] → flush_upload(GCS) (in the `post` block)
        // where inner.snapshot's internal pause/capture/resume is a
        // no-op pause (FC's PATCH /vm is idempotent) followed by the
        // memory dump and a resume that brings the VM back running.
        // This guarantees memory + disk are CAPTURED at the same point
        // in time: the explicit pause stops vCPUs, wait_idle drains any
        // in-flight virtio writes through the NBD daemon, flush_local
        // drains the just-quiesced dirty set locally, and inner.snapshot
        // then captures the page cache (which agrees with that disk
        // state). The disk GCS upload is deferred to flush_upload AFTER
        // resume — off the frozen-guest path — but the captured *content*
        // is fixed at the drain, so coherence is unchanged.
        //
        // Pre-12m ordering was flush → inner.snapshot (FC's pause
        // happened inside the inner call AFTER our flush). Writes
        // queued between flush and pause landed in memory but not
        // the drained disk set — the cross-host evac canary md5
        // mismatch on dev-vm validation was exactly this race.
        //
        // ADR 0028 A.log: the pause instant is the coherence cut for
        // the (memory, disk, event-log) triple — the coord resolves
        // the session_events cursor as "last event at or before this"
        // when it records the checkpoint.
        let paused_at = self.clock.now_utc();
        self.inner
            .pause(id)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("pre-flush pause: {e}")))?;

        // Issue #202: arm the unwind guard the instant the guest is
        // paused. Unlike the migration paths this path raises no fence,
        // and `inner.snapshot` below resumes the guest on success — but
        // if `inner.snapshot` returns `Err` (FC `PUT /snapshot/create`
        // can hang 60 s then fail) or the handler future is cancelled
        // here, the guest stays paused and the drained dirty buffer is
        // lost. The guard owns the drained `PendingDiskFlush` and rides
        // inside the returned `SnapshotCapture` until the finisher's
        // `flush_upload` takes it over (covering the cancellation gap in
        // `snapshot_begin` between this return and the finisher spawn).
        let mut unwind = CaptureUnwind::new(self.inner.clone(), id);
        unwind.arm();

        // ADR 0038 B3: under the pause, only DRAIN the dirty buffer
        // (+ hash + stash in the backend's pending tier) — the
        // multi-second GCS upload is deferred to `flush_upload` after
        // the guest resumes (the `post` block below), so the
        // guest-visible pause is O(local copy), not O(GCS). The drain
        // still captures disk-at-the-pause-instant (ADR 0018 §12m); the
        // memory capture below is paired with it.
        #[cfg(target_os = "linux")]
        {
            // INVARIANT (see `nbd_sandboxes`): clone the Arc + copy the
            // device path out of the guard, then drop it BEFORE any
            // `.await` — a held guard across the drain/upload below
            // parks contending `destroy`/`create` workers on the
            // shard's sync RwLock and can deadlock the runtime.
            let backend_dev = self
                .nbd_sandboxes
                .get(&id)
                .map(|entry| (entry.backend.clone(), entry.device_path().to_path_buf()));
            if let Some((backend, dev)) = backend_dev {
                // Push the HOST's block-device page cache down to the
                // daemon BEFORE draining. FC's virtio-blk writes to
                // /dev/nbdN through the kernel page cache (drive
                // cache_type = Unsafe: guest FLUSH does not propagate), so
                // without this fsync the drain captures only what
                // background writeback (~30 s) happened to deliver — a
                // session that wrote recently snapshots a TORN chunk (the
                // delivered front + a stale tail). The migration captures
                // each carried this fsync already; the standard pipeline
                // (periodic checkpoint / idle-evict / rehome) relied on
                // "quiescent sessions age past the writeback interval",
                // which the disk post-copy canary disproved: write → evac
                // 15 s later published chunk 34 with its last 56 KiB
                // reverted, and a periodic checkpoint of an actively-
                // writing guest has the same hole (recovery from it would
                // be corrupt).
                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&dev)?;
                    f.sync_all()
                })
                .await
                .map_err(|e| SandboxError::Snapshot(format!("nbd host-cache flush join: {e}")))?
                .map_err(|e| SandboxError::Snapshot(format!("nbd host-cache flush: {e}")))?;
                // Drain in-flight NBD requests so the drain sees a quiescent
                // dirty buffer. With FC paused above, no new virtio writes
                // are issued, and wait_idle returns once already-in-flight
                // requests have completed through backend.write().
                backend.wait_idle().await;
                let pending = backend
                    .flush_local()
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("nbd disk drain: {e}")))?;
                // Issue #202: the dirty buffer now lives only in
                // `pending` — move it into the guard so any error/cancel
                // before the finisher takes over re-queues it (and
                // resumes the guest). `fenced` stays false: this path
                // never raises the migration fence.
                unwind.disk_backend = Some(backend);
                unwind.disk_pending = Some(pending);
            } else {
                // The 2026-07-17 corruption path (session 03e6535e): a
                // sandbox with an NBD-backed rootfs but NO `nbd_sandboxes`
                // entry is a post-pod-roll survivor whose in-pod NBD server
                // died with the old host-agent and was never rehydrated
                // (the #739 family). Silently skipping the drain here
                // records a snapshot with `disk_manifest=None` +
                // `recoverable=true` — dropping EVERY acked disk write of
                // the session and poisoning its lineage (the next resume
                // then boots onto a literal /dev/nbdN — see
                // `prepare_resume_nbd_attach`'s D4 guard). The verdict is
                // the pure `plan_capture_disk_drain` (ADR 0098 G2 — the
                // survivor-invisibility family's capture leg, which the
                // host simulator drives): refuse rather than skip; the
                // error requeues the eviction op (redrive-safe). The skip
                // stays correct for a legitimately non-NBD rootfs
                // (macOS/dev/flat-file), which `rootfs_device` reports as
                // `None`.
                let rootfs_dev = self.inner.rootfs_device(id);
                let rootfs_is_nbd = rootfs_dev
                    .as_ref()
                    .is_some_and(|d| d.to_string_lossy().starts_with("/dev/nbd"));
                if matches!(
                    engram_host_core::plan_capture_disk_drain(
                        false,
                        self.host_runs_nbd_data_plane(),
                        rootfs_is_nbd,
                    ),
                    engram_host_core::CaptureDrainPlan::RefuseUntracked
                ) {
                    let dev = rootfs_dev.expect("RefuseUntracked implies an nbd rootfs device");
                    return Err(SandboxError::Snapshot(format!(
                        "sandbox {id} has an NBD-backed rootfs ({}) but no nbd_sandboxes \
                         entry — a post-roll survivor whose disk server is gone. Refusing to \
                         snapshot with disk_manifest=None (would drop the session's acked \
                         disk writes and poison its lineage); the session must be rehydrated \
                         or evicted-locally first.",
                        dev.display(),
                    )));
                }
            }
        }

        // ADR 0007 Phase 6: backend owns its staging dir; we look
        // it up via snapshot_path_for after the inner call so we
        // can read/patch the on-disk artifacts the inner backend
        // wrote (memory.bin / memory.diff, manifest.json). The inner
        // call's pause-create-resume cycle is idempotent against our
        // earlier pause and brings the VM back to running on exit —
        // for the diff flavor that resume lands after O(dirty set),
        // not O(guest RAM).
        // ADR 0038 B0: time the FC memory capture (`PUT /snapshot/
        // create`) — the previously-invisible step that hung 60 s on the
        // cold Full seed. After B2, `type="full"` should vanish on the
        // resume path (chain seeded → diff).
        let snap_type = if chain_prev.is_some() { "diff" } else { "full" };
        let create_start = crate::time_source::metrics_now();
        let create_res = if chain_prev.is_some() {
            self.inner.snapshot_diff(id).await
        } else {
            self.inner.snapshot(id).await
        };
        metrics::histogram!(
            crate::metrics::SNAPSHOT_CREATE_SECONDS,
            "type" => snap_type,
            "outcome" => if create_res.is_ok() { "success" } else { "error" },
        )
        .record(create_start.elapsed().as_secs_f64());
        let metadata = match create_res {
            Ok(m) => m,
            Err(e) => {
                // A failed Diff create may still have consumed the dirty
                // bitmap (FC reads+resets it before/while writing the
                // file; mid-write failure semantics are undefined).
                // Conservative: poison. Worst case is one unnecessary
                // Full; the alternative is silent memory corruption.
                if chain_prev.is_some() {
                    poison_checkpoint_chain_after_failed_diff(
                        &self.checkpoint_chains,
                        self.chain_heads.as_deref(),
                        id,
                        "fc snapshot_diff create",
                    );
                }
                return Err(e);
            }
        };
        let dest = self.inner.snapshot_path_for(metadata.id);
        // `create_res` was Ok ⟹ `inner.snapshot`/`snapshot_diff` brought
        // the guest back running. The guard rides into `SnapshotCapture`
        // (still armed) so the cancellation gap in `snapshot_begin` and
        // any failure before the finisher consumes it stays covered; the
        // finisher takes the pending out of the guard and defuses it.
        Ok((
            capture_guard,
            SnapshotCapture {
                metadata,
                dest,
                chain_prev,
                paused_at,
                unwind,
            },
        ))
    }

    /// ADR 0045 C2 (E2B fold): read the sandbox's per-jail working-set
    /// trace (fault-order hot set) for the migration rider. Best-effort
    /// by design: an absent/corrupt file (handler predates the dump,
    /// guest never faulted, bake-profile override) returns empty.
    fn read_hot_chunks(&self, id: SandboxId) -> Vec<[u8; 32]> {
        let Some(path) = self.inner.working_set_trace_path(id) else {
            return Vec::new();
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Vec::new();
        };
        match serde_json::from_slice::<engram_chunk_store::working_set::WorkingSetTrace>(&bytes) {
            Ok(trace) => trace.chunks.iter().map(|h| *h.as_bytes()).collect(),
            Err(e) => {
                tracing::debug!(sandbox_id = %id, error = %e, "hot-set trace parse failed; empty rider");
                Vec::new()
            }
        }
    }

    /// ADR 0045 C2 (E2B fold): order a pull set hot-first — chunks in
    /// the source's fault-order hot set come first (in that order),
    /// the rest keep their manifest order behind them.
    fn order_hot_first(
        remaining: Vec<engram_chunk_store::manifest::ChunkHash>,
        hot: &[[u8; 32]],
    ) -> Vec<engram_chunk_store::manifest::ChunkHash> {
        if hot.is_empty() || remaining.is_empty() {
            return remaining;
        }
        let rank: std::collections::HashMap<&[u8; 32], usize> =
            hot.iter().enumerate().map(|(i, h)| (h, i)).collect();
        let mut remaining = remaining;
        // Stable: non-hot chunks keep their relative manifest order.
        remaining.sort_by_key(|h| rank.get(h.as_bytes()).copied().unwrap_or(usize::MAX));
        remaining
    }

    /// ADR 0095: the peer-hinted resume pre-pass — land this snapshot's
    /// locally-missing chunk set (memory + disk session manifests,
    /// `contains_on_disk`-filtered, which also elides the pinned image
    /// base) from the hinted sibling before the guest resumes. Wholly
    /// best-effort: any shortfall simply leaves those chunks to the
    /// fault path, which resolves local → GCS exactly as before this
    /// ADR. No hot-first rider here — the coordinator has no
    /// working-set trace for an ordinary resume (the per-host traces
    /// are dest-local and this dest never ran the session), so
    /// manifest order stands, memory first (the wake-up set lives
    /// there).
    async fn peer_resume_prepass(&self, metadata: &SnapshotMetadata) {
        use engram_protocol::grpc_client::PeerChunkScope;
        let (Some(store), Some(cache)) = (self.chunk_store.clone(), self.chunk_cache.clone())
        else {
            return;
        };
        // ADR 0101 A: fetch the two manifests concurrently; `want` still
        // extends memory-first (the wake-up set lives there — see the
        // no-hot-first note above).
        async fn fetch_manifest(
            store: engram_chunk_store::ChunkStore,
            manifest_ref: Option<engram_core::types::manifest::ManifestRef>,
        ) -> Result<
            Option<engram_chunk_store::manifest::Manifest>,
            (
                engram_core::types::manifest::ManifestRef,
                engram_chunk_store::ChunkStoreError,
            ),
        > {
            match manifest_ref {
                None => Ok(None),
                Some(r) => store.get_manifest(r).await.map(Some).map_err(|e| (r, e)),
            }
        }
        let (mem_res, disk_res) = tokio::join!(
            fetch_manifest(store.clone(), metadata.memory_manifest),
            fetch_manifest(store.clone(), metadata.disk_manifest)
        );
        let mut want: Vec<engram_chunk_store::manifest::ChunkHash> = Vec::new();
        for res in [mem_res, disk_res] {
            match res {
                Ok(Some(m)) => want.extend(m.chunks.iter().map(|c| c.hash)),
                Ok(None) => {}
                Err((manifest_ref, e)) => {
                    tracing::warn!(
                        ?manifest_ref,
                        error = %e,
                        "peer resume pre-pass: manifest load failed; skipping tier",
                    );
                    return;
                }
            }
        }
        let mut missing = Vec::with_capacity(want.len());
        let mut seen = std::collections::HashSet::with_capacity(want.len());
        for h in want {
            if seen.insert(h) && !cache.contains_on_disk(h) {
                missing.push(h);
            }
        }
        if missing.is_empty() {
            return; // affinity-host resume: everything already local
        }
        let scope = PeerChunkScope::Snapshot(metadata.id);
        let total = missing.len();
        let started = crate::time_source::metrics_now();
        for addr in metadata.peer_hints.iter().take(2) {
            let stats = crate::peer_fill::pull_chunks_from_peer(
                addr,
                scope.clone(),
                &missing,
                &cache,
                &self.peer_health,
            )
            .await;
            tracing::info!(
                snapshot_id = %metadata.id,
                peer = %addr,
                landed = stats.landed,
                landed_bytes = stats.landed_bytes,
                missing_on_peer = stats.missing,
                failed = stats.failed,
                backpressure = stats.backpressure,
                of = total,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "peer resume pre-pass window complete (ADR 0095)",
            );
            if !stats.failed {
                return;
            }
            missing.retain(|h| !cache.contains_on_disk(*h));
            if missing.is_empty() {
                return;
            }
        }
    }

    /// ADR 0045 C1: pull a chunk set from a live migration export over
    /// the source host's gRPC channel into the local cache. Used by the
    /// background divergence pull (LAN beats GCS by ~20x for the session
    /// chain). Returns the number of chunks landed.
    async fn pull_chunks_from_source(
        source_addr: &str,
        export_id: &str,
        hashes: &[engram_chunk_store::manifest::ChunkHash],
        cache: &ChunkCache,
    ) -> Result<usize, SandboxError> {
        use engram_core::types::snapshot::MigrationItem;
        let channel = tonic::transport::Endpoint::from_shared(source_addr.to_string())
            .map_err(|e| SandboxError::InvalidSpec(format!("bad source_addr: {e}")))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| SandboxError::Snapshot(format!("dial source: {e}")))?;
        let source = engram_protocol::grpc_client::GrpcHostClient::new(channel);
        // N concurrent streams: one ordered gRPC stream moves ~20 MB/s
        // (frame channel depth x 1 MiB frames); the divergent set is
        // hundreds of MB and sits on the RESTORE critical path now, so
        // fan out over batches to reach LAN line rate.
        const STREAMS: usize = 8;
        use futures::StreamExt;
        let batch = hashes.len().div_ceil(STREAMS).max(1);
        let mut tasks = Vec::new();
        for chunk_hashes in hashes.chunks(batch) {
            let source = source.clone();
            let cache = cache.clone();
            let export_id = export_id.to_string();
            let chunk_hashes = chunk_hashes.to_vec();
            tasks.push(tokio::spawn(async move {
                let items: Vec<MigrationItem> = chunk_hashes
                    .iter()
                    .map(|h| MigrationItem::Chunk(*h.as_bytes()))
                    .collect();
                let mut stream = source
                    .migration_fetch(&export_id, items)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("divergence fetch: {e}")))?;
                let mut current: Vec<u8> = Vec::new();
                let mut current_idx: Option<u32> = None;
                let mut landed = 0usize;
                while let Some(frame) = stream.next().await {
                    let frame =
                        frame.map_err(|e| SandboxError::Snapshot(format!("fetch frame: {e}")))?;
                    if current_idx != Some(frame.item_idx) {
                        current_idx = Some(frame.item_idx);
                        current.clear();
                    }
                    current.extend_from_slice(&frame.data);
                    if frame.last {
                        let idx = frame.item_idx as usize;
                        let hash = chunk_hashes.get(idx).ok_or_else(|| {
                            SandboxError::Snapshot("divergence fetch: unknown item index".into())
                        })?;
                        cache.put_no_evict(*hash, &current).await.map_err(|e| {
                            SandboxError::Snapshot(format!("land chunk {hash}: {e}"))
                        })?;
                        // Review finding 3: this loop (the synchronous
                        // pre-resume divergence pull, ADR 0045 C1) landed
                        // chunks into the same local cache as
                        // `migration_prestage`'s loop but left them
                        // uncounted — undercounting peer-fill volume on
                        // every warm migration resume.
                        count_peer_chunk_fill(current.len());
                        landed += 1;
                        current = Vec::new();
                        current_idx = None;
                    }
                }
                Ok::<usize, SandboxError>(landed)
            }));
        }
        let mut landed = 0usize;
        for t in tasks {
            landed += t
                .await
                .map_err(|e| SandboxError::Snapshot(format!("pull task join: {e}")))??;
        }
        if let Err(e) = cache.sweep().await {
            tracing::warn!(error = %e, "post-pull cache sweep failed (non-fatal)");
        }
        Ok(landed)
    }

    /// ADR 0045 C2 (destination): stage the presetup's restore package
    /// — the sidecar, the inline session manifest, and the peer spec —
    /// then arm the fetch poller (spawned inside `restore_with` once
    /// the NBD handle exists). No export artifacts exist yet: the
    /// source hasn't paused.
    async fn postcopy_stage(
        &self,
        metadata: &SnapshotMetadata,
        mig: &engram_core::types::snapshot::MigrationSourceInfo,
    ) -> Result<(), SandboxError> {
        let (Some(peer_addr), Some(peer_token)) = (&mig.peer_addr, &mig.peer_token) else {
            return Err(SandboxError::InvalidSpec(
                "post-copy rider missing peer_addr/peer_token".into(),
            ));
        };
        if mig.sidecar_json.is_empty() {
            return Err(SandboxError::InvalidSpec(
                "post-copy rider missing the sidecar".into(),
            ));
        }
        let dest = self.inner.snapshot_path_for(metadata.id);
        fs::create_dir_all(&dest)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("create snapshot dir: {e}")))?;
        fs::write(dest.join("manifest.json"), &mig.sidecar_json)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("write sidecar: {e}")))?;
        fs::write(
            dest.join("migration-session-manifest.json"),
            &mig.memory_manifest_json,
        )
        .await
        .map_err(|e| SandboxError::Snapshot(format!("write session manifest: {e}")))?;
        let peer_spec = engram_sandbox_firecracker::MigrationPeerSpec {
            peer_addr: peer_addr.clone(),
            export_id: mig.export_id.clone(),
            peer_token: peer_token.clone(),
            hot_chunks: mig.hot_chunks.clone(),
        };
        fs::write(
            dest.join(engram_sandbox_firecracker::MIGRATION_PEER_FILE),
            serde_json::to_vec(&peer_spec)
                .map_err(|e| SandboxError::Snapshot(format!("peer spec json: {e}")))?,
        )
        .await
        .map_err(|e| SandboxError::Snapshot(format!("write peer spec: {e}")))?;
        self.postcopy_dests.insert(
            metadata.id,
            PostCopyDestPending {
                source_addr: mig.source_addr.clone(),
                export_id: mig.export_id.clone(),
            },
        );
        Ok(())
    }

    /// ADR 0045 C2 (destination): the fetch poller. Once the source's
    /// capture lands (the export starts answering), pull the disk
    /// SEAL descriptor, rebase the NBD attach to the source's
    /// published base manifest + arm the peer overlay (sealed chunks
    /// demand-fault from the frozen source's RAM), spawn the
    /// background drain, then write `state.bin` LAST — its appearance
    /// is the FC load gate. No chunk staging, no uploads: the drain
    /// unfences the publisher when the disk is self-sufficient, and
    /// durability rides the dest's normal flush cadence from there.
    #[cfg(target_os = "linux")]
    fn spawn_postcopy_fetch_poller(
        &self,
        pending: PostCopyDestPending,
        dest_dir: std::path::PathBuf,
        nbd: Option<(
            Arc<crate::disk_daemon::ChunkedDiskBackend>,
            std::path::PathBuf,
        )>,
    ) {
        use engram_core::types::snapshot::MigrationItem;
        tokio::spawn(async move {
            let budget = std::time::Duration::from_secs(240);
            let started = crate::time_source::metrics_now();
            let tmp = dest_dir.join("postcopy-tmp");
            let _ = fs::create_dir_all(&tmp).await;

            // 1. Poll until the export serves (capture done). Dial
            //    ONCE and poll tight over the live channel — the old
            //    250 ms sleep (plus a fresh TCP+HTTP/2 handshake per
            //    attempt) was a pure blackout tax whenever the
            //    pre-stage finished before the capture; a failed
            //    attempt is a sub-ms NotFound on the pod network.
            let mut source: Option<engram_protocol::grpc_client::GrpcHostClient> = None;
            let (seal_path, state_tmp, fetch_ms) = loop {
                if started.elapsed() > budget {
                    tracing::error!(
                        export_id = %pending.export_id,
                        "post-copy fetch poller timed out; the FC load gate will tear the restore down",
                    );
                    return;
                }
                let client = match &source {
                    Some(c) => c,
                    None => match Self::dial_migration_source(&pending.source_addr).await {
                        Ok(c) => source.insert(c),
                        Err(e) => {
                            tracing::debug!(error = %e, "post-copy source dial failed; retrying");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                    },
                };
                let t_fetch = crate::time_source::metrics_now();
                match Self::fetch_export_items(
                    client,
                    &pending.export_id,
                    vec![MigrationItem::DiskSealInfo, MigrationItem::StateBin],
                    &tmp,
                )
                .await
                {
                    Ok(()) => {
                        break (
                            tmp.join("disk-seal.json"),
                            tmp.join("state.bin"),
                            t_fetch.elapsed().as_millis() as u64,
                        )
                    }
                    Err(e) => {
                        // A transport-level failure poisons the cached
                        // channel; NotFound (export not open yet) does
                        // not.
                        if !matches!(e, SandboxError::NotFound) {
                            tracing::debug!(error = %e, "post-copy fetch not ready; retrying");
                            source = None;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    }
                }
            };

            // 2. Parse the seal; rebase NBD to the source's published
            //    base + arm the overlay; spawn the drain. Every error
            //    arm here returns WITHOUT landing state.bin — the FC
            //    load gate times out → NeverLoaded → the coordinator
            //    aborts to the source, zero loss. Never resume on a
            //    disk that could be stale.
            #[derive(serde::Deserialize)]
            struct SealInfo {
                sealed_chunk_indices: Vec<u64>,
                manifest: Option<engram_chunk_store::Manifest>,
                disk_ref_bincode: Option<Vec<u8>>,
            }
            let info: SealInfo = match fs::read(&seal_path).await {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::error!(error = %e,
                            "post-copy disk seal parse failed; NOT landing state.bin");
                        return;
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e,
                        "post-copy disk seal read failed; NOT landing state.bin");
                    return;
                }
            };
            match (&nbd, info.manifest) {
                (Some((nbd, device)), Some(manifest)) => {
                    let Some(rref) = info
                        .disk_ref_bincode
                        .as_deref()
                        .and_then(|b| bincode::deserialize(b).ok())
                    else {
                        tracing::error!(
                            "post-copy disk seal missing/invalid ref; NOT landing state.bin"
                        );
                        return;
                    };
                    let fetcher = Arc::new(GrpcPostCopyDiskFetcher::new(
                        pending.source_addr.clone(),
                        pending.export_id.clone(),
                    ));
                    match nbd
                        .install_postcopy_overlay(
                            &manifest,
                            rref,
                            &info.sealed_chunk_indices,
                            fetcher,
                        )
                        .await
                    {
                        Ok(_subscription) => {
                            tracing::info!(
                                export_id = %pending.export_id,
                                sealed = info.sealed_chunk_indices.len(),
                                "post-copy disk overlay armed (sealed chunks demand-fault P2P)",
                            );
                            nbd.clone().spawn_postcopy_drain();
                        }
                        Err(e) => {
                            tracing::error!(error = %e,
                                "post-copy disk overlay install failed; NOT landing state.bin");
                            return;
                        }
                    }
                    // Drop the device's host page cache (BLKFLSBUF):
                    // the kernel's udev/partition probe read the first
                    // blocks at CONNECT time — before the overlay — so
                    // the page cache may hold PRE-divergence bytes for
                    // sealed chunks (the ext4 superblock region is
                    // near-always sealed on a live session). FC reads
                    // /dev/nbdN through that cache; a stale superblock
                    // served to the resumed guest is the corruption
                    // class this line exists for.
                    if let Err(e) = crate::disk_daemon::flush_block_device_cache(device) {
                        tracing::error!(error = %e, device = %device.display(),
                            "BLKFLSBUF failed; NOT landing state.bin (stale-probe risk)");
                        return;
                    }
                }
                (None, Some(_)) if !info.sealed_chunk_indices.is_empty() => {
                    // The source sealed dirty disk but this host has
                    // no NBD data plane to overlay it on — resuming
                    // would run on a silently-stale rootfs.
                    tracing::error!(
                        sealed = info.sealed_chunk_indices.len(),
                        "post-copy disk seal present but no NBD backend; NOT landing state.bin",
                    );
                    return;
                }
                _ => {
                    // No chunked disk on the source — nothing to
                    // overlay; release the publish fence armed at
                    // attach (nothing gates durability).
                    if let Some((nbd, _)) = &nbd {
                        nbd.set_migration_fence(false);
                    }
                }
            }

            // 3. state.bin LAST — this opens the FC load gate.
            if let Err(e) = fs::rename(&state_tmp, dest_dir.join("state.bin")).await {
                tracing::error!(error = %e, "post-copy state.bin land failed");
                return;
            }
            tracing::info!(
                export_id = %pending.export_id,
                elapsed_ms = started.elapsed().as_millis() as u64,
                fetch_ms,
                "post-copy restore inputs staged (state.bin landed)",
            );
        });
    }

    /// Await the disk drain's terminal outcome. `None` subscription =
    /// no overlay (never a disk post-copy, or the drain already
    /// completed and cleared it) = nothing to wait for.
    #[cfg(target_os = "linux")]
    async fn await_disk_drain(
        sub: Option<crate::disk_daemon::PostCopyDrainSubscription>,
    ) -> Result<u64, String> {
        let Some(mut rx) = sub else { return Ok(0) };
        let wait = async {
            loop {
                if let Some(result) = rx.borrow_and_update().clone() {
                    return result;
                }
                if rx.changed().await.is_err() {
                    // Sender dropped without a terminal value — the
                    // drain task died. Check the final state once.
                    return rx
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| Err("disk drain task dropped".into()));
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(600), wait)
            .await
            .map_err(|_| "disk drain timed out (600s)".to_string())?
    }

    /// Dial a migration source's gRPC endpoint.
    #[cfg(target_os = "linux")]
    async fn dial_migration_source(
        source_addr: &str,
    ) -> Result<engram_protocol::grpc_client::GrpcHostClient, SandboxError> {
        let channel = tonic::transport::Endpoint::from_shared(source_addr.to_string())
            .map_err(|e| SandboxError::InvalidSpec(format!("bad source_addr: {e}")))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| SandboxError::Snapshot(format!("dial migration source: {e}")))?;
        Ok(engram_protocol::grpc_client::GrpcHostClient::new(channel))
    }

    /// Fetch named export items into `dir` (each written under its
    /// canonical filename). One-shot; errors when the export isn't
    /// open yet (the poller's retry signal). Takes a CONNECTED client
    /// — the poller reuses one channel across attempts instead of a
    /// TCP+HTTP/2 handshake per poll.
    #[cfg(target_os = "linux")]
    async fn fetch_export_items(
        source: &engram_protocol::grpc_client::GrpcHostClient,
        export_id: &str,
        items: Vec<engram_core::types::snapshot::MigrationItem>,
        dir: &std::path::Path,
    ) -> Result<(), SandboxError> {
        use engram_core::types::snapshot::MigrationItem;
        let item_specs = items.clone();
        let mut stream = source
            .migration_fetch(export_id, items)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("migration fetch: {e}")))?;
        use futures::StreamExt;
        let mut current: Vec<u8> = Vec::new();
        let mut current_idx: Option<u32> = None;
        while let Some(frame) = stream.next().await {
            let frame = frame.map_err(|e| SandboxError::Snapshot(format!("fetch frame: {e}")))?;
            if current_idx != Some(frame.item_idx) {
                if !current.is_empty() {
                    return Err(SandboxError::Snapshot(
                        "migration fetch: out-of-order frame".into(),
                    ));
                }
                current_idx = Some(frame.item_idx);
            }
            current.extend_from_slice(&frame.data);
            if frame.last {
                let idx = frame.item_idx as usize;
                let name = match item_specs.get(idx) {
                    Some(MigrationItem::StateBin) => "state.bin",
                    Some(MigrationItem::Sidecar) => "manifest.json",
                    Some(MigrationItem::DiskManifest) => "disk-manifest.json",
                    Some(MigrationItem::DiskSealInfo) => "disk-seal.json",
                    _ => {
                        return Err(SandboxError::Snapshot(
                            "fetch_export_items: unexpected item".into(),
                        ));
                    }
                };
                fs::write(dir.join(name), &current)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("write {name}: {e}")))?;
                current = Vec::new();
                current_idx = None;
            }
        }
        Ok(())
    }

    /// ADR 0045 C1 (destination): pull the frozen source's export —
    /// state.bin + sidecar into the local snapshot dir, every transfer
    /// chunk into the NVMe cache (hash-verified by `cache.put`), the
    /// inline session manifest as a local file the handler resolves
    /// from disk, and the inline disk manifest staged for
    /// `prepare_resume_nbd_attach`.
    async fn migration_prestage(
        &self,
        metadata: &SnapshotMetadata,
        mig: &engram_core::types::snapshot::MigrationSourceInfo,
    ) -> Result<(), SandboxError> {
        use engram_core::types::snapshot::MigrationItem;
        let Some(cache) = self.chunk_cache.clone() else {
            return Err(SandboxError::InvalidSpec(
                "migration restore needs a chunk cache".into(),
            ));
        };
        let dest = self.inner.snapshot_path_for(metadata.id);
        fs::create_dir_all(&dest)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("create snapshot dir: {e}")))?;

        let channel = tonic::transport::Endpoint::from_shared(mig.source_addr.clone())
            .map_err(|e| SandboxError::InvalidSpec(format!("bad source_addr: {e}")))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| SandboxError::Snapshot(format!("dial migration source: {e}")))?;
        let source = engram_protocol::grpc_client::GrpcHostClient::new(channel);

        let mut items = vec![MigrationItem::StateBin, MigrationItem::Sidecar];
        items.extend(
            mig.new_memory_chunk_hashes
                .iter()
                .map(|h| MigrationItem::Chunk(*h)),
        );
        items.extend(
            mig.new_disk_chunk_hashes
                .iter()
                .map(|h| MigrationItem::Chunk(*h)),
        );
        let item_specs = items.clone();
        let mut stream = source
            .migration_fetch(&mig.export_id, items)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("migration fetch: {e}")))?;

        use futures::StreamExt;
        let mut current: Vec<u8> = Vec::new();
        let mut current_idx: Option<u32> = None;
        while let Some(frame) = stream.next().await {
            let frame = frame.map_err(|e| SandboxError::Snapshot(format!("fetch frame: {e}")))?;
            if current_idx != Some(frame.item_idx) {
                if current_idx.is_some() && !current.is_empty() {
                    return Err(SandboxError::Snapshot(
                        "migration fetch: item switched before its last frame".into(),
                    ));
                }
                current_idx = Some(frame.item_idx);
                current.clear();
            }
            if frame.offset != current.len() as u64 {
                return Err(SandboxError::Snapshot(
                    "migration fetch: out-of-order frame".into(),
                ));
            }
            current.extend_from_slice(&frame.data);
            if frame.last {
                let idx = frame.item_idx as usize;
                let spec = item_specs.get(idx).ok_or_else(|| {
                    SandboxError::Snapshot("migration fetch: unknown item index".into())
                })?;
                match spec {
                    MigrationItem::StateBin => {
                        fs::write(dest.join("state.bin"), &current)
                            .await
                            .map_err(|e| SandboxError::Snapshot(format!("write state.bin: {e}")))?;
                    }
                    MigrationItem::Sidecar => {
                        fs::write(dest.join("manifest.json"), &current)
                            .await
                            .map_err(|e| SandboxError::Snapshot(format!("write sidecar: {e}")))?;
                    }
                    MigrationItem::DiskManifest => {
                        fs::write(dest.join("disk-manifest.json"), &current)
                            .await
                            .map_err(|e| {
                                SandboxError::Snapshot(format!("write disk manifest: {e}"))
                            })?;
                    }
                    MigrationItem::Chunk(h) => {
                        let hash = engram_chunk_store::manifest::ChunkHash::from_bytes(*h);
                        // No per-write sweep (the prod canary's 70s
                        // prestage was ~63 sweeps of the full NVMe
                        // cache); the batch-closing sweep runs after
                        // the stream below.
                        cache.put_no_evict(hash, &current).await.map_err(|e| {
                            SandboxError::Snapshot(format!("stage chunk {hash}: {e}"))
                        })?;
                        // ADR 0019 / telemetry restoration (#526): the
                        // peer half of the peer-vs-GCS fill split — this
                        // chunk filled the local cache from the migration
                        // SOURCE host, not BlobStorage. Baseline meter for
                        // epic-gcs-free-resume's "GCS-free by policy"
                        // claim (the `source="gcs"` half lives at
                        // `engram-chunk-store::cache`'s leader-persist arm).
                        count_peer_chunk_fill(current.len());
                    }
                    // C1 prestage never requests the post-copy disk
                    // items (those ride the C2 fetch poller).
                    MigrationItem::DiskSealInfo | MigrationItem::DiskChunkAt(_) => {
                        return Err(SandboxError::Snapshot(
                            "migration prestage: unexpected disk post-copy item".into(),
                        ));
                    }
                }
                current = Vec::new();
                current_idx = None;
            }
        }

        if let Err(e) = cache.sweep().await {
            tracing::warn!(error = %e, "post-prestage cache sweep failed (non-fatal)");
        }

        // Stage the inline manifests: the session (memory) manifest as
        // the local file `restore_in_jail` auto-detects; the disk
        // manifest for the NBD attach.
        fs::write(
            dest.join("migration-session-manifest.json"),
            &mig.memory_manifest_json,
        )
        .await
        .map_err(|e| SandboxError::Snapshot(format!("write migration manifest: {e}")))?;
        if !mig.disk_manifest_json.is_empty() {
            let disk: engram_chunk_store::Manifest =
                serde_json::from_slice(&mig.disk_manifest_json)
                    .map_err(|e| SandboxError::Snapshot(format!("parse disk manifest: {e}")))?;
            self.inline_disk_manifests
                .insert(mig.disk_manifest_ref, disk);
        }
        tracing::info!(
            snapshot_id = %metadata.id,
            export = %mig.export_id,
            mem_chunks = mig.new_memory_chunk_hashes.len(),
            disk_chunks = mig.new_disk_chunk_hashes.len(),
            "migration prestage complete (ADR 0045 C1)",
        );
        Ok(())
    }

    /// ADR 0045 C1 (destination): post-restore — seed the chain from
    /// the inline v+1 content, fence checkpoints + disk publishes
    /// until durability, and spawn the catch-up (chunks → manifests)
    /// into the `snapshot_wait` slot for the coordinator's
    /// row-only-at-finalize.
    async fn migration_finish_restore(
        &self,
        id: SandboxId,
        mig: engram_core::types::snapshot::MigrationSourceInfo,
        mut row_template: SnapshotMetadata,
    ) -> Result<(), SandboxError> {
        let mem_manifest: engram_chunk_store::Manifest =
            serde_json::from_slice(&mig.memory_manifest_json)
                .map_err(|e| SandboxError::Snapshot(format!("parse mem manifest: {e}")))?;
        self.checkpoint_chains.insert(
            id,
            crate::checkpoint::CheckpointChain {
                manifest_ref: mig.memory_manifest_ref,
                manifest: mem_manifest.clone(),
            },
        );
        // Fences: the capture lock blocks checkpoints (a dest
        // checkpoint pre-durability would publish a v+2 referencing
        // not-yet-uploaded chunks); the NBD migration_fence blocks
        // disk publishes for the same reason.
        let capture_guard = self.capture_lock(id).lock_owned().await;
        #[cfg(target_os = "linux")]
        if let Some(entry) = self.nbd_sandboxes.get(&id) {
            entry.backend.set_migration_fence(true);
        }

        let Some(chunk_store) = self.chunk_store.clone() else {
            return Err(SandboxError::InvalidSpec("no chunk store".into()));
        };
        let Some(cache) = self.chunk_cache.clone() else {
            return Err(SandboxError::InvalidSpec("no chunk cache".into()));
        };
        #[cfg(target_os = "linux")]
        let nbd = self.nbd_sandboxes.get(&id).map(|e| e.backend.clone());
        let inline_disks = self.inline_disk_manifests.clone();
        // For the deferred chain-head commit below: the in-RAM seed
        // above precedes chunk durability, so the durable record must
        // wait for the catch-up's put_manifest — a pod roll mid-catch-up
        // then rehydrates nothing (Full, safe) instead of a head whose
        // chunks never reached GCS.
        let chain_heads = self.chain_heads.clone();
        let chain_session = self.session_bindings.get(&id).map(|s| *s);
        let clock = self.clock.clone();
        let handle = tokio::spawn(async move {
            let _capture_guard = capture_guard;
            // 1. Upload every pulled chunk (content-addressed,
            //    idempotent). PARALLEL with bounded fan-out — the
            //    serial loop cost ~55ms/chunk of GCS RTT (the prod
            //    canary's 4.6s catch-up; the same lesson as ADR 0039
            //    item #19), and puts are order-independent.
            {
                use futures::stream::{StreamExt, TryStreamExt};
                futures::stream::iter(
                    mig.new_memory_chunk_hashes
                        .iter()
                        .chain(mig.new_disk_chunk_hashes.iter())
                        .copied()
                        .collect::<Vec<_>>(),
                )
                .map(|h| {
                    let cache = cache.clone();
                    let chunk_store = chunk_store.clone();
                    async move {
                        let hash = engram_chunk_store::manifest::ChunkHash::from_bytes(h);
                        let bytes = cache
                            .get(hash, || async {
                                Err(engram_chunk_store::error::ChunkStoreError::Internal(
                                    "catch-up chunk must be cache-resident".into(),
                                ))
                            })
                            .await
                            .map_err(|e| {
                                SandboxError::Snapshot(format!("catch-up read {hash}: {e}"))
                            })?;
                        // ADR 0078 move 5: the teleport catch-up uploads the
                        // source's un-flushed memory divergence — new by
                        // construction (memory changed since the last
                        // publish), so skip the per-chunk `exists()` GCS HEAD.
                        chunk_store.put_chunk_unchecked(&bytes).await.map_err(|e| {
                            SandboxError::Snapshot(format!("catch-up upload {hash}: {e}"))
                        })?;
                        Ok::<(), SandboxError>(())
                    }
                })
                .buffer_unordered(32)
                .try_collect::<()>()
                .await?;
            }
            // 2. Publish the memory manifest (session-owned lineage —
            //    no conflict possible).
            chunk_store
                .put_manifest(mig.memory_manifest_ref, &mem_manifest)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("publish mem manifest: {e}")))?;
            // The chain head is durable now — commit its record (the
            // capture_guard is still held, so no capture can have
            // invalidated in between). Best-effort like every other
            // chain-head write.
            if let Some(store) = &chain_heads {
                let record = crate::checkpoint::ChainHeadRecord {
                    sandbox_id: id,
                    manifest_ref: mig.memory_manifest_ref,
                    session_id: chain_session,
                    updated_at: clock.now_utc(),
                };
                if let Err(e) = store.persist(record).await {
                    tracing::warn!(sandbox_id = %id, error = %e,
                        "migration catch-up: chain-head record write failed");
                }
            }
            // 3. Publish the disk manifest with the shared-lineage
            //    conflict-retry (mirror flush_upload's rule).
            let mut disk_ref_final = None;
            if let Some((_, disk_manifest)) = inline_disks.remove(&mig.disk_manifest_ref) {
                let mut attempt_ref = mig.disk_manifest_ref;
                for _ in 0..5 {
                    match chunk_store.put_manifest(attempt_ref, &disk_manifest).await {
                        Ok(()) => {
                            disk_ref_final = Some(attempt_ref);
                            break;
                        }
                        Err(engram_chunk_store::error::ChunkStoreError::VersionConflict {
                            latest,
                            ..
                        }) => {
                            attempt_ref.version = latest + 1;
                        }
                        Err(e) => {
                            return Err(SandboxError::Snapshot(format!(
                                "publish disk manifest: {e}"
                            )))
                        }
                    }
                }
                let Some(final_ref) = disk_ref_final else {
                    return Err(SandboxError::Snapshot(
                        "disk manifest publish: version conflict retries exhausted".into(),
                    ));
                };
                #[cfg(target_os = "linux")]
                if let Some(nbd) = &nbd {
                    nbd.rebase_manifest_ref(final_ref).await;
                    nbd.set_migration_fence(false);
                }
                row_template.disk_manifest = Some(final_ref);
            } else {
                #[cfg(target_os = "linux")]
                if let Some(nbd) = &nbd {
                    nbd.set_migration_fence(false);
                }
            }
            row_template.memory_manifest = Some(mig.memory_manifest_ref);
            tracing::info!(
                sandbox_id = %id,
                mem_ref = %mig.memory_manifest_ref,
                "migration durability catch-up complete (ADR 0045 C1)",
            );
            Ok(row_template)
        });
        if let Some(prior) = self
            .snapshot_waits
            .insert(id, SnapshotWait::from_handle(handle))
        {
            prior.abort.abort();
        }
        Ok(())
    }

    /// ADR 0045 C1: expired migration exports for the TTL sweep —
    /// `(sandbox, bound session, export_id)` per export past
    /// [`crate::migration::EXPORT_TTL`].
    pub fn expired_migration_exports(&self) -> Vec<(SandboxId, Option<SessionId>, String)> {
        self.migrations
            .expired()
            .into_iter()
            .filter_map(|sandbox_id| {
                let export_id = self.migrations.export_id_of(sandbox_id)?;
                let session = self.session_bindings.get(&sandbox_id).map(|e| *e);
                Some((sandbox_id, session, export_id))
            })
            .collect()
    }

    /// The session a sandbox is bound to, when known (populated by
    /// `notify_session_policy` / registration rehydration).
    pub fn session_for_sandbox(&self, id: SandboxId) -> Option<SessionId> {
        self.session_bindings.get(&id).map(|e| *e)
    }

    /// ADR 0090: repopulate a binding the local table missed (e.g. a
    /// pidfd-reattached survivor whose NBD rehydrate bailed before the
    /// insert). Source of truth is the coordinator's `sandbox_owner`
    /// answer; recording it locally restores the fast path for the
    /// publisher/reconciler without another RPC.
    pub fn record_session_binding(&self, sandbox_id: SandboxId, session_id: SessionId) {
        self.session_bindings.insert(sandbox_id, session_id);
    }

    /// ADR 0090: the survivors whose NBD slots this generation
    /// quarantined (rehydrate `RECONFIGURE` failed) — re-advertised in
    /// every heartbeat until the sandbox is destroyed, so the
    /// coordinator drives the `evict_local → resume` remediation.
    pub fn quarantined_survivors(&self) -> Vec<engram_protocol::heartbeat::QuarantinedSurvivor> {
        self.quarantined_survivors
            .iter()
            .map(|e| engram_protocol::heartbeat::QuarantinedSurvivor {
                sandbox_id: *e.key(),
                session_id: *e.value(),
            })
            .collect()
    }

    /// ADR 0091: record a control-plane-dead guest (checkpoint driver's
    /// 3/3-probe verdict). Re-advertised every heartbeat until cleared.
    pub fn mark_guest_unreachable(&self, sandbox_id: SandboxId, session_id: SessionId) {
        self.unreachable_guests.insert(sandbox_id, session_id);
    }

    /// ADR 0091: a successful capture (or destroy) clears the suspicion.
    pub fn clear_guest_unreachable(&self, sandbox_id: SandboxId) {
        self.unreachable_guests.remove(&sandbox_id);
    }

    /// ADR 0091: the heartbeat's unreachable-guest advert.
    pub fn unreachable_guests(&self) -> Vec<(SandboxId, SessionId)> {
        self.unreachable_guests
            .iter()
            .map(|e| (*e.key(), *e.value()))
            .collect()
    }

    pub fn checkpoint_records_dir(&self) -> Option<PathBuf> {
        self.checkpoint_dir.as_ref().map(|d| d.join("records"))
    }

    /// Test-only view of a sandbox's in-RAM chain head.
    #[cfg(test)]
    pub(crate) fn chain_head_for_test(
        &self,
        id: SandboxId,
    ) -> Option<engram_core::types::manifest::ManifestRef> {
        self.checkpoint_chains.get(&id).map(|c| c.manifest_ref)
    }

    /// Re-seed checkpoint chains for VMs that survived a host-agent
    /// restart (pidfd reattach, ADR 0044 K2 / ADR 0090), from the
    /// durable [`crate::checkpoint::ChainHeadRecord`]s — and GC records
    /// whose sandbox did NOT survive. Called once at startup, after the
    /// reattach pass and `set_self_ref`, BEFORE anything that can start
    /// a capture (checkpoint driver, eviction redrive, coordinator
    /// registration): a survivor's first post-roll capture then rides
    /// the O(dirty-set) diff path instead of a FULL multi-GiB re-chunk
    /// (the 2026-07-13 incident's 40-minute evict).
    ///
    /// Torn-capture safety is inherited from the record's write-ahead
    /// protocol (see the record's type doc): a record only exists if no
    /// FC snapshot create ran since the chain durably advanced, so
    /// seeding from it is exactly as sound as never having lost the
    /// DashMap. NBD-quarantined survivors seed too — the disk plane's
    /// health is orthogonal to the KVM dirty bitmap, and their
    /// evict_local capture is precisely the one that must not be a
    /// Full.
    pub async fn rehydrate_chain_heads(&self) {
        let Some(store) = self.chain_heads.clone() else {
            return;
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return;
        };
        let live: std::collections::HashSet<SandboxId> = match self.inner.list().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::warn!(error = %e, "chain-head rehydrate: backend list failed; skipping");
                return;
            }
        };
        let dir = store.dir().to_path_buf();
        for record in crate::checkpoint::ChainHeadRecord::load_all(&dir).await {
            let id = record.sandbox_id;
            if !live.contains(&id) {
                // The sandbox didn't survive (node reboot, destroyed
                // while the record write raced teardown) — the record
                // is unreachable; sweep it.
                store.remove_best_effort(id);
                continue;
            }
            // Serialize against any capture already running for this
            // sandbox: a capture that won the lock first has already
            // write-ahead-removed the record, so the re-read below
            // no-ops — without the lock, "read record → capture
            // invalidates + creates (bitmap reset) → seed stale head"
            // would rebuild exactly the corrupt diff this protocol
            // exists to prevent.
            let lock = self.capture_lock(id);
            let _guard = lock.lock_owned().await;
            if self.checkpoint_chains.contains_key(&id) {
                continue;
            }
            let Some(record) = crate::checkpoint::ChainHeadRecord::load(&dir, id).await else {
                continue;
            };
            match chunk_store.get_manifest(record.manifest_ref).await {
                Ok(manifest) => {
                    self.checkpoint_chains.insert(
                        id,
                        crate::checkpoint::CheckpointChain {
                            manifest_ref: record.manifest_ref,
                            manifest,
                        },
                    );
                    tracing::info!(
                        sandbox_id = %id,
                        session_id = ?record.session_id,
                        manifest = %record.manifest_ref,
                        "chain head rehydrated from durable record; survivor's next capture will diff",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %id,
                        manifest = %record.manifest_ref,
                        error = %e,
                        "chain-head rehydrate: manifest fetch failed; next capture falls back to Full",
                    );
                }
            }
        }
    }

    /// Local-first survivor NBD rehydrate (session 731df805,
    /// 2026-07-17): claim + RECONFIGURE the NBD device of every live
    /// survivor the coordinator's register-time list MISSED. That list
    /// is derived from PG session status and can be wrong — it was
    /// empty for rung-parked (`evicting`) survivors, so nothing
    /// re-claimed their devices after the pod roll and the
    /// stale-binding sweep disconnected the live rootfs out from under
    /// the paused guests. Everything this needs is already durable on
    /// this host: the write-ahead `ChainHeadRecord` carries
    /// (sandbox_id, session_id, manifest_ref) and the reattach pass
    /// has rebuilt `inner`'s sandbox set — so a coordinator-side gap
    /// must never again decide whether a resident VM keeps its disk.
    ///
    /// Run AFTER the coord-list `rehydrate_survivors` pass (entries
    /// covered by both are skipped via the `nbd_sandboxes` presence
    /// check inside [`Self::rehydrate_sandbox`]) and BEFORE the
    /// stale-binding sweep snapshots the free slot pool.
    ///
    /// Returns `(rehydrated, failed)`.
    #[cfg(target_os = "linux")]
    pub async fn rehydrate_local_survivors(&self) -> (usize, usize) {
        let Some(store) = self.chain_heads.clone() else {
            return (0, 0);
        };
        let live: std::collections::HashSet<SandboxId> = match self.inner.list().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "local survivor rehydrate: backend list failed; skipping",
                );
                return (0, 0);
            }
        };
        let served: std::collections::HashSet<SandboxId> =
            self.nbd_sandboxes.iter().map(|e| *e.key()).collect();
        let records = crate::checkpoint::ChainHeadRecord::load_all(store.dir()).await;
        let mut rehydrated = 0usize;
        let mut failed = 0usize;
        for (session_id, sandbox_id, manifest_ref) in
            local_survivor_candidates(records, &live, &served)
        {
            match self
                .rehydrate_sandbox(session_id, sandbox_id, manifest_ref)
                .await
            {
                Ok(true) => {
                    tracing::warn!(
                        %sandbox_id,
                        %session_id,
                        manifest = %manifest_ref,
                        "local survivor rehydrate: re-served an NBD device the \
                         coordinator's rehydrate list missed (coord-side gap — \
                         the device would otherwise have been left to the \
                         stale-binding sweep)",
                    );
                    rehydrated += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        %sandbox_id,
                        %session_id,
                        error = %e,
                        "local survivor rehydrate failed; continuing with the rest",
                    );
                    failed += 1;
                }
            }
        }
        (rehydrated, failed)
    }

    /// The Layer-2 kernel-derived inventory + Layer-3 classification barrier
    /// (ADR 0098 §Phase 3, Wave 7b, #784). Run AFTER the two rehydrate passes
    /// (coord-list + #739 local) and BEFORE the destructive stale-binding sweep:
    /// enumerate the kernel's CONNECTED devices as ground truth and RECONCILE the
    /// tracked records against them, classifying every connected slot into
    /// exactly one [`SlotClass`](engram_host_core::SlotClass). The tracked-record
    /// device set is every device a coord-list survivor, a durable
    /// `ChainHeadRecord`, or a now-served sandbox maps to — so a re-served
    /// survivor shows self-owned (`Serving`) and a device NO record accounts for
    /// (its live guest invisible to both passes) surfaces as
    /// `QuarantinedUnknown`, fires the `rehydrate-unknown-device` soft-invariant
    /// + counter (parked, RECONNECTABLE), and is NEVER handed to the sweep.
    ///
    /// Returns the classification; the caller feeds `.reap` (the sole
    /// `TerminalSafeToReap` subset) to [`recover_stuck_nbd_devices`] — the
    /// ordering contract enforced by the [`ReapList`](engram_host_core::ReapList)
    /// type, not a comment.
    #[cfg(target_os = "linux")]
    pub async fn classify_startup_slots(
        &self,
        kernel: &dyn engram_host_core::NbdKernel,
        coord_survivors: &[crate::coord_client::RehydrateSandboxRef],
    ) -> engram_host_core::StartupClassification<std::path::PathBuf> {
        // The tracked-record device set — the Layer-2 reconcile key. A device is
        // "accounted for" if a coord-list survivor, a durable ChainHeadRecord, or
        // an already-served sandbox maps to it (`rootfs_device` resolves the
        // sandbox's `/dev/nbdN`). Anything CONNECTED but absent from this set is
        // a survivor invisible to the records.
        let mut record_devices: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        for entry in coord_survivors {
            if let Some(dev) = self.inner.rootfs_device(entry.sandbox_id) {
                record_devices.insert(dev);
            }
        }
        if let Some(store) = self.chain_heads.clone() {
            for record in crate::checkpoint::ChainHeadRecord::load_all(store.dir()).await {
                if let Some(dev) = self.inner.rootfs_device(record.sandbox_id) {
                    record_devices.insert(dev);
                }
            }
        }
        for entry in self.nbd_sandboxes.iter() {
            if let Some(dev) = self.inner.rootfs_device(*entry.key()) {
                record_devices.insert(dev);
            }
        }
        // Devices this process itself PARKED (a failed rehydrate's
        // `slot.quarantine()`) are tracked records too. The three sources
        // above all resolve through the live FC entry (`rootfs_device`),
        // which a concurrent sandbox destroy can vacate between the park and
        // this barrier — 2026-07-21: a rehydrate-failed survivor whose
        // session completed two seconds later was reported as an UNKNOWN
        // device demanding an operator, when this very process had parked it
        // on purpose moments earlier. The allocator's parked set is
        // device-keyed, so it survives the FC entry vanishing.
        if let Some(pool) = self.nbd_pool.as_ref() {
            for dev in pool.parked_devices() {
                record_devices.insert(dev);
            }
        }

        let classification =
            crate::disk_daemon::classify_startup_inventory(kernel, &record_devices);

        // Quarantine: a CONNECTED device the reconcile could not account for. Fire
        // the alertable soft-invariant + counter per device and leave it
        // kernel-bound (RECONNECTABLE) — never sever, never silently skip.
        for device in &classification.quarantined {
            engram_core::soft_invariant!(
                "rehydrate-unknown-device",
                false,
                "startup classification barrier: kernel-CONNECTED NBD device {} has a \
                 live (or unprovable) holder but NO tracked record accounts for it — a \
                 survivor invisible to both the coordinator rehydrate list AND the #739 \
                 local ChainHeadRecord pass (#769 gap A). Quarantined: left RECONNECTABLE \
                 (kernel binding intact, kept out of new-claim circulation by the \
                 nbd_kernel_busy probe), NEVER handed to the stale-binding sweep. An \
                 operator/runbook must reconcile this device's session",
                device.display(),
            );
            ::metrics::counter!(crate::metrics::REHYDRATE_UNKNOWN_DEVICE_TOTAL).increment(1);
        }
        if !classification.reconnect.is_empty() {
            tracing::warn!(
                count = classification.reconnect.len(),
                "startup classification: {} kernel-connected device(s) are known survivors \
                 the rehydrate passes did not (yet) re-serve — left RECONNECTABLE for a \
                 retry, never reaped",
                classification.reconnect.len(),
            );
        }
        tracing::info!(
            serving = classification.serving.len(),
            reconnect = classification.reconnect.len(),
            quarantined = classification.quarantined.len(),
            reap = classification.reap.len(),
            "startup NBD classification barrier complete (kernel-derived inventory \
             reconciled against tracked records)",
        );
        classification
    }

    /// ADR 0028 Fix A: post-capture chain bookkeeping + the durable
    /// host-owned record. Runs at the tail of every successful
    /// `snapshot()` (periodic checkpoint, eviction, drain, SIGTERM —
    /// they're all chain entries now). Never fails the capture: the
    /// snapshot's own durability (chunks + blobs in GCS) is already
    /// settled; everything here is acceleration (chain) or
    /// reconciliation insurance (record).
    async fn seed_checkpoint_chain_sparse(
        &self,
        id: SandboxId,
        memory_ref: engram_core::types::manifest::ManifestRef,
    ) {
        self.finisher()
            .seed_checkpoint_chain_sparse(id, memory_ref)
            .await
    }

    async fn seed_checkpoint_chain_forked(
        &self,
        id: SandboxId,
        src_ref: engram_core::types::manifest::ManifestRef,
    ) {
        self.finisher()
            .seed_checkpoint_chain_forked(id, src_ref)
            .await
    }

    /// ADR 0101 B: sandboxes due for a periodic checkpoint — each
    /// sandbox's due-interval comes from its last epoch's observed
    /// dirty rate ([`crate::checkpoint::next_epoch_after`]), clamped to
    /// `[cfg.min_interval, cfg.interval]`. A sandbox with no pacing
    /// sample yet (fresh bind, chain seed pending, Full-only history)
    /// keeps the max-interval backstop cadence. (The flat-interval
    /// predecessor is retired — this is the only candidacy surface.)
    pub fn checkpoint_candidates_adaptive(
        &self,
        cfg: &crate::checkpoint::CheckpointConfig,
    ) -> Vec<(SandboxId, SessionId)> {
        let Some(max_interval) = cfg.interval else {
            return Vec::new();
        };
        if self.checkpoint_dir.is_none() {
            return Vec::new();
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.session_bindings
            .iter()
            .filter_map(|e| {
                let id = *e.key();
                let last = self.last_snapshot_unix_ms.get(&id).map(|v| *v).unwrap_or(0);
                let due_after = match self.checkpoint_pacing.get(&id).map(|v| *v) {
                    Some(s) => crate::checkpoint::next_epoch_after(
                        s.epoch,
                        s.dirty_bytes,
                        cfg.min_interval,
                        max_interval,
                        cfg.target_epoch_bytes,
                    ),
                    None => max_interval,
                };
                (now_ms - last >= due_after.as_millis() as i64).then_some((id, *e.value()))
            })
            .collect()
    }

    /// ADR 0028 Fix A: one periodic checkpoint — the same capture
    /// `snapshot()` runs for evictions (diff-flavored once the chain
    /// is seeded), self-committed because the durable record +
    /// heartbeat re-advertise own the reference, not a coord
    /// commit/abort pipeline.
    pub async fn checkpoint_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<SnapshotMetadata, SandboxError> {
        use engram_core::traits::SandboxBackend as _;
        let metadata = self.snapshot(id).await?;
        let _ = self.commit_snapshot(id).await;
        Ok(metadata)
    }

    /// Override the default Phase B flush scheduler config. The
    /// production host-agent leaves this at `from_env()`; tests use
    /// this to disable the scheduler entirely (`enabled = false`)
    /// or tighten the interval to drive deterministic test cases.
    pub fn with_flush_config(mut self, config: crate::disk_daemon::FlushSchedulerConfig) -> Self {
        self.flush_config = config;
        self
    }

    /// ADR 0016 Phase B commit 4: build a coord-bound publisher
    /// using the host-agent's `HttpCoordClient` and the freshly-wrapped
    /// `session_bindings` map. The publisher spawns its own drain
    /// task; the returned `LiveManifestPublisherHandle` is held
    /// inside PooledBackend so the task dies with us.
    ///
    /// Internal session resolver closes over a clone of
    /// `self.session_bindings` — that's the host-agent's
    /// sandbox→session index, populated by `notify_session_policy`
    /// (start_agent) and cleared by `destroy`. A publish that
    /// arrives before `notify_session_policy` has populated the
    /// entry (warm-pool sandbox pre-assignment, or the small
    /// cold-create window between `inner.create()` and
    /// `start_agent`) returns None from the resolver → drain task
    /// skips with a debug log.
    pub fn with_live_manifest_coord_publisher(
        mut self,
        coord: Arc<dyn engram_host_core::CoordControlPlane>,
        host_id: engram_core::HostId,
    ) -> Self {
        let session_bindings = Arc::clone(&self.session_bindings);
        let resolver: Arc<dyn crate::disk_daemon::SessionResolver> =
            Arc::new(move |sandbox_id: SandboxId| -> Option<SessionId> {
                session_bindings.get(&sandbox_id).map(|e| *e)
            });
        let (publisher, handle) =
            crate::disk_daemon::CoordLiveManifestPublisher::spawn(coord.clone(), host_id, resolver);
        self.live_manifest_publisher = publisher;
        self.live_manifest_publisher_handle = Some(handle);
        // Issue #225: keep a direct handle to coord for the SIGTERM
        // final-flush pass, which must publish synchronously (the
        // async publisher's drain task is gone by the time the
        // process exits).
        self.shutdown_manifest_publish = Some((coord, host_id));
        self
    }

    /// Test hook: inject an arbitrary [`LiveManifestPublisher`]
    /// (e.g. a recording mock). Production wiring uses
    /// [`Self::with_live_manifest_coord_publisher`].
    #[cfg(test)]
    pub fn with_live_manifest_publisher(
        mut self,
        publisher: Arc<dyn crate::disk_daemon::LiveManifestPublisher>,
    ) -> Self {
        self.live_manifest_publisher = publisher;
        self.live_manifest_publisher_handle = None;
        self
    }

    /// Attach a `/dev/nbdN` slot pool. When set + `chunk_store` +
    /// `chunk_cache` are also wired AND the cached image bundle
    /// carries a `disk_manifest`, `create()` spawns an NBD daemon
    /// instead of materializing the manifest to a single file. FC's
    /// `path_on_host` becomes `/dev/nbdN`. Linux-only at runtime;
    /// on macOS the builder accepts a pool but the spawn path is
    /// gated so dev workflows fall back to materialize-to-file.
    pub fn with_nbd_pool(mut self, pool: Arc<crate::disk_daemon::NbdSlotAllocator>) -> Self {
        self.nbd_pool = Some(pool);
        self
    }

    /// The NBD slot pool, when this host serves chunked rootfs via
    /// NBD. Used by the post-rehydrate stale-binding sweep to scope
    /// itself to still-free slots (ADR 0044 K2).
    pub fn nbd_pool(&self) -> Option<Arc<crate::disk_daemon::NbdSlotAllocator>> {
        self.nbd_pool.clone()
    }

    /// Test-only: whether `id` currently has a live NBD data plane in
    /// `nbd_sandboxes`. Used by the issue-#223 cancellation regression
    /// test to assert the daemon landed in the map (and was therefore
    /// retained, owned by normal `destroy()` teardown) even when the
    /// `restore` handler future was dropped mid-flight.
    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn __test_nbd_sandbox_registered(&self, id: SandboxId) -> bool {
        self.nbd_sandboxes.contains_key(&id)
    }

    /// Test-only: clone the live `ChunkedDiskBackend` Arc for a
    /// registered sandbox. Used by the issue-#225 SIGTERM-final-flush
    /// regression test to drive a dirty write through the same backend
    /// the shutdown pass flushes and then assert durability.
    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn __test_nbd_backend(
        &self,
        id: SandboxId,
    ) -> Option<Arc<crate::disk_daemon::ChunkedDiskBackend>> {
        self.nbd_sandboxes.get(&id).map(|e| e.backend.clone())
    }

    /// Test-only: bind a sandbox to a session in `session_bindings`,
    /// the same index `notify_session_policy` / registration populate.
    /// The issue-#225 test uses it so the shutdown flush can resolve a
    /// session id for its (mock) coord publish.
    #[cfg(target_os = "linux")]
    #[doc(hidden)]
    pub fn __test_bind_session(&self, sandbox_id: SandboxId, session_id: SessionId) {
        self.session_bindings.insert(sandbox_id, session_id);
    }

    /// Issue #225: SIGTERM final-flush pass. NBD WRITEs are acked to
    /// the guest the instant the bytes land in the backend's in-RAM
    /// `dirty` tier; durability rides the FlushScheduler's ~30 s /
    /// 256 MiB cadence. A routine pod roll abandons each data plane
    /// (`abandon_for_shutdown` → `drop(backend)`) WITHOUT a final
    /// flush, discarding up to one cadence-window of ACKED writes —
    /// the VM keeps running (K2 contract) but the successor rehydrates
    /// from the last *published* manifest, silently rolling the live
    /// guest's disk back under it. This pass closes that window: per
    /// surviving sandbox, in parallel and under a hard deadline, it
    /// quiesces in-flight I/O (`wait_idle`) then runs a full `flush()`
    /// (drain → GCS upload → manifest rebase) and SYNCHRONOUSLY
    /// publishes the new `live_disk_manifest` to coord — so the
    /// successor's existing rehydrate path picks up the current ref.
    ///
    /// MUST run BEFORE `abandon_nbd_data_planes_for_shutdown`: this
    /// only flushes, it does not tear anything down, so the abandon
    /// sweep still runs afterward to leave the kernel-side devices
    /// alive for the successor. The whole pass is budgeted against
    /// `deadline` (derived from the pod's `terminationGracePeriodSeconds`
    /// minus headroom). Any sandbox not flushed within the budget is
    /// logged LOUDLY with its id + dirty byte count so the (now bounded)
    /// loss is at least visible — it is then abandoned dirty by the
    /// following sweep, exactly as before this fix.
    ///
    /// The synchronous coord publish is deliberate: the normal flush
    /// path publishes via the async `live_manifest_publisher`, whose
    /// coalescing drain task is aborted on process exit — a manifest
    /// queued there during shutdown would never reach coord. When no
    /// coord publisher is wired (`shutdown_manifest_publish` is `None`:
    /// no-op / test publishers) the chunks are still durably uploaded
    /// to GCS; only the coord publish is skipped.
    #[cfg(target_os = "linux")]
    pub async fn flush_nbd_data_planes_for_shutdown(&self, deadline: std::time::Duration) {
        // The `DeviceSync` seam's `sync_device` method (ADR 0098 P4).
        use engram_host_core::DeviceSync as _;
        let entries: Vec<(
            SandboxId,
            Arc<crate::disk_daemon::ChunkedDiskBackend>,
            std::path::PathBuf,
        )> = self
            .nbd_sandboxes
            .iter()
            .map(|e| {
                (
                    *e.key(),
                    e.value().backend.clone(),
                    e.value().device_path().to_path_buf(),
                )
            })
            .collect();
        if entries.is_empty() {
            return;
        }
        let total = entries.len();
        tracing::info!(
            sandboxes = total,
            deadline_secs = deadline.as_secs_f64(),
            "SIGTERM: final disk-flush pass over surviving NBD data planes",
        );

        // Fan out one flush future per sandbox; each resolves the
        // session binding + does the synchronous coord publish itself.
        // Budget the WHOLE fan-out against `deadline` — a single
        // tokio::time::timeout around the join handles the per-sandbox
        // parallelism + the global cap in one place.
        let publish = self.shutdown_manifest_publish.clone();
        let session_bindings = self.session_bindings.clone();
        let flush_all = async move {
            let mut tasks = Vec::with_capacity(entries.len());
            for (sandbox_id, backend, device) in entries {
                let publish = publish.clone();
                let session_id = session_bindings.get(&sandbox_id).map(|e| *e);
                tasks.push(tokio::spawn(async move {
                    // 2026-07-16 RCA: FC's drive is buffered host I/O with
                    // cache_type=Unsafe, so guest-acked writes can still be
                    // sitting in the HOST page cache for /dev/nbdN — a tier
                    // the dirty-map flush below never sees, and one the
                    // pod-handoff dead-connection window can silently drop
                    // (`lost async page write`). Force it down into the
                    // daemon's dirty tier NOW, while our serve loop is
                    // still alive to ack the writeback (the checkpoint path
                    // does the same). Routed through the DeviceSync seam
                    // (ADR 0098 P4) — a spawn_blocking open+sync_all; a
                    // join/sync failure is warn-and-proceed. O_DIRECT here
                    // is a no-op (see `device_sync`), so the sync path is
                    // unchanged.
                    if let Err(e) = crate::device_sync::HostDeviceSync
                        .sync_device(&device)
                        .await
                    {
                        tracing::warn!(
                            %sandbox_id,
                            device = %device.display(),
                            error = %e,
                            "SIGTERM final flush: host page-cache sync of the NBD \
                             device failed; proceeding (pages left behind will ride \
                             the kernel's dead-conn parking to the successor)",
                        );
                    }
                    // Quiesce the virtio → kernel-NBD → daemon pipeline so
                    // the flush captures the just-acked disk state, then
                    // drain + upload + rebase. `flush` no-ops (zero chunks)
                    // when the dirty tier is empty — cheap for quiescent
                    // survivors.
                    backend.wait_idle().await;
                    // ADR 0098 P4: the per-survivor disposition is the pure
                    // `classify_survivor` decision; the driver only sequences
                    // the effects off its verdict.
                    let bound_publish = publish.zip(session_id);
                    let outcome = match backend.flush().await {
                        Ok(o) => o,
                        Err(e) => {
                            // FlushProbe::FlushError ⇒ RelyOnSpool: the abandon
                            // sweep's spool export is the durability backstop.
                            tracing::warn!(
                                %sandbox_id,
                                error = %e,
                                "SIGTERM final flush failed; survivor's un-uploaded \
                                 writes ride the shutdown spool to the successor",
                            );
                            return;
                        }
                    };
                    let action = engram_host_core::classify_survivor(
                        engram_host_core::FlushProbe::Flushed {
                            chunks_flushed: outcome.chunks_flushed,
                            bound: bound_publish.is_some(),
                        },
                    );
                    match action {
                        // Already clean — nothing new to publish.
                        engram_host_core::SurvivorAction::SkipClean => return,
                        // Unreachable for a `Flushed` probe (only `FlushError`
                        // maps to RelyOnSpool, and that returned above); keep
                        // the arm so the match stays exhaustive.
                        engram_host_core::SurvivorAction::RelyOnSpool => return,
                        engram_host_core::SurvivorAction::DurableNoPublish
                        | engram_host_core::SurvivorAction::Publish => {}
                    }
                    tracing::info!(
                        %sandbox_id,
                        chunks = outcome.chunks_flushed,
                        bytes = outcome.bytes_uploaded,
                        manifest_version = outcome.manifest_ref.version,
                        "SIGTERM final flush uploaded survivor's dirty chunks",
                    );
                    // Synchronously publish so the successor rehydrates
                    // from the just-uploaded ref instead of the stale one.
                    let Some(((coord, host_id), session_id)) = bound_publish else {
                        // DurableNoPublish: no coord wired, or the sandbox
                        // isn't bound to a session yet (warm-pool /
                        // pre-start_agent window). The chunks are durable in
                        // GCS regardless; the publish is what we cannot do here.
                        tracing::debug!(
                            %sandbox_id,
                            "SIGTERM final flush: chunks durable in GCS but no \
                             coord publish (unbound sandbox or no publisher)",
                        );
                        return;
                    };
                    let req = engram_host_core::LiveManifestPublishRequest {
                        session_id,
                        sandbox_id,
                        manifest_id: outcome.manifest_ref.manifest_id,
                        manifest_version: outcome.manifest_ref.version,
                    };
                    match coord.publish_live_manifest(host_id, &req).await {
                        Ok(_) => tracing::info!(
                            %sandbox_id,
                            %session_id,
                            manifest_version = outcome.manifest_ref.version,
                            "SIGTERM final flush: live_disk_manifest published to coord",
                        ),
                        // A publish failure falls back to the same RelyOnSpool
                        // posture: the shutdown spool's store-ahead ref covers
                        // a same-node successor.
                        Err(e) => tracing::warn!(
                            %sandbox_id,
                            %session_id,
                            error = %e,
                            "SIGTERM final flush: chunks uploaded to GCS but coord \
                             publish failed; the shutdown spool's store-ahead ref \
                             covers a same-node successor",
                        ),
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
        };

        if tokio::time::timeout(deadline, flush_all).await.is_err() {
            // Deadline overrun: some survivors were not GCS-flushed in
            // time. This is no longer a data-loss event: the abandon
            // sweep that runs next exports every still-dirty tier to the
            // node-local shutdown spool (2026-07-16 RCA), and the
            // successor adopts it. Log the stragglers so the GCS-side
            // durability gap on this node stays visible.
            // INVARIANT (see `nbd_sandboxes`): snapshot id+backend Arcs out
            // of the map, then `.await` on the owned Arcs — never hold a
            // DashMap guard across the `dirty_bytes` await.
            let stragglers: Vec<(SandboxId, Arc<crate::disk_daemon::ChunkedDiskBackend>)> = self
                .nbd_sandboxes
                .iter()
                .map(|e| (*e.key(), e.value().backend.clone()))
                .collect();
            for (sandbox_id, backend) in stragglers {
                let dirty = backend.dirty_bytes().await;
                // ADR 0098 P4: `is_straggler` is the pure deadline-overrun
                // decision (still-dirty at the deadline ⇒ loud, the spool
                // catches it).
                if engram_host_core::is_straggler(dirty) {
                    tracing::warn!(
                        %sandbox_id,
                        dirty_bytes = dirty,
                        deadline_secs = deadline.as_secs_f64(),
                        "SIGTERM final flush DEADLINE OVERRUN: survivor still has \
                         un-uploaded dirty bytes; they will be preserved in the \
                         shutdown spool for the successor to adopt",
                    );
                }
            }
        }
    }

    /// ADR 0044 K2 graceful shutdown: abandon every live NBD data
    /// plane WITHOUT disconnecting the kernel side, so surviving FC
    /// VMs keep their (parked) devices for the successor generation
    /// to RECONFIGURE. Called from the SIGTERM path right before the
    /// process exits; per-sandbox destroy keeps its normal
    /// disconnect-on-drop.
    ///
    /// Issue #224: this is a TERMINAL mode, not a one-shot sweep. The
    /// `abandoning` flag is raised (SeqCst) BEFORE the drain so that
    /// any insert site (`create` / `restore_with` /
    /// `rehydrate_sandbox`) whose multi-second await window
    /// (pool-claim, GCS manifest fetch, netlink RECONFIGURE / FC
    /// restore) is still in flight will observe the flag at its
    /// pre-insert check and `abandon_for_shutdown()` the state
    /// in-place instead of leaking a live data plane into the map
    /// after the drain. We then drain, and re-drain once: the
    /// flag-before-drain ordering means anything that passes its
    /// flag check before the store but inserts after the first drain
    /// is rare, but the belt-and-braces second pass closes it (an
    /// insert that lands between the two drains is caught here; one
    /// that lands after both saw the flag set during its own check
    /// and abandoned instead).
    #[cfg(target_os = "linux")]
    pub async fn abandon_nbd_data_planes_for_shutdown(&self) -> usize {
        // Raise the terminal flag FIRST — ordering is the correctness
        // gate. Every insert site loads it with SeqCst right before
        // its `insert`; a store-then-drain here guarantees an insert
        // that is about to land either (a) already saw the flag and
        // abandoned in-place, or (b) lands in the map and is swept by
        // one of the two drains below.
        self.abandoning
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Keep a backend Arc per abandoned sandbox: after the serve
        // loop dies the dirty tier is FROZEN (later guest writes park
        // in the kernel's dead-conn window for the successor to
        // replay), which makes post-abandon the one race-free moment
        // to export un-uploaded chunks to the shutdown spool
        // (2026-07-16 session-85e0298a RCA — pre-spool, these acked
        // writes died with the process and the successor rolled the
        // live guest's disk back under it).
        let mut frozen: Vec<(SandboxId, Arc<crate::disk_daemon::ChunkedDiskBackend>)> = Vec::new();
        let drain = |frozen: &mut Vec<_>| {
            let ids: Vec<_> = self.nbd_sandboxes.iter().map(|e| *e.key()).collect();
            for id in ids {
                if let Some((_, state)) = self.nbd_sandboxes.remove(&id) {
                    let backend = state.backend.clone();
                    state.abandon_for_shutdown();
                    frozen.push((id, backend));
                }
            }
        };
        drain(&mut frozen);
        // Belt-and-braces second pass: catches a state inserted
        // between the flag store and the first drain's snapshot.
        drain(&mut frozen);
        let abandoned = frozen.len();

        let Some(spool_root) = self.shutdown_spool_root() else {
            for (sandbox_id, backend) in &frozen {
                let dirty = backend.dirty_bytes().await;
                if dirty > 0 {
                    tracing::error!(
                        %sandbox_id,
                        dirty_bytes = dirty,
                        "shutdown abandon: un-uploaded dirty bytes and NO spool root \
                         (checkpoint_dir unset); the successor will roll back these \
                         acked guest writes",
                    );
                }
            }
            return abandoned;
        };
        for (sandbox_id, backend) in frozen {
            let (manifest_ref, chunks) = backend.export_unflushed().await;
            // Written even when `chunks` is empty: a zero-chunk spool still
            // carries the manifest ref, which covers the flush-succeeded-but-
            // coord-publish-failed shutdown — the chunks and manifest are
            // durable in the blob store under a version coord never heard
            // about, and the successor must attach from THAT ref (the spool's
            // store-ahead rule), not roll back to coord's stale one.
            match crate::disk_daemon::spool::write_spool(
                self.host_fs.as_ref(),
                &spool_root,
                sandbox_id,
                manifest_ref,
                &chunks,
            )
            .await
            {
                Ok(bytes) => tracing::info!(
                    %sandbox_id,
                    chunks = chunks.len(),
                    bytes,
                    manifest = %manifest_ref,
                    "shutdown abandon: un-uploaded dirty chunks preserved in the \
                     local spool for the successor to adopt",
                ),
                Err(e) => tracing::error!(
                    %sandbox_id,
                    chunks = chunks.len(),
                    error = %e,
                    "shutdown abandon: SPOOL WRITE FAILED; the successor will roll \
                     back these acked guest writes",
                ),
            }
        }
        abandoned
    }

    /// Node-local root for the shutdown spool (un-uploaded dirty
    /// chunks handed from a dying host-agent generation to its
    /// successor). Lives under `checkpoint_dir` — the same hostPath
    /// volume the checkpoint chain records already rely on surviving
    /// pod rolls. `None` ⟺ checkpointing is disabled (dev/tests).
    #[cfg(target_os = "linux")]
    fn shutdown_spool_root(&self) -> Option<std::path::PathBuf> {
        self.checkpoint_dir.as_ref().map(|d| d.join("spool"))
    }

    /// Issue #224: whether the terminal shutdown-abandon mode is
    /// engaged. Insert sites consult this immediately before adding a
    /// freshly-built `NbdSandboxState` to `nbd_sandboxes`; when set
    /// they `abandon_for_shutdown()` the state instead so the
    /// successor host-agent generation keeps the kernel-side device.
    #[cfg(target_os = "linux")]
    fn is_abandoning(&self) -> bool {
        self.abandoning.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Attach a chunk store + a per-host directory where chunked
    /// manifests get materialized. Once set, `create()` resolves the
    /// cached image's `bundle.json` → manifest → file via the chunk
    /// store, bypassing the OCI-pulled `rootfs.ext4`. Required for
    /// chunked storage to actually flow through dev / production
    /// sessions.
    pub fn with_chunk_store(mut self, chunk_store: ChunkStore, materialize_dir: PathBuf) -> Self {
        self.chunk_store = Some(chunk_store);
        self.materialize_dir = Some(materialize_dir);
        self
    }

    /// ADR 0080 §C: attach the image-materialize scratch root (the
    /// `MaterializeImage` RPC's working space; same volume as the
    /// chunk cache so the statvfs headroom check measures the disk
    /// that actually fills). Without it, `materialize_image` errors
    /// InvalidSpec — this host can't take enable-time materializes.
    pub fn with_materialize_scratch(mut self, dir: PathBuf) -> Self {
        self.materialize_scratch = Some(dir);
        self
    }

    /// Attach an NVMe-backed `ChunkCache`. Optional; chains on top
    /// of `with_chunk_store`. Production hosts wire one to
    /// amortise repeated chunk reads across manifests; dev /
    /// single-host setups can skip it without breaking the
    /// materialization path.
    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Attach an image cache. Calling this after `new()` lets
    /// `create()` resolve `spec.image_uri` to a cached on-disk path
    /// before forwarding to the inner backend.
    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// ADR 0008 Phase 5: attach an `OciClient` to enable the
    /// chunks-in-OCI fault path. When set together with a chunk
    /// store, `resolve_rootfs` upgrades chunked-OCI images
    /// (`CachedImage::is_disk_chunked_oci()`) from BlobStorage-only
    /// to a tiered `BlobStorage → OCI` fault path with
    /// write-through fill to BlobStorage. Without this, chunked-OCI
    /// images degrade to legacy behavior — chunks must live in the
    /// host's BlobStorage namespace or session-create fails on
    /// first chunk fault.
    pub fn with_oci_client(mut self, client: engram_oci::OciClient) -> Self {
        self.oci_client = Some(client);
        self
    }

    /// Attach a host-egress proxy. Once set, incoming
    /// `notify_session_policy` calls register against this proxy's
    /// registry and `destroy` unregisters. The CA PEM is available
    /// to substrate-builders via `host_egress.ca_cert_pem`.
    pub fn with_egress(mut self, egress: Arc<HostEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Resolve the disk for a cached image. Three branches in
    /// priority order:
    ///
    /// 1. **NBD path** (Linux + `nbd_pool` + `chunk_store` +
    ///    `chunk_cache` + `bundle.disk_manifest` all present):
    ///    spawn an NBD daemon serving the chunk manifest, return
    ///    `(/dev/nbdN, Some(NbdSandboxState))`. The state is held
    ///    by `create()` and stashed in `nbd_sandboxes` keyed by
    ///    the eventual SandboxId.
    /// 2. **Materialize-to-file** (chunk_store + materialize_dir +
    ///    bundle): walk the chunk manifest into a single ext4
    ///    file at `<materialize_dir>/<manifest_id>-v<n>.ext4`,
    ///    return that path. The legacy chunked path; still the
    ///    only option on macOS or when NBD isn't wired.
    /// 3. **OCI fallback**: hand back the OCI-pulled `rootfs.ext4`
    ///    unchanged. The pre-chunked-storage path; retiring as
    ///    every host gets a chunk_store.
    async fn resolve_rootfs(
        &self,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<(PathBuf, NbdStateSlot), SandboxError> {
        // ADR 0008 Phase 5 final piece: chunked-OCI images carry
        // the `Manifest` object only in the bake's BlobStorage
        // namespace. The runtime host's namespace may be empty.
        // Synthesize the manifest from the bootstrap sidecar (which
        // arrived with the OCI pull) and persist it before either
        // disk-resolution branch reads it. Idempotent — skips when
        // the manifest is already present.
        if let Some(cs) = self.chunk_store.as_ref() {
            ensure_chunked_manifest_in_blob_storage(cs, cached)
                .await
                .map_err(|e| {
                    tracing::error!(uri = %uri, error = %e, "chunked-OCI manifest synthesis failed");
                    e
                })?;
        }

        // Branch 1: NBD daemon.
        #[cfg(target_os = "linux")]
        if let Some(state) = self.try_spawn_nbd(uri, cached).await? {
            tracing::info!(
                uri = %uri,
                device = %state.device_path().display(),
                "rootfs branch: NBD daemon",
            );
            return Ok((state.device_path().to_path_buf(), Some(state)));
        }

        // Branch 2: materialize-to-file (the chunked path).
        let (chunk_store, materialize_dir) = match (
            self.chunk_store.as_ref(),
            self.materialize_dir.as_ref(),
        ) {
            (Some(cs), Some(dir)) => (cs, dir),
            _ => {
                let path = legacy_rootfs_path(cached)?;
                tracing::debug!(uri = %uri, path = %path.display(), "rootfs branch: legacy ext4 (no chunk store wired)");
                return Ok((path, nbd_state_none()));
            }
        };
        let bundle = match cached.bundle.as_ref() {
            Some(b) => b,
            None => {
                let path = legacy_rootfs_path(cached)?;
                tracing::debug!(uri = %uri, path = %path.display(), "rootfs branch: legacy ext4 (no bundle)");
                return Ok((path, nbd_state_none()));
            }
        };

        // ADR 0008 Phase 5: when the image is chunked-OCI shaped AND
        // an `OciClient` is wired, build a per-call `ChunkStore`
        // with a `TieredChunkResolver` so chunks missing from
        // BlobStorage fall through to OCI Range GET. Successful OCI
        // fetches tee back into BlobStorage (CDN-fill) so subsequent
        // sessions on the same host hit the cache tier.
        //
        // For non-chunked-OCI images (legacy ADR 0007 path,
        // BlobStorage-only) we use the default chunk_store as-is.
        let effective_store = self
            .upgrade_chunk_store_for_chunked_oci(chunk_store, uri, cached)
            .await
            .map_err(|e| {
                tracing::error!(uri = %uri, error = %e, "chunk-store upgrade for chunked-OCI failed");
                e
            })?;
        tracing::debug!(
            uri = %uri,
            manifest = %bundle.disk_manifest,
            chunked_oci = cached.is_disk_chunked_oci(),
            "rootfs branch: materialize-to-file",
        );

        // ADR 0008: chunk reads inside `materialize_to_file_cached`
        // route through `effective_store` (the per-session tiered
        // store) via the cache's closure-based fetcher. The cache
        // itself is backend-agnostic — no rebind needed.
        let path = materialize_chunked_rootfs(
            &effective_store,
            self.chunk_cache.as_ref(),
            materialize_dir,
            uri,
            bundle.disk_manifest,
            &self.materialize_lock,
        )
        .await?;
        Ok((path, nbd_state_none()))
    }

    /// ADR 0028 Fix B: resolve the root disk from an explicit chunked
    /// manifest (`spec.rootfs_manifest`) instead of an image — the
    /// disk-only cold-boot recovery shape, where a fresh kernel mounts
    /// a session's evolved `live_disk_manifest`. NBD-attached when the
    /// host has the prereqs (prod FC hosts always do), so the
    /// continuous-flush scheduler continues the SAME manifest lineage
    /// the recovery booted from; materialize-to-file otherwise
    /// (dev/VZ parity — those hosts don't track flushes for any
    /// sandbox, so the recovered session degrades identically).
    async fn resolve_rootfs_from_manifest(
        &self,
        manifest_ref: engram_core::types::manifest::ManifestRef,
    ) -> Result<(PathBuf, NbdStateSlot), SandboxError> {
        #[cfg(target_os = "linux")]
        if let (Some(pool), Some(store), Some(cache)) = (
            self.nbd_pool.as_ref(),
            self.chunk_store.as_ref(),
            self.chunk_cache.as_ref(),
        ) {
            let store_arc = Arc::new(store.clone());
            let state = crate::disk_daemon::attach_manifest(
                manifest_ref,
                cache.clone(),
                store_arc,
                pool,
                self.flush_config.dirty_threshold_bytes,
                // Disk-only cold recovery resumes the session's OWN evolved
                // manifest — already a private id, so tick, don't fork.
                /*fork_at_attach=*/
                false,
            )
            .await
            .map_err(|e| SandboxError::Vm(format!("rootfs-manifest NBD attach: {e}").into()))?;
            tracing::info!(
                manifest = %manifest_ref,
                device = %state.device_path().display(),
                "rootfs branch: NBD daemon (explicit manifest override)",
            );
            return Ok((state.device_path().to_path_buf(), Some(state)));
        }

        let (chunk_store, materialize_dir) =
            match (self.chunk_store.as_ref(), self.materialize_dir.as_ref()) {
                (Some(cs), Some(dir)) => (cs, dir),
                _ => {
                    return Err(SandboxError::InvalidSpec(
                        "spec.rootfs_manifest is set but this host has neither an NBD \
                     pool nor a chunk store + materialize dir wired"
                            .into(),
                    ));
                }
            };
        tracing::info!(
            manifest = %manifest_ref,
            "rootfs branch: materialize-to-file (explicit manifest override)",
        );
        let path = materialize_chunked_rootfs(
            chunk_store,
            self.chunk_cache.as_ref(),
            materialize_dir,
            "rootfs-manifest-override",
            manifest_ref,
            &self.materialize_lock,
        )
        .await?;
        Ok((path, nbd_state_none()))
    }

    /// ADR 0008 Phase 5: upgrade the base `ChunkStore` to a tiered
    /// one when the cached image carries Nydus-shaped chunked OCI
    /// layers AND an `OciClient` is configured. Returns the
    /// original store unchanged otherwise — non-chunked-OCI images
    /// don't need the OCI tier.
    ///
    /// Errors here surface from bootstrap parsing (corrupt sidecar)
    /// or the chunk-store's blob_storage handle; both indicate a
    /// real failure mode worth aborting on.
    async fn upgrade_chunk_store_for_chunked_oci(
        &self,
        base: &ChunkStore,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<ChunkStore, SandboxError> {
        let oci = match self.oci_client.as_ref() {
            Some(c) => c,
            None => return Ok(base.clone()),
        };
        if !cached.is_disk_chunked_oci() {
            return Ok(base.clone());
        }
        let index = cached
            .build_oci_chunk_index()
            .await
            .map_err(|e| SandboxError::Vm(format!("chunked-OCI index: {e}").into()))?;
        let index = match index {
            Some(i) => i,
            None => return Ok(base.clone()),
        };
        let blob = base.blob_storage();
        let cache_tier: Arc<dyn engram_chunk_store::ChunkResolver> =
            Arc::new(engram_chunk_store::BlobStorageResolver::new(blob.clone()));
        let origin_tier: Arc<dyn engram_chunk_store::ChunkResolver> = Arc::new(
            engram_oci::OciChunkResolver::new(oci.clone(), uri.to_string(), index),
        );
        let tiered: Arc<dyn engram_chunk_store::ChunkResolver> = Arc::new(
            engram_chunk_store::TieredChunkResolver::new(vec![cache_tier, origin_tier], Some(blob)),
        );
        tracing::debug!(
            uri = %uri,
            "chunked-OCI image: installing TieredChunkResolver for materialize",
        );
        Ok(base.clone().with_resolver(tiered))
    }

    /// Linux-only NBD spawn branch. Returns `None` when any
    /// prerequisite (pool / chunk_store / chunk_cache /
    /// bundle.disk_manifest) is missing. Caller falls back to
    /// materialize-to-file in that case.
    ///
    /// For chunked-OCI v2 images the store is wrapped with the
    /// same `TieredChunkResolver` the materialize-to-file branch
    /// uses, so NBD reads of chunks absent from BlobStorage fall
    /// through to OCI Range GET and tee back into BlobStorage.
    /// Without this the NBD daemon 404s forever on first-touch
    /// chunks.
    #[cfg(target_os = "linux")]
    async fn try_spawn_nbd(
        &self,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<Option<crate::disk_daemon::NbdSandboxState>, SandboxError> {
        let (pool, store, cache) = match (
            self.nbd_pool.as_ref(),
            self.chunk_store.as_ref(),
            self.chunk_cache.as_ref(),
        ) {
            (Some(p), Some(s), Some(c)) => (p, s, c),
            _ => return Ok(None),
        };
        let bundle = match cached.bundle.as_ref() {
            Some(b) => b,
            None => return Ok(None),
        };
        let effective_store = self
            .upgrade_chunk_store_for_chunked_oci(store, uri, cached)
            .await
            .map_err(|e| {
                tracing::error!(uri = %uri, error = %e, "chunk-store upgrade for chunked-OCI (NBD) failed");
                e
            })?;
        let store_arc = Arc::new(effective_store);
        let state = crate::disk_daemon::attach_manifest(
            bundle.disk_manifest,
            cache.clone(),
            store_arc,
            pool,
            self.flush_config.dirty_threshold_bytes,
            // ADR 0049 follow-up: FRESH create from the shared base image —
            // fork the disk manifest to a private per-session id on first write.
            /*fork_at_attach=*/
            true,
        )
        .await
        .map_err(|e| SandboxError::Vm(format!("nbd attach_manifest: {e}").into()))?;
        Ok(Some(state))
    }

    /// Materialise `<src>/memory.bin` from the FC sidecar JSON's
    /// `memory_manifest` (if both the manifest exists in the
    /// chunk store and the local file is absent). Used by
    /// `restore()` to bootstrap cross-host transfers — the
    /// snapshot directory may carry only `state.bin` +
    /// `manifest.json` when staged by the coord; this fills in
    /// the memory file from the chunked durability tier.
    ///
    /// Returns early-Ok on every common skip condition (no chunk
    /// store wired, sidecar absent, memory_manifest unset, file
    /// already present). Failures bubble; callers can log + fall
    /// through to inner.restore which will surface a clearer
    /// "memory.bin missing" error.
    /// ADR 0014 M1.13: parallel prefetch the memory manifest's
    /// chunks into the local NVMe chunk cache. Idempotent (no-op
    /// when the cache is already warm) and best-effort (errors
    /// degrade to the serial fault path inside materialize_to_file_cached).
    ///
    /// Concurrency is bounded at [`MEMORY_PREFETCH_CONCURRENCY`] (32).
    /// Single-stream GCS hits ~80 MB/s on the prod n2-standard-8 hosts;
    /// the prior bound of 8 (~640 MB/s) left most of the 10 Gbps line
    /// rate idle while the cold resume waited 66.5 s on this prefetch
    /// (traced). 32 (~2.5 GB/s aggregate) saturates closer to line rate
    /// without tripping GCS per-object rate limits, shrinking the
    /// cold-cache refill that gates resume. (ADR 0039 item #19 also
    /// flags moving this prefetch off the resume critical path entirely
    /// — see the design note; that's the higher-risk follow-up.)
    async fn prefetch_memory_chunks(
        &self,
        metadata: &SnapshotMetadata,
    ) -> Result<usize, SandboxError> {
        match (self.chunk_store.as_ref(), self.chunk_cache.as_ref()) {
            (Some(cs), Some(cache)) => {
                Self::prefetch_memory_chunks_inner(cs, cache, metadata).await
            }
            _ => Ok(0),
        }
    }

    /// Owner-agnostic body of [`Self::prefetch_memory_chunks`]. Takes the
    /// already-resolved store + cache (by ref) so it can run either inline
    /// (`await`, on a NON-LAZY File restore where the serial
    /// `materialize_memory_if_missing` reads the warmed cache) or inside a
    /// spawned background task (ADR 0043 P1, any lazy UFFD restore — the warmed
    /// chunks are consumed only by the handler's later lazy faults, so warming
    /// need not block restore).
    async fn prefetch_memory_chunks_inner(
        chunk_store: &ChunkStore,
        cache: &ChunkCache,
        metadata: &SnapshotMetadata,
    ) -> Result<usize, SandboxError> {
        let Some(mref) = metadata.memory_manifest else {
            return Ok(0);
        };
        // ADR 0014 M1.14: if the snapshot has a published working-
        // set trace, narrow the prefetch to those chunks only —
        // typically a small subset of the full manifest (~5-15
        // chunks vs ~30+). Shrinks cold-cache refill from ~2 s to
        // ~500 ms in the best case. Fallback to full-manifest
        // prefetch (M1.13 behavior) when the trace isn't present,
        // unreachable, or empty.
        let hashes_to_prefetch: Vec<_> = match metadata.working_set_blob_key.as_deref() {
            Some(ws_key) => match Self::fetch_working_set_chunks(chunk_store, ws_key).await {
                Ok(chunks) if !chunks.is_empty() => {
                    tracing::debug!(
                        ws_key,
                        chunk_count = chunks.len(),
                        "chunked restore prefetch narrowed to working set",
                    );
                    chunks
                }
                Ok(_) => {
                    tracing::debug!(
                        ws_key,
                        "chunked restore prefetch: working set empty, falling back to full manifest",
                    );
                    let manifest = chunk_store.get_manifest(mref).await.map_err(|e| {
                        SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
                    })?;
                    manifest.chunks.iter().map(|c| c.hash).collect()
                }
                Err(e) => {
                    tracing::warn!(
                        ws_key,
                        error = %e,
                        "chunked restore prefetch: working set fetch failed, falling back to full manifest",
                    );
                    let manifest = chunk_store.get_manifest(mref).await.map_err(|e| {
                        SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
                    })?;
                    manifest.chunks.iter().map(|c| c.hash).collect()
                }
            },
            None => {
                let manifest = chunk_store.get_manifest(mref).await.map_err(|e| {
                    SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
                })?;
                manifest.chunks.iter().map(|c| c.hash).collect()
            }
        };
        let chunk_count = hashes_to_prefetch.len();
        let store_for_fetch = chunk_store.clone();
        cache
            .prefetch_chunks_parallel(
                hashes_to_prefetch,
                MEMORY_PREFETCH_CONCURRENCY,
                move |hash| {
                    let s = store_for_fetch.clone();
                    async move { s.get_chunk(hash).await }
                },
            )
            .await
            .map_err(|e| SandboxError::Snapshot(format!("prefetch_chunks_parallel: {e}")))?;
        tracing::debug!(
            manifest = %mref,
            chunk_count,
            "chunked restore: memory chunks prefetched into NVMe",
        );
        Ok(chunk_count)
    }

    /// ADR 0014 M1.14: load a working_set.json from BlobStorage and
    /// return its chunk hashes. Used by `prefetch_memory_chunks`
    /// to narrow the parallel prefetch to the chunks the kernel
    /// actually touches on warm-restore. Returns `Ok(vec![])` when
    /// the blob is missing (older bakes that pre-date M1.14) so
    /// the caller falls back to full-manifest prefetch.
    async fn fetch_working_set_chunks(
        chunk_store: &ChunkStore,
        ws_key: &str,
    ) -> Result<Vec<engram_chunk_store::manifest::ChunkHash>, SandboxError> {
        let blob = chunk_store.blob_storage();
        let bytes = match blob.get(ws_key).await {
            Ok(b) => b,
            Err(engram_core::BlobError::NotFound) => return Ok(Vec::new()),
            Err(e) => {
                return Err(SandboxError::Snapshot(format!(
                    "fetch working-set blob {ws_key}: {e}"
                )))
            }
        };
        let trace: engram_chunk_store::working_set::WorkingSetTrace =
            serde_json::from_slice(&bytes).map_err(|e| {
                SandboxError::Snapshot(format!("parse working-set trace {ws_key}: {e}"))
            })?;
        Ok(trace.chunks)
    }

    async fn materialize_memory_if_missing(
        &self,
        src: &std::path::Path,
    ) -> Result<(), SandboxError> {
        let mem_path = src.join("memory.bin");
        if fs::metadata(&mem_path).await.is_ok() {
            return Ok(());
        }
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return Ok(());
        };
        let manifest_json = src.join("manifest.json");
        let Ok(bytes) = fs::read(&manifest_json).await else {
            return Ok(());
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Ok(());
        };
        let mref_value = value.get("memory_manifest").filter(|v| !v.is_null());
        let Some(mref_value) = mref_value else {
            return Ok(());
        };
        let mref: engram_core::types::manifest::ManifestRef =
            serde_json::from_value(mref_value.clone()).map_err(|e| {
                SandboxError::Snapshot(format!(
                    "parse memory_manifest from {}: {e}",
                    manifest_json.display(),
                ))
            })?;
        // Ensure the destination dir exists (cross-host stages may
        // hand us a path that's missing intermediate components).
        if let Some(parent) = mem_path.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                SandboxError::Snapshot(format!("create snapshot dir {}: {e}", parent.display(),))
            })?;
        }
        let manifest = chunk_store
            .get_manifest(mref)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("get_manifest {mref}: {e}")))?;
        match self.chunk_cache.as_ref() {
            Some(cache) => chunk_store
                .materialize_to_file_cached(&manifest, &mem_path, cache)
                .await
                .map_err(|e| {
                    SandboxError::Snapshot(format!("materialize memory.bin (cached): {e}"))
                })?,
            None => chunk_store
                .materialize_to_file(&manifest, &mem_path)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("materialize memory.bin: {e}")))?,
        }
        tracing::info!(
            manifest = %mref,
            path = %mem_path.display(),
            "materialised memory.bin from chunked memory manifest",
        );
        Ok(())
    }

    /// ADR 0014 issue #1/#2 + ADR 0028 addendum: tear down the LOCAL
    /// artifacts of a snapshot whose downstream pipeline failed (or
    /// whose successor `snapshot()` call is about to overwrite it).
    /// Removes only the per-snapshot local dir at
    /// `<work_dir>/sandboxes/snapshots/<snapshot_id>/` (4+ GiB on FC;
    /// the 99 GB `engrams-fc-xngk` host filled in ~12 min by leaking 25
    /// of these in 13 min) — host-disk hygiene, and rebuildable from
    /// BlobStorage on the next restore.
    ///
    /// It does NOT delete the per-snapshot BlobStorage objects
    /// (state.bin / sidecar.json / working_set.json) or the chunked
    /// manifests. Those are governed by the pin-set GC model
    /// (`coordinator::snapshot_blob_gc` for the portable blobs, the
    /// chunk GC for manifests): a blob is deleted only when no
    /// `snapshots` row references it, after a grace period. Deleting
    /// them inline here was the engine of the recurring
    /// "recoverable=true but blobs gone" brick — a concurrent producer
    /// (a periodic checkpoint vs an eviction sharing this one
    /// per-sandbox inflight slot) would abort a snapshot whose row was
    /// already recorded, deleting its durable blobs out from under a
    /// resume. Now no producer ever deletes a durable blob.
    ///
    /// Best-effort: a local-dir removal error is logged, never
    /// propagated. Clears the inflight tracking entry on entry so a
    /// double-call doesn't try to clean twice.
    async fn abort_prior_inflight_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let Some((_, snapshot_id)) = self.inflight_snapshots.remove(&id) else {
            return Ok(());
        };
        let dest = self.inner.snapshot_path_for(snapshot_id);
        match fs::remove_dir_all(&dest).await {
            Ok(()) => tracing::info!(
                sandbox_id = %id,
                snapshot_id = %snapshot_id,
                dest = %dest.display(),
                "abort_snapshot: removed local snapshot dir (blobs are GC-governed)",
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                sandbox_id = %id,
                snapshot_id = %snapshot_id,
                dest = %dest.display(),
                error = %e,
                "abort_snapshot: rm -rf of snapshot dir failed (best-effort)",
            ),
        }
        Ok(())
    }
}

/// ADR 0014 cross-host restore helper: download `state.bin` and the
/// FC sidecar JSON from `BlobStorage` into the snapshot staging
/// directory iff (a) the metadata carries portable blob keys and
/// (b) the local files aren't already there. Idempotent — same-
/// host restore is a pair of `fs::metadata` short-circuits.
async fn materialize_state_if_missing(
    blob: &dyn engram_core::traits::storage::BlobStorage,
    src: &std::path::Path,
    state_blob_key: Option<&str>,
    sidecar_blob_key: Option<&str>,
) -> Result<(), SandboxError> {
    // The dest paths mirror the inner FC backend's snapshot layout
    // (`<snapshot_dir>/{state.bin,manifest.json}`). Cross-host
    // restore may hand us a path whose parent doesn't exist yet.
    fs::create_dir_all(src).await.map_err(|e| {
        SandboxError::Snapshot(format!("create snapshot dir {}: {e}", src.display()))
    })?;
    // ADR 0101 A: the two artifacts are independent blob objects — fetch
    // concurrently (a cold cross-host resume previously paid the two
    // downloads back-to-back on its critical path).
    let state_path = src.join("state.bin");
    let sidecar_path = src.join("manifest.json");
    let state_fut = async {
        if let Some(key) = state_blob_key {
            if fs::metadata(&state_path).await.is_err() {
                engram_chunk_store::snapshot_blob::download_file(blob, key, &state_path)
                    .await
                    .map_err(|e| {
                        SandboxError::Snapshot(format!("download state.bin from {key}: {e}"))
                    })?;
                tracing::info!(
                    path = %state_path.display(),
                    key = key,
                    "materialised state.bin from BlobStorage",
                );
            }
        }
        Ok::<(), SandboxError>(())
    };
    let sidecar_fut = async {
        if let Some(key) = sidecar_blob_key {
            if fs::metadata(&sidecar_path).await.is_err() {
                engram_chunk_store::snapshot_blob::download_file(blob, key, &sidecar_path)
                    .await
                    .map_err(|e| {
                        SandboxError::Snapshot(format!("download sidecar.json from {key}: {e}"))
                    })?;
                tracing::info!(
                    path = %sidecar_path.display(),
                    key = key,
                    "materialised sidecar.json from BlobStorage",
                );
            }
        }
        Ok::<(), SandboxError>(())
    };
    let (state_res, sidecar_res) = tokio::join!(state_fut, sidecar_fut);
    state_res?;
    sidecar_res?;
    Ok(())
}

/// Cross-host chunked-rootfs materialization: rebuild `rootfs.ext4`
/// from chunks if it isn't already on disk, then patch the FC
/// sidecar's `spec.rootfs_source` to the local file path. Without
/// this, `restore_in_jail` installs the canonical-rootfs symlink
/// pointing at the bake-time path (`/tmp/.tmpXXX/...`) which
/// doesn't exist on the receiver, and FC `load_snapshot` errors
/// with "Block: Virtio backend error: No such file or directory".
///
/// Reached today by the ADR 0018 dead-host evac path (source FC is
/// gone; the snapshot lives in BlobStorage and the rootfs must be
/// rebuilt locally before `load_snapshot`) and by any /resume that
/// lands on a host that didn't capture the snapshot. The original
/// caller — ADR 0014's warm-pool cross-host refill — was retired
/// with ADR 0015 M5, but the same primitive serves the current
/// restore shapes unchanged.
///
/// Called from `PooledBackend::restore` after `materialize_state_if_missing`
/// (which downloads the sidecar that this function then patches).
/// No-op when `metadata.disk_manifest` is None (legacy snapshots)
/// or when the local rootfs file already exists at the canonical
/// location (idempotent same-host resume).
async fn materialize_disk_if_missing(
    chunk_store: &ChunkStore,
    chunk_cache: Option<&ChunkCache>,
    src: &std::path::Path,
    disk_manifest: Option<engram_core::types::manifest::ManifestRef>,
    materialize_lock: &Mutex<()>,
) -> Result<(), SandboxError> {
    let Some(disk_ref) = disk_manifest else {
        return Ok(());
    };
    // Use the same content-addressed file name the cold-create path
    // uses, in the same per-snapshot scratch dir, so a future
    // `image_cache.ensure_image` materialization shares the file
    // (and same-host resume short-circuits).
    let local_rootfs = src.join(format!(
        "{}-v{}.ext4",
        disk_ref.manifest_id, disk_ref.version
    ));
    fs::create_dir_all(src).await.map_err(|e| {
        SandboxError::Snapshot(format!("create snapshot dir {}: {e}", src.display()))
    })?;
    let _guard = materialize_lock.lock().await;
    let manifest = chunk_store
        .get_manifest(disk_ref)
        .await
        .map_err(|e| SandboxError::Snapshot(format!("get rootfs manifest {disk_ref}: {e}")))?;
    if let Ok(meta) = fs::metadata(&local_rootfs).await {
        if meta.len() == manifest.total_bytes {
            tracing::debug!(
                path = %local_rootfs.display(),
                manifest = %disk_ref,
                "rootfs already materialised; skipping rebuild",
            );
        }
    }
    if fs::metadata(&local_rootfs)
        .await
        .map(|m| m.len() != manifest.total_bytes)
        .unwrap_or(true)
    {
        match chunk_cache {
            Some(cache) => chunk_store
                .materialize_to_file_cached(&manifest, &local_rootfs, cache)
                .await
                .map_err(|e| {
                    SandboxError::Snapshot(format!(
                        "materialize rootfs (cached) {}: {e}",
                        local_rootfs.display()
                    ))
                })?,
            None => chunk_store
                .materialize_to_file(&manifest, &local_rootfs)
                .await
                .map_err(|e| {
                    SandboxError::Snapshot(format!(
                        "materialize rootfs {}: {e}",
                        local_rootfs.display()
                    ))
                })?,
        }
        tracing::info!(
            path = %local_rootfs.display(),
            manifest = %disk_ref,
            size_bytes = manifest.total_bytes,
            "materialised rootfs.ext4 from chunked disk manifest",
        );
    }

    // Patch the FC sidecar's `spec.rootfs_source` to point at the
    // just-materialized file. ADR 0016 Phase B commit 5 factored
    // this out into a shared helper so the resume-path NBD attach
    // (which has no local file but needs the same sidecar patch
    // pointing at /dev/nbdN) can call it without duplicating the
    // serde + canonicalize ceremony.
    //
    // Must be absolute: the canonical-rootfs symlink lives at the
    // bake's `/tmp/.tmpXXX/rootfs/<src>.dev`, so a relative target
    // resolves against `/tmp/.tmpXXX/rootfs/` and points at nothing.
    let local_rootfs = fs::canonicalize(&local_rootfs).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "canonicalize materialised rootfs {}: {e}",
            local_rootfs.display()
        ))
    })?;
    patch_sidecar_rootfs_source(src, &local_rootfs).await?;

    Ok(())
}

/// ADR 0016 Phase B commit 5 — patch the FC sidecar's
/// `spec.rootfs_source` to `target`. Shared by:
///
/// - `materialize_disk_if_missing` (legacy path): `target` is the
///   freshly-written `<src>/<manifest_id>-vN.ext4` flat file.
/// - `prepare_resume_nbd_attach` (Phase B path): `target` is the
///   `/dev/nbdN` device served by the chunked-disk daemon.
///
/// `restore_canonical_symlinks` inside FC's `inner.restore` reads
/// this field and installs two symlinks (host's `<work_dir>/rootfs/
/// <new_sandbox_id>.dev` AND the source-keyed path FC's `state.bin`
/// embeds at bake time). Both point at `target`. FC's `load_snapshot`
/// then reads its rootfs through whichever symlink it dereferences —
/// either a flat file or the NBD device.
///
/// Pre-Phase-B, the symlink target was always a flat file. Phase B
/// makes the chunked-disk lineage land at `/dev/nbdN` instead,
/// rejoining the resumed sandbox with `nbd_sandboxes` tracking.
async fn patch_sidecar_rootfs_source(
    src: &std::path::Path,
    target: &std::path::Path,
) -> Result<(), SandboxError> {
    let sidecar_path = src.join("manifest.json");
    let bytes = fs::read(&sidecar_path).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "read fc manifest.json {}: {e}",
            sidecar_path.display()
        ))
    })?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        SandboxError::Snapshot(format!(
            "parse fc manifest.json {}: {e}",
            sidecar_path.display()
        ))
    })?;
    let spec = value
        .get_mut("spec")
        .and_then(|s| s.as_object_mut())
        .ok_or_else(|| SandboxError::Snapshot("fc sidecar spec is not an object".into()))?;
    spec.insert(
        "rootfs_source".into(),
        serde_json::Value::String(target.to_string_lossy().into_owned()),
    );
    let patched = serde_json::to_vec_pretty(&value)
        .map_err(|e| SandboxError::Snapshot(format!("serialize patched sidecar: {e}")))?;
    fs::write(&sidecar_path, patched).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "write patched sidecar to {}: {e}",
            sidecar_path.display()
        ))
    })?;
    Ok(())
}

/// Read the FC sidecar's `spec.rootfs_source` back off disk — the inverse
/// of [`patch_sidecar_rootfs_source`]'s write, reading the SAME
/// `<src>/manifest.json`. Used by the resume path's D4 guard to detect a
/// snapshot whose sidecar still names a capture-time literal `/dev/nbdN`
/// device that no NBD attach replaced. Best-effort: a missing/unparseable
/// sidecar or an absent field returns `None` (the guard then does not
/// fire, and the normal restore proceeds — a malformed sidecar fails later
/// in FC's own restore with its own error).
async fn read_sidecar_rootfs_source(src: &std::path::Path) -> Option<String> {
    let bytes = fs::read(src.join("manifest.json")).await.ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("spec")
        .and_then(|s| s.get("rootfs_source"))
        .and_then(|r| r.as_str())
        .map(str::to_owned)
}

/// ADR 0008 Phase 5 final piece: ensure the disk `Manifest`
/// referenced by `cached.bundle.disk_manifest` is reachable in
/// the host's `BlobStorage`. For chunked-OCI images, the bake-
/// side wrote the Manifest to its own BlobStorage namespace; the
/// runtime host's namespace may be empty. We synthesize from the
/// bootstrap sidecar (which arrived with the OCI pull) and
/// persist via `put_manifest`. Idempotent — skips when the
/// manifest is already in BlobStorage, or when the artifact is
/// not chunked-OCI shaped (legacy bundle.json / no bundle).
///
/// Called from `resolve_rootfs` before either disk-resolution
/// branch (NBD or materialize-to-file) reads the manifest.
pub async fn ensure_chunked_manifest_in_blob_storage(
    chunk_store: &ChunkStore,
    cached: &CachedImage,
) -> Result<(), SandboxError> {
    // Three short-circuits — non-chunked-OCI images don't need
    // this path at all.
    let Some(bundle) = cached.bundle.as_ref() else {
        return Ok(());
    };
    let Some(bs_path) = cached.disk_bootstrap_path.as_ref() else {
        return Ok(());
    };
    let manifest_ref = bundle.disk_manifest;

    // Already there? Most common case after the first session on a
    // given image: cheap stat-shape lookup, no-op.
    match chunk_store.get_manifest(manifest_ref).await {
        Ok(_) => return Ok(()),
        Err(engram_chunk_store::ChunkStoreError::Blob(engram_core::error::BlobError::NotFound)) => {
        }
        Err(e) => {
            tracing::error!(
                manifest_ref = %manifest_ref,
                error = %e,
                "chunked-OCI: probing for existing manifest hit unexpected error",
            );
            return Err(SandboxError::Vm(
                format!("probe manifest {manifest_ref}: {e}").into(),
            ));
        }
    }

    // Synthesize from the bootstrap on disk and persist. The
    // bootstrap was written by `pull_image` (`bootstrap.disk.json`
    // sidecar); it carries every byte of Manifest info we need.
    let bytes = fs::read(bs_path).await.map_err(|e| {
        tracing::error!(path = %bs_path.display(), error = %e, "chunked-OCI: read bootstrap failed");
        SandboxError::Vm(format!("read bootstrap {}: {e}", bs_path.display()).into())
    })?;
    let bootstrap: engram_chunk_store::Bootstrap = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::error!(path = %bs_path.display(), error = %e, "chunked-OCI: parse bootstrap failed");
        SandboxError::Vm(format!("parse bootstrap {}: {e}", bs_path.display()).into())
    })?;
    let synthesized = bootstrap.to_manifest();
    tracing::info!(
        manifest_ref = %manifest_ref,
        chunks = synthesized.chunks.len(),
        total_bytes = synthesized.total_bytes,
        kind = ?synthesized.kind,
        "chunked-OCI: synthesizing manifest from bootstrap + persisting to BlobStorage",
    );
    match chunk_store.put_manifest(manifest_ref, &synthesized).await {
        Ok(_) => Ok(()),
        // VersionConflict means another concurrent caller raced us
        // to put the same manifest. Synthesis is deterministic
        // (same bootstrap → same chunk list at same offsets), so
        // their version is identical to ours. Treat as success.
        Err(engram_chunk_store::ChunkStoreError::VersionConflict { .. }) => {
            tracing::debug!(
                manifest_ref = %manifest_ref,
                "chunked-OCI: concurrent put_manifest beat us; using their copy",
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                manifest_ref = %manifest_ref,
                error = %e,
                "chunked-OCI: put_manifest of synthesized manifest failed",
            );
            Err(SandboxError::Vm(
                format!("persist synthesized manifest {manifest_ref}: {e}").into(),
            ))
        }
    }
}

/// Materialize a chunked manifest to a content-addressed file under
/// `materialize_dir`. The output path is named by
/// `manifest_id-v{version}.ext4` so two sessions referencing the same
/// manifest deterministically land at the same file (the inner backend
/// can then APFS-clonefile or NBD-attach on top). Idempotent: returns
/// fast when the file already exists with the expected size.
async fn materialize_chunked_rootfs(
    chunk_store: &ChunkStore,
    chunk_cache: Option<&ChunkCache>,
    materialize_dir: &std::path::Path,
    uri: &str,
    manifest_ref: engram_core::types::manifest::ManifestRef,
    materialize_lock: &Mutex<()>,
) -> Result<PathBuf, SandboxError> {
    fs::create_dir_all(materialize_dir).await.map_err(|e| {
        tracing::error!(
            dir = %materialize_dir.display(),
            error = %e,
            "materialize_chunked_rootfs: create dir failed",
        );
        SandboxError::Vm(format!("materialize dir: {e}").into())
    })?;
    let dest = materialize_dir.join(format!(
        "{}-v{}.ext4",
        manifest_ref.manifest_id, manifest_ref.version,
    ));

    // Fast-path: file exists at the expected size. We trust local-
    // disk integrity here — the chunk store's content addressing
    // already validates bytes on the way in.
    let manifest = chunk_store.get_manifest(manifest_ref).await.map_err(|e| {
        tracing::error!(
            uri = %uri,
            manifest_ref = %manifest_ref,
            error = %e,
            "materialize_chunked_rootfs: get_manifest failed",
        );
        SandboxError::Vm(format!("get manifest {manifest_ref}: {e}").into())
    })?;
    if let Ok(meta) = fs::metadata(&dest).await {
        if meta.len() == manifest.total_bytes {
            tracing::debug!(
                uri = %uri,
                manifest = %manifest_ref,
                path = %dest.display(),
                "chunked rootfs already materialized",
            );
            return Ok(dest);
        }
    }

    // Slow-path: hold the lock so two concurrent creates against the
    // same manifest don't both walk the chunks. The second waiter
    // sees the file once the first releases and short-circuits via
    // the fast-path check we redo below.
    let _guard = materialize_lock.lock().await;
    if let Ok(meta) = fs::metadata(&dest).await {
        if meta.len() == manifest.total_bytes {
            return Ok(dest);
        }
    }

    tracing::info!(
        uri = %uri,
        manifest = %manifest_ref,
        chunks = manifest.chunks.len(),
        total_bytes = manifest.total_bytes,
        path = %dest.display(),
        cached = chunk_cache.is_some(),
        "materializing chunked rootfs",
    );
    // Route chunk reads through the local NVMe cache when one is
    // attached. Without it we re-fetch from BlobStorage every
    // materialize even when chunks haven't changed across
    // manifests.
    match chunk_cache {
        Some(cache) => {
            chunk_store
                .materialize_to_file_cached(&manifest, &dest, cache)
                .await
        }
        None => chunk_store.materialize_to_file(&manifest, &dest).await,
    }
    .map_err(|e| {
        tracing::error!(
            uri = %uri,
            manifest_ref = %manifest_ref,
            dest = %dest.display(),
            chunks = manifest.chunks.len(),
            total_bytes = manifest.total_bytes,
            error = %e,
            "materialize_chunked_rootfs: materialize_to_file failed",
        );
        SandboxError::Vm(format!("materialize {manifest_ref}: {e}").into())
    })?;
    Ok(dest)
}

/// Chunk an FC `memory.bin` snapshot file into the chunk store
/// and commit a fresh memory manifest. Returns the new
/// `ManifestRef` so callers can attach it to the FC sidecar JSON
/// and `SnapshotMetadata`.
///
/// Uses [`engram_chunk_store::ChunkStore::chunk_file`] with the
/// default 512 KiB memory chunk size. Sparse / all-zero blocks
/// are omitted from the manifest (the chunk_file primitive already
/// short-circuits them); on restore the resolver treats absent
/// chunk entries as zero-fill (UFFDIO_ZEROPAGE) so zero pages cost
/// ADR 0045 D5: what `capture_phase` hands the post phase.
pub(crate) struct SnapshotCapture {
    metadata: SnapshotMetadata,
    dest: PathBuf,
    chain_prev: Option<(
        engram_core::types::manifest::ManifestRef,
        engram_chunk_store::Manifest,
    )>,
    paused_at: chrono::DateTime<chrono::Utc>,
    /// Issue #202: the armed capture-unwind guard. Owns the drained
    /// `PendingDiskFlush` (Linux) and resumes the guest if this capture
    /// is dropped before the finisher takes it over. The finisher
    /// extracts the pending and defuses it.
    unwind: CaptureUnwind,
}

/// Incident 2026-07-10: FC's `PUT /snapshot/create` (Diff) consumes —
/// and resets — the KVM dirty-page bitmap the moment it runs. If
/// anything after that point fails (the create itself mid-write, the
/// sparse re-chunk, the eviction finalize-record persist), the consumed
/// dirty set is GONE: the chain's manifest never advanced, so a
/// subsequent Diff against the stale chain entry would silently exclude
/// every page the failed capture consumed — and a guest restored from
/// that chain reads pre-capture bytes at those pages (memory
/// corruption). Drop the chain entry instead: the next capture finds no
/// chain and takes a FULL snapshot — slower, correct. (The eviction
/// flavor is immune only AFTER its finalize record is durable; before
/// that instant it has the same hole, hence the `snapshot_begin` call
/// sites.)
fn poison_checkpoint_chain_after_failed_diff(
    chains: &DashMap<SandboxId, crate::checkpoint::CheckpointChain>,
    chain_heads: Option<&crate::checkpoint::ChainHeadStore>,
    id: SandboxId,
    failed_step: &str,
) {
    // The durable chain-head record was already write-ahead-removed
    // before the FC create (capture_phase / migration_capture), so this
    // is the defensive double-unlink — the poison must never leave a
    // record a post-roll rehydrate could seed from. (It also bumps the
    // store epoch, fencing any straggling persist.)
    if let Some(store) = chain_heads {
        store.remove_best_effort(id);
    }
    if chains.remove(&id).is_some() {
        metrics::counter!(crate::metrics::CHECKPOINT_CHAIN_POISONED_TOTAL).increment(1);
        tracing::warn!(
            sandbox_id = %id,
            failed_step,
            "diff capture failed after FC consumed the dirty bitmap; \
             checkpoint chain dropped — next capture will be a FULL snapshot",
        );
    }
}

/// ADR 0088 addendum: a snapshot whose `capture_phase` completed
/// synchronously but whose `finish()` runs on a spawned task — see
/// [`PooledBackend::snapshot_deferred`] for the contract (always
/// `join()`, never drop).
pub(crate) struct DeferredSnapshot {
    handle: tokio::task::JoinHandle<Result<SnapshotMetadata, SandboxError>>,
}

impl DeferredSnapshot {
    pub(crate) async fn join(self) -> Result<SnapshotMetadata, SandboxError> {
        match self.handle.await {
            Ok(result) => result,
            Err(e) => Err(SandboxError::Snapshot(format!(
                "deferred snapshot finish task died: {e}"
            ))),
        }
    }
}

/// ADR 0045 D5: see [`PooledBackend::finisher`]. Owns Arc-clones of the
/// fields the snapshot post phase + chain bookkeeping touch, so the phase
/// can run detached from the originating RPC.
#[derive(Clone)]
pub(crate) struct SnapshotFinisher {
    #[cfg(target_os = "linux")]
    nbd_sandboxes: Arc<DashMap<SandboxId, crate::disk_daemon::NbdSandboxState>>,
    chunk_store: Option<ChunkStore>,
    chunk_cache: Option<ChunkCache>,
    bundle_dir: PathBuf,
    /// The backend's staged-bundle extension (squashfs on both backends), so `publish`
    /// opens the SAME staged filename the backend attaches. Copied from
    /// `SandboxBackend::bundle_file_ext` at construction (like `bundle_dir`).
    bundle_file_ext: &'static str,
    inflight_snapshots: Arc<DashMap<SandboxId, engram_core::types::SnapshotId>>,
    last_snapshot_unix_ms: Arc<DashMap<SandboxId, i64>>,
    /// ADR 0101 B: last completed epoch's pacing sample, written by the
    /// diff arm of the post phase; read by the adaptive checkpoint
    /// controller (`checkpoint_candidates_adaptive`).
    checkpoint_pacing: Arc<DashMap<SandboxId, EpochPacingSample>>,
    checkpoint_chains: Arc<DashMap<SandboxId, crate::checkpoint::CheckpointChain>>,
    checkpoint_dir: Option<PathBuf>,
    chain_heads: Option<Arc<crate::checkpoint::ChainHeadStore>>,
    session_bindings: Arc<DashMap<SandboxId, SessionId>>,
    /// ADR 0098 D1: cloned from the owning `PooledBackend` — the chain-head
    /// record's `updated_at` reads through the injected clock.
    clock: Arc<dyn engram_core::traits::Clock>,
}

/// ADR 0101 B: what the last capture observed about a sandbox's memory
/// dirty rate — the adaptive checkpoint controller's only input.
/// `dirty_bytes: None` means the capture carried no rate signal (a Full
/// capture, or a diff-less flavor) — the controller then falls back to
/// the max-interval backstop.
#[derive(Clone, Copy, Debug)]
pub struct EpochPacingSample {
    pub dirty_bytes: Option<u64>,
    pub epoch: std::time::Duration,
}

impl SnapshotFinisher {
    /// ADR 0045 D5 + issue #147: the post phase — disk upload, memory
    /// re-chunk (parallel, `buffer_unordered(32)` inside the chunk
    /// store), portable-blob upload, bundle publish, then chain
    /// bookkeeping / cleanup. Instrumented end-to-end (the re-chunk used
    /// to be invisible in traces).
    pub(crate) async fn finish(
        &self,
        id: SandboxId,
        cap: SnapshotCapture,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let finish_start = crate::time_source::metrics_now();
        let flavor = if cap.chain_prev.is_some() {
            "diff"
        } else {
            "full"
        };
        // Filled by the diff branch below; consumed by the chain
        // advance after the post-processing block succeeds.
        let mut next_manifest_for_chain: Option<engram_chunk_store::Manifest> = None;
        // ADR 0101 B: set by the diff arm below; `None` for Full /
        // diff-less flavors (no rate signal — see `EpochPacingSample`).
        let mut epoch_dirty_bytes: Option<u64> = None;

        // ADR 0014 cleanup hygiene: from here on, FC has materialised
        // state.bin + memory.bin in `dest` (4+ GiB). Any failure in
        // the post-inner steps below (chunking, sidecar patch, BlobStorage
        // upload, NBD version conflict surfaced via the caller's
        // earlier flush) MUST rm -rf `dest` before propagating, or
        // we leak 4 GiB per failure — idle-evict retries every ~30s
        // and fills the host disk inside an hour.
        let metadata = cap.metadata;
        let dest = cap.dest;
        let chain_prev = cap.chain_prev;
        let paused_at = cap.paused_at;
        // Issue #202: the finisher now owns the capture. The guest is
        // running (inner.snapshot resumed it) and `flush_upload` below
        // owns the drained chunks' re-queue on its own error path, so
        // take the pending out of the guard and defuse it — from here
        // the guard's resume/requeue must NOT fire.
        let mut unwind = cap.unwind;
        #[cfg(target_os = "linux")]
        let nbd_pending_flush = unwind.disk_pending.take();
        unwind.defuse();
        drop(unwind);
        let mut metadata = metadata;
        // Issue #529: stamp the exact pause instant unconditionally — the
        // coord's composed eviction path resolves the session_events
        // coherence cursor from this instead of its own wall-clock `now`
        // sampled after the (possibly multi-second) post phase below,
        // closing the skew that made a clean evict→resume rewind the
        // coordinator's own lifecycle events (median 4, prod evidence).
        metadata.paused_at = Some(paused_at);
        let post = async {
            // ADR 0038 B3: the guest has resumed (inner.snapshot above
            // brought it back). Upload the drained disk chunks to GCS +
            // publish the manifest now — OFF the frozen-guest path. The
            // operation scope makes the chunk uploads attach `chunk.flush`
            // spans to the snapshot op's trace. Awaited here (before the
            // snapshot is recorded) so the recorded `disk_manifest`
            // references durable chunks; `base` is rebased only after the
            // upload, so the background scheduler never sees a
            // not-yet-uploaded chunk.
            #[cfg(target_os = "linux")]
            if let Some(pending) = nbd_pending_flush {
                // INVARIANT (see `nbd_sandboxes`): clone the Arc and drop
                // the guard BEFORE `flush_upload` — this is the multi-
                // second GCS upload (~32 s for ~1,936 chunks) and the
                // worst guard-across-await offender. Holding the shard
                // guard here parks every contending `destroy`/`create`
                // worker for the upload's full duration and can deadlock
                // the runtime. A `None`-after-clone (entry destroyed
                // mid-upload) is treated as "entry missing", exactly as
                // the previous `if let Some(entry)` did.
                let backend = self.nbd_sandboxes.get(&id).map(|e| e.backend.clone());
                if let Some(backend) = backend {
                    backend.operation_scope().begin("snapshot");
                    let res = backend.flush_upload(pending).await;
                    backend.operation_scope().end();
                    let outcome =
                        res.map_err(|e| SandboxError::Snapshot(format!("nbd disk upload: {e}")))?;
                    tracing::info!(
                        sandbox_id = %id,
                        manifest = %outcome.manifest_ref,
                        chunks_flushed = outcome.chunks_flushed,
                        bytes_uploaded = outcome.bytes_uploaded,
                        "chunked NBD disk uploaded (post-resume)",
                    );
                    metadata.disk_manifest = Some(outcome.manifest_ref);
                }
            }
            // ADR 0007 / Phase 5: when a chunk store is wired AND the
            // underlying backend left a memory.bin in `dest` (FC does;
            // VZ + Process don't), chunk it into the store + patch the
            // FC snapshot manifest so the UFFD handler can resolve
            // session-divergent pages on restore. Skipped silently when
            // either condition isn't met — VZ + Process paths still
            // produce valid snapshots without a memory_manifest, and
            // FC without a chunk_store falls back to RestoreMode::File.
            let Some(chunk_store) = self.chunk_store.as_ref() else {
                return Ok(metadata);
            };
            let manifest_ref = if let Some((prev_ref, prev_manifest)) = chain_prev.as_ref() {
                // ADR 0028 Fix A diff path: re-chunk ONLY the chunks the
                // dirty extents touched — the previous manifest's hashes
                // carry over for everything else, so CPU + upload stay
                // O(dirty set). The manifest id is stable for the chain's
                // lifetime; only `version` ticks.
                //
                // ADR 0039: sparse-only. Reconstruct each dirty chunk from
                // its prev content (warm chunk cache) + the sparse diff —
                // no full memfile to read, keep, or overlay.
                let diff_path = dest.join("memory.diff");
                let ranges = crate::checkpoint::dirty_ranges(&diff_path)
                    .map_err(|e| SandboxError::Snapshot(format!("dirty ranges: {e}")))?;
                // ADR 0101 B: the diff's dirty extent sum is the epoch's
                // rate signal for the adaptive checkpoint controller.
                epoch_dirty_bytes = Some(ranges.iter().map(|(_, len)| *len).sum());
                let next = chunk_store
                    .update_for_dirty_ranges_sparse(prev_manifest, &diff_path, &ranges)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("sparse re-chunk: {e}")))?;
                let next_ref = prev_ref.next_version();
                chunk_store
                    .put_manifest(next_ref, &next)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("put manifest {next_ref}: {e}")))?;
                // The sparse diff did its job; drop it so the local
                // snapshot dir stays state.bin + sidecar sized.
                let _ = fs::remove_file(&diff_path).await;
                next_manifest_for_chain = Some(next);
                tracing::info!(
                    session_sandbox = %id,
                    manifest = %next_ref,
                    dirty_ranges = ranges.len(),
                    "diff checkpoint re-chunked",
                );
                next_ref
            } else {
                let mem_path = dest.join("memory.bin");
                if fs::metadata(&mem_path).await.is_err() {
                    return Ok(metadata);
                }
                let mref = chunk_memory_to_store(
                    chunk_store,
                    &mem_path,
                    self.chunk_cache.as_ref(),
                    "snapshot_finish",
                )
                .await?;
                // ADR 0039: the dump is now durable in the chunk store and
                // the chain seeds from the manifest (not this file) — drop
                // the GiB-scale memory.bin so committed snapshot dirs stay
                // state.bin + sidecar sized. A cross-host restore
                // re-materializes it from the chunks via the manifest
                // (restore_materializes_missing_memory_bin_from_chunks).
                let _ = fs::remove_file(&mem_path).await;
                tracing::info!(
                    session_sandbox = %id,
                    manifest = %mref,
                    "chunked FC memory.bin → chunk store (local dump removed)",
                );
                mref
            };
            // Patch the FC sidecar JSON (`manifest.json`) so its
            // `memory_manifest` field carries the ref the UFFD handler
            // needs at restore time. The FC backend deserializes via
            // serde with `#[serde(default)]`, so a JSON patch over the
            // wire-shape stays compatible without us depending on its
            // private struct.
            let manifest_json = dest.join("manifest.json");
            patch_fc_manifest_memory_ref(
                &manifest_json,
                manifest_ref,
                self.session_bindings.get(&id).map(|e| *e),
            )
            .await?;
            metadata.memory_manifest = Some(manifest_ref);

            // ADR 0014: upload state.bin + sidecar to BlobStorage so a
            // sibling host can restore from this snapshot. memory.bin
            // is already chunk-stored above; state.bin and sidecar are
            // small opaque blobs (state.bin is FC VMM+device state,
            // sidecar is `manifest.json` carrying spec + memory_manifest
            // + source sandbox_id). Skipped silently when state.bin is
            // missing (defensive: should always exist after FC snapshot,
            // but the chunked-memory path already gates on memory.bin
            // existence for the same reason).
            let blob = chunk_store.blob_storage();
            let state_path = dest.join("state.bin");
            let sidecar_path = dest.join("manifest.json");
            if fs::metadata(&state_path).await.is_ok() && fs::metadata(&sidecar_path).await.is_ok()
            {
                let state_key = engram_chunk_store::snapshot_blob::state_blob_key(metadata.id);
                let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(metadata.id);
                engram_chunk_store::snapshot_blob::upload_file(
                    blob.as_ref(),
                    &state_key,
                    &state_path,
                )
                .await
                .map_err(|e| SandboxError::Snapshot(format!("upload state.bin: {e}")))?;
                engram_chunk_store::snapshot_blob::upload_file(
                    blob.as_ref(),
                    &sidecar_key,
                    &sidecar_path,
                )
                .await
                .map_err(|e| SandboxError::Snapshot(format!("upload sidecar.json: {e}")))?;
                metadata.state_blob_key = Some(state_key);
                metadata.sidecar_blob_key = Some(sidecar_key);
                metadata.source_sandbox_id = Some(id);
                tracing::info!(
                    source_sandbox = %id,
                    snapshot_id = %metadata.id,
                    "portable snapshot artifacts uploaded to BlobStorage",
                );
            }
            // ADR 0035 §2: idempotently publish the pinned bundle
            // generations. Runs on every snapshot flavor — an eviction
            // snapshot can pin a swapped-in generation no base capture
            // ever published. Failure fails the snapshot (a pin nothing
            // can satisfy is worse than a retried eviction).
            if !metadata.aux_bundles.is_empty() {
                crate::bundles::BundleStore::new(
                    blob.clone(),
                    self.bundle_dir.clone(),
                    self.bundle_file_ext,
                )
                .publish(&metadata.aux_bundles)
                .await?;
            }
            Ok::<_, SandboxError>(metadata)
        }
        .await;

        let result = match post {
            Ok(m) => {
                // ADR 0014 issue #1/#2: record the snapshot_id so a
                // future commit/abort RPC can clean up the per-snapshot
                // artifacts even though the caller only knows the
                // sandbox_id. Also lets a retry of this same sandbox's
                // snapshot() find and tear down the prior attempt.
                self.inflight_snapshots.insert(id, m.id);
                // ADR 0016 Phase A: stamp the memory-tier RPO signal
                // the `cow_state` RPC reports. Done AFTER post-
                // processing succeeded — a snapshot whose state.bin /
                // sidecar upload failed isn't durable and shouldn't
                // advance the RPO indicator on the diagnostic
                // surface. Pre-`commit_snapshot` is fine: the row
                // hasn't been "claimed" by PG yet but the bytes are
                // in BlobStorage, which is what the RPO measures.
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let prev_ms = self.last_snapshot_unix_ms.insert(id, now_ms);
                // ADR 0101 B: pacing sample for the adaptive checkpoint
                // controller — the epoch is the gap since the previous
                // successful capture (any flavor; that's what the dirty
                // set accumulated over). First capture has no epoch, so
                // no sample: the controller keeps the backstop cadence.
                if let Some(prev_ms) = prev_ms.filter(|p| *p > 0 && *p <= now_ms) {
                    let epoch = std::time::Duration::from_millis((now_ms - prev_ms) as u64);
                    self.checkpoint_pacing.insert(
                        id,
                        EpochPacingSample {
                            dirty_bytes: epoch_dirty_bytes,
                            epoch,
                        },
                    );
                    if let Some(dirty) = epoch_dirty_bytes {
                        metrics::histogram!(crate::metrics::CHECKPOINT_EPOCH_BYTES)
                            .record(dirty as f64);
                        metrics::histogram!(crate::metrics::CHECKPOINT_EPOCH_SECONDS)
                            .record(epoch.as_secs_f64());
                    }
                }
                // ADR 0028 Fix A: seed/advance the rolling chain +
                // persist the durable host-owned record. Best-effort
                // beyond the capture: a seed failure means the next
                // capture is Full again; a record failure means only
                // PG (if the caller's pipeline survives) knows this
                // checkpoint. Both safe, both logged inside.
                self.advance_checkpoint_state(id, &m, paused_at, next_manifest_for_chain)
                    .await;
                Ok(m)
            }
            Err(e) => {
                // The FC Diff already ran (capture_phase succeeded), so
                // the dirty bitmap is consumed and the memory.diff we're
                // about to delete is its only record — a later Diff off
                // the unadvanced chain would silently miss these pages.
                // Poison the chain so the next capture is a Full.
                if chain_prev.is_some() {
                    poison_checkpoint_chain_after_failed_diff(
                        &self.checkpoint_chains,
                        self.chain_heads.as_deref(),
                        id,
                        "snapshot post-processing",
                    );
                }
                // ADR 0014 cleanup hygiene: rm -rf the FC-written
                // snapshot dir before propagating. Idle-evict retry
                // (every ~30s) without this leaks 4 GiB per try and
                // fills the host disk inside an hour.
                match tokio::fs::remove_dir_all(&dest).await {
                    Ok(_) => tracing::warn!(
                        sandbox_id = %id,
                        dest = %dest.display(),
                        error = %e,
                        "PooledBackend::snapshot post-inner failed; orphan dir cleaned",
                    ),
                    Err(rm_err) => tracing::warn!(
                        sandbox_id = %id,
                        dest = %dest.display(),
                        snapshot_error = %e,
                        rm_error = %rm_err,
                        "PooledBackend::snapshot post-inner failed; rm -rf of orphan dir also failed",
                    ),
                }
                Err(e)
            }
        };
        metrics::histogram!(
            crate::metrics::SNAPSHOT_FINISH_SECONDS,
            "type" => flavor,
            "outcome" => if result.is_ok() { "success" } else { "error" },
        )
        .record(finish_start.elapsed().as_secs_f64());
        result
    }
    fn checkpoint_records_dir(&self) -> Option<PathBuf> {
        self.checkpoint_dir.as_ref().map(|d| d.join("records"))
    }

    /// Durably mirror the in-RAM chain head at `manifest_ref` (which
    /// MUST already be published in the chunk store — every caller sits
    /// after a successful `put_manifest` or a fetch of an existing
    /// manifest). Best-effort: a failed write just means a post-roll
    /// rehydrate finds no record and the survivor's next capture is a
    /// Full — safe, slower, and self-healing at the next checkpoint.
    /// Routed through the [`crate::checkpoint::ChainHeadStore`] so a
    /// cancelled write's detached tail can never outlive a later
    /// invalidate.
    async fn persist_chain_head(
        &self,
        id: SandboxId,
        manifest_ref: engram_core::types::manifest::ManifestRef,
    ) {
        let Some(store) = &self.chain_heads else {
            return;
        };
        let record = crate::checkpoint::ChainHeadRecord {
            sandbox_id: id,
            manifest_ref,
            session_id: self.session_bindings.get(&id).map(|s| *s),
            updated_at: self.clock.now_utc(),
        };
        if let Err(e) = store.persist(record).await {
            tracing::warn!(
                sandbox_id = %id,
                manifest = %manifest_ref,
                error = %e,
                "durable chain-head write failed; a pod roll before the next \
                 checkpoint costs this sandbox one Full capture",
            );
        }
    }

    async fn advance_checkpoint_state(
        &self,
        id: SandboxId,
        metadata: &SnapshotMetadata,
        paused_at: chrono::DateTime<chrono::Utc>,
        next_manifest: Option<engram_chunk_store::Manifest>,
    ) {
        if self.checkpoint_dir.is_none() {
            return;
        }
        let Some(memory_ref) = metadata.memory_manifest else {
            // No chunked memory (no chunk store / non-FC backend):
            // nothing to chain, and a record without a memory
            // manifest adds nothing over the live disk manifest.
            return;
        };

        match next_manifest {
            // Diff capture: the sparse re-chunk already ran in the post
            // block; just advance the chain's manifest pointer, then
            // re-commit the durable chain-head record the capture's
            // write-ahead invalidate removed.
            Some(next) => {
                if let Some(mut chain) = self.checkpoint_chains.get_mut(&id) {
                    chain.manifest_ref = memory_ref;
                    chain.manifest = next;
                }
                self.persist_chain_head(id, memory_ref).await;
            }
            // Full capture: seed the chain manifest-only from the manifest
            // we just published (ADR 0039 — no local rolling image; the
            // memory.bin was chunked + removed in the post block).
            // Subsequent captures ride the sparse diff path. (The seed
            // persists the chain-head record itself.)
            None => {
                self.seed_checkpoint_chain_sparse(id, memory_ref).await;
            }
        }

        // Durable record — only for session-bound sandboxes (anonymous
        // base-snapshot captures have no session to reconcile).
        let Some(session_id) = self.session_bindings.get(&id).map(|s| *s) else {
            return;
        };
        let Some(records_dir) = self.checkpoint_records_dir() else {
            return;
        };
        let record = crate::checkpoint::CheckpointRecord {
            snapshot_id: metadata.id,
            session_id,
            sandbox_id: id,
            image_version: metadata.image_version.clone(),
            size_bytes: metadata.size_bytes,
            disk_manifest: metadata.disk_manifest,
            memory_manifest: metadata.memory_manifest,
            aux_bundles: metadata.aux_bundles.clone(),
            paused_at,
            captured_at: metadata.created_at,
            // This is `advance_checkpoint_state` — the composed
            // `snapshot()`/periodic-checkpoint path. The eviction flavor's
            // OWN terminal record write (`run_eviction_finalize`) stamps
            // `EvictionFinal` explicitly; this call site never runs for it
            // (it uses `snapshot_begin`, not `snapshot()`).
            kind: engram_protocol::heartbeat::CheckpointKind::Periodic,
        };
        if let Err(e) = record
            .persist(&engram_host_core::TokioFs, &records_dir)
            .await
        {
            tracing::warn!(
                sandbox_id = %id,
                snapshot_id = %metadata.id,
                error = %e,
                "durable checkpoint record write failed; PG row (if the caller's \
                 pipeline survives) is the only reference",
            );
        }
    }

    /// ADR 0045 seed-at-create: seed the chain by FORKING the source
    /// lineage — publish the source manifest's content under a fresh
    /// manifest id @v1, and chain on that. Required because fresh
    /// creates seed from the SHARED per-image base manifest: chaining
    /// directly on it makes every session's first diff race to publish
    /// `base_id@v2` (the e2e-caught version conflict). The fork gives
    /// each session a lineage it solely owns; the diff path then ticks
    /// versions unchanged. One small manifest-JSON PUT (no chunk
    /// uploads — the content is byte-identical to the source).
    /// Best-effort like the sparse seed: failure → no chain → the next
    /// capture is Full.
    async fn seed_checkpoint_chain_forked(
        &self,
        id: SandboxId,
        src_ref: engram_core::types::manifest::ManifestRef,
    ) {
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return;
        };
        if self.checkpoint_dir.is_none() || self.checkpoint_chains.contains_key(&id) {
            return;
        }
        let manifest = match chunk_store.get_manifest(src_ref).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    manifest = %src_ref,
                    error = %e,
                    "seed-at-create: source manifest fetch failed; first checkpoint falls back to Full",
                );
                return;
            }
        };
        let fork_ref = engram_core::types::manifest::ManifestRef::new();
        if let Err(e) = chunk_store.put_manifest(fork_ref, &manifest).await {
            tracing::warn!(
                sandbox_id = %id,
                src = %src_ref,
                fork = %fork_ref,
                error = %e,
                "seed-at-create: fork manifest publish failed; first checkpoint falls back to Full",
            );
            return;
        }
        self.checkpoint_chains.insert(
            id,
            crate::checkpoint::CheckpointChain {
                manifest_ref: fork_ref,
                manifest,
            },
        );
        // The fork manifest is durable (put_manifest above succeeded) —
        // commit the chain-head record so the chain survives a pod roll.
        self.persist_chain_head(id, fork_ref).await;
        tracing::info!(
            sandbox_id = %id,
            src = %src_ref,
            fork = %fork_ref,
            "ADR 0045 seed-at-create: chain seeded on a forked lineage; first checkpoint will diff",
        );
    }

    /// ADR 0038 B2 / ADR 0039: seed the checkpoint chain manifest-only
    /// (no local rolling image — "sparse mode"). Two entry points: on
    /// RESUME from the source's memory manifest, and after a fresh Full
    /// capture from the just-published one. ADR 0039 retired the rolling
    /// memfile, so this is the *only* seed. The manifest already describes
    /// the full image in the chunk store, so the next periodic checkpoint
    /// takes the diff path (`update_for_dirty_ranges_sparse`) instead of a
    /// Full re-read of guest RAM — which under UFFD faults the entire
    /// working set in from the store (the 60 s `PUT /snapshot/create`
    /// hang).
    ///
    /// Best-effort: a missing/unfetchable manifest just means the next
    /// checkpoint falls back to Full. Skips when checkpointing is
    /// disabled, there's no chunk store, or a chain is already tracked
    /// (a fresh `restore` mints a new sandbox id; the Full-capture caller
    /// only reaches here when the chain was empty).
    async fn seed_checkpoint_chain_sparse(
        &self,
        id: SandboxId,
        memory_ref: engram_core::types::manifest::ManifestRef,
    ) {
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return;
        };
        if self.checkpoint_dir.is_none() || self.checkpoint_chains.contains_key(&id) {
            return;
        }
        match chunk_store.get_manifest(memory_ref).await {
            Ok(manifest) => {
                self.checkpoint_chains.insert(
                    id,
                    crate::checkpoint::CheckpointChain {
                        manifest_ref: memory_ref,
                        manifest,
                    },
                );
                // The seed source is a published manifest (we just
                // fetched it) — commit the chain-head record so the
                // chain survives a pod roll.
                self.persist_chain_head(id, memory_ref).await;
                tracing::info!(
                    sandbox_id = %id,
                    manifest = %memory_ref,
                    "ADR 0038/0039: seeded sparse checkpoint chain; next checkpoint will diff",
                );
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    manifest = %memory_ref,
                    error = %e,
                    "resume chain seed failed; first checkpoint falls back to Full",
                );
            }
        }
    }
}

/// zero chunks + zero bytes of object storage.
pub(crate) async fn chunk_memory_to_store(
    chunk_store: &ChunkStore,
    memory_bin: &std::path::Path,
    cache: Option<&ChunkCache>,
    // `source` label for the re-chunk histogram: which capture flavor
    // paid for this full-image scan (`snapshot_finish` | `evict_finalize`).
    source: &'static str,
) -> Result<engram_core::types::manifest::ManifestRef, SandboxError> {
    // ADR 0039 (sticky-everywhere): write-through the base memory chunks
    // into the host's local cache as they're uploaded, so the capturing
    // host keeps them local instead of re-fetching its own writes.
    let res = chunk_store
        .chunk_file_into(
            memory_bin,
            engram_chunk_store::ManifestKind::Memory,
            None,
            cache,
            None,
        )
        .await;
    let outcome = if res.is_ok() { "success" } else { "error" };
    if let Ok((_, stats)) = &res {
        for (phase, seconds) in [
            ("scan", stats.scan_seconds),
            ("upload", stats.flush_seconds),
        ] {
            metrics::histogram!(
                crate::metrics::RECHUNK_SECONDS,
                "phase" => phase,
                "source" => source,
                "outcome" => outcome,
            )
            .record(seconds);
        }
        metrics::counter!(crate::metrics::RECHUNK_BYTES_SCANNED_TOTAL, "source" => source)
            .increment(stats.bytes_scanned);
        metrics::counter!(crate::metrics::RECHUNK_BYTES_UPLOADED_TOTAL, "source" => source)
            .increment(stats.bytes_uploaded);
        tracing::info!(
            memory_bin = %memory_bin.display(),
            source,
            scan_ms = (stats.scan_seconds * 1000.0) as u64,
            upload_ms = (stats.flush_seconds * 1000.0) as u64,
            bytes_scanned = stats.bytes_scanned,
            chunks_uploaded = stats.chunks_uploaded,
            bytes_uploaded = stats.bytes_uploaded,
            "full memory re-chunk phase breakdown",
        );
    }
    let (manifest, _) = res.map_err(|e| {
        SandboxError::Snapshot(format!("chunk memory.bin {}: {e}", memory_bin.display(),))
    })?;
    let manifest_ref = engram_core::types::manifest::ManifestRef::new();
    chunk_store
        .put_manifest(manifest_ref, &manifest)
        .await
        .map_err(|e| {
            SandboxError::Snapshot(format!(
                "put_manifest {manifest_ref} for {}: {e}",
                memory_bin.display(),
            ))
        })?;
    Ok(manifest_ref)
}

/// Set / overwrite the `memory_manifest` field on the FC sidecar
/// JSON at `manifest_json`. Operates on the JSON value rather than
/// the FC backend's private `FcSnapshotManifest` type — the on-disk
/// shape is the wire contract between the two crates, and FC
/// deserializes via `#[serde(default)]` so JSON-level patches stay
/// compatible without a circular dependency.
pub(crate) async fn patch_fc_manifest_memory_ref(
    manifest_json: &std::path::Path,
    memory_manifest: engram_core::types::manifest::ManifestRef,
    // Tier 2 (resume-prefault fix): the session id, stamped as the
    // session-stable working-set trace key. `None` on backends/paths with
    // no bound session leaves the FC-side default (`None`), i.e. today's
    // per-host manifest keying.
    trace_lineage: Option<SessionId>,
) -> Result<(), SandboxError> {
    let bytes = fs::read(manifest_json).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "read fc manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        SandboxError::Snapshot(format!(
            "parse fc manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    let obj = value.as_object_mut().ok_or_else(|| {
        SandboxError::Snapshot(format!(
            "fc manifest.json {} root is not an object",
            manifest_json.display(),
        ))
    })?;
    obj.insert(
        "memory_manifest".into(),
        serde_json::to_value(memory_manifest)
            .map_err(|e| SandboxError::Snapshot(format!("serialize memory_manifest: {e}")))?,
    );
    // Tier 2 (resume-prefault fix): stamp the session-stable trace key next
    // to the memory ref. On the next resume the FC backend reads this and
    // passes `--trace-key <session_id>`, so the handler keys its prefault-
    // replay by the session (canonical) instead of the fresh per-checkpoint
    // memory manifest id — which never matches on resume. Same serde-default
    // JSON-patch approach as the memory ref above.
    if let Some(session_id) = trace_lineage {
        obj.insert(
            "trace_lineage_id".into(),
            serde_json::to_value(session_id)
                .map_err(|e| SandboxError::Snapshot(format!("serialize trace_lineage_id: {e}")))?,
        );
    }
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|e| SandboxError::Snapshot(format!("serialize patched manifest.json: {e}")))?;
    fs::write(manifest_json, bytes).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "write patched manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    Ok(())
}

// ADR 0021 P1.5: `harness_name_for_substrate` / `harness_name_from_uri`
// retired with the substrate (the in-VM harness path comes from the
// image manifest's `[harness] exec` now, not a URI suffix) — this used
// to be their doc comment; dead code, cleaned up incidentally while
// touching this section for #526 (a dangling doc comment here newly
// tripped `clippy::empty_line_after_doc_comments` against the struct
// below once one was inserted after it).

/// ADR 0019 / telemetry restoration (#526): the on-disk shape of the
/// uffd-handler's `prefault-stats.json` (`PrefaultStats` in
/// `engram-uffd-handler::runtime`), duck-typed here rather than shared
/// via a cross-crate dependency — the file format is the contract
/// between the two binaries, not a Rust type (the writer is Linux-only
/// internally; this reader must build on every host-agent target).
/// `#[serde(default)]` on the count fields means a stats file that only
/// carries `trace_loaded` (shouldn't happen from this repo's writer,
/// but keeps a future schema-superset — e.g. `prefault-admission-
/// control`'s convergent gate file — from breaking this reader) still
/// parses.
#[derive(Debug, serde::Deserialize)]
struct PrefaultStatsFile {
    trace_loaded: bool,
    #[serde(default)]
    installed: usize,
    #[serde(default)]
    skipped: usize,
}

/// Review finding 6: the peer-fill fields `PrefaultStats` carries for
/// post-copy migration destinations (issue step (d)3 — "uffd-handler
/// live peer page-serves are recorded in the same per-jail stats file").
/// Parsed independently of `PrefaultStatsFile`/`classify_prefault_outcome`
/// (which stay focused on the `engram_resume_prefault_*` counter
/// contract) — these are **span attributes only for now**, not a new
/// counter family; `epic-gcs-free-resume` owns promoting them. Every
/// field defaults to `0` — both for a genuinely non-peer resume (the
/// fields are simply absent from the JSON) and for a peer resume whose
/// drain happened to move nothing, so this is only recorded on the span
/// when at least one field is nonzero (see `restore()`).
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
struct PeerFillSnapshot {
    #[serde(default)]
    peer_pulled: u64,
    #[serde(default)]
    peer_alt_sourced: u64,
    #[serde(default)]
    peer_zero_chunks: u64,
    #[serde(default)]
    peer_live_faults: u64,
}

impl PeerFillSnapshot {
    fn is_nonzero(&self) -> bool {
        self.peer_pulled != 0
            || self.peer_alt_sourced != 0
            || self.peer_zero_chunks != 0
            || self.peer_live_faults != 0
    }
}

fn parse_peer_fill_snapshot(stats_bytes: Option<&[u8]>) -> Option<PeerFillSnapshot> {
    serde_json::from_slice(stats_bytes?).ok()
}

/// The three effectiveness outcomes `engram_resume_prefault_total` is
/// labeled with. See the module doc on [`crate::metrics::RESUME_PREFAULT_TOTAL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefaultOutcome {
    /// A trace was requested + loaded, and the prefault ran.
    /// `installed == 0` is itself worth alarming on downstream (a
    /// stale/mismatched trace) but is still a "replayed" outcome, not
    /// an error — the counter split lives in `RESUME_PREFAULT_CHUNKS_TOTAL`.
    Replayed { installed: usize, skipped: usize },
    /// No trace was requested/loaded for this resume (base image,
    /// migration dest with no prior life, or the prior life's publish
    /// never landed) — expected, not an error.
    NoTrace,
    /// A stats file was expected (this backend has a
    /// `prefault_stats_path`) but wasn't there — the alarm condition.
    /// This is the exact class of bug that went inert 3x silently
    /// (d0e5ecf3, cf6e4d32, 7c2a7226): the handler died, or the wiring
    /// silently didn't fire, and nothing said so.
    StatsMissing,
}

impl PrefaultOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Replayed { .. } => "replayed",
            Self::NoTrace => "no_trace",
            Self::StatsMissing => "stats_missing",
        }
    }
}

/// Review finding 1: how long / how often to retry the prefault-stats
/// read before conceding `stats_missing`. The write races a background
/// thread whose fetch duration is unbounded in principle (chunk count x
/// NVMe/GCS latency) but "seconds" in the evidence pass's worst observed
/// case (migration-dest: drain-then-trace-prefault). 20s at a 250ms
/// cadence is generous relative to that without holding the poll open
/// indefinitely on a genuinely dead handler; this loop is spawned
/// off `restore()`'s return path, so it never adds latency to the
/// resume itself.
const PREFAULT_STATS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const PREFAULT_STATS_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Poll for the uffd-handler's `prefault-stats.json` until it appears or
/// [`PREFAULT_STATS_POLL_TIMEOUT`] elapses. `prefault_from_trace` writes
/// the file exactly once, atomically (temp+rename), at the END of its
/// background fetch — so there is no "partial write" state to worry
/// about: the file is either not there yet (keep polling) or fully
/// there (done). Returns `None` if the timeout is reached without ever
/// seeing the file — the same `stats_missing` alarm as before, but now
/// only after a generous wait instead of on the first instant.
async fn read_prefault_stats_with_retry(stats_path: &std::path::Path) -> Option<Vec<u8>> {
    read_prefault_stats_with_retry_bounded(
        stats_path,
        PREFAULT_STATS_POLL_INTERVAL,
        PREFAULT_STATS_POLL_TIMEOUT,
    )
    .await
}

/// Parameterized half of [`read_prefault_stats_with_retry`], split out so
/// tests can exercise the "appears mid-poll" and "never appears" cases
/// with millisecond bounds instead of the real 20s production timeout.
async fn read_prefault_stats_with_retry_bounded(
    stats_path: &std::path::Path,
    interval: std::time::Duration,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    let deadline = crate::time_source::metrics_now_tokio() + timeout;
    loop {
        if let Ok(bytes) = fs::read(stats_path).await {
            return Some(bytes);
        }
        if crate::time_source::metrics_now_tokio() >= deadline {
            return None;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Pure stats-file-bytes → outcome mapping (the testable half of the
/// read). `None` input means the file didn't exist / couldn't be read
/// (both collapse to `StatsMissing` — a read error other than "not
/// found" is just as much an alarm as absence). A corrupt/unparseable
/// file (the atomic temp+rename write should prevent partial reads, but
/// defend anyway) is treated the same way.
fn classify_prefault_outcome(stats_bytes: Option<&[u8]>) -> PrefaultOutcome {
    let Some(bytes) = stats_bytes else {
        return PrefaultOutcome::StatsMissing;
    };
    match serde_json::from_slice::<PrefaultStatsFile>(bytes) {
        Ok(s) if s.trace_loaded => PrefaultOutcome::Replayed {
            installed: s.installed,
            skipped: s.skipped,
        },
        Ok(_) => PrefaultOutcome::NoTrace,
        Err(_) => PrefaultOutcome::StatsMissing,
    }
}

/// Emit `engram_resume_prefault_total` + `engram_resume_prefault_chunks_total`
/// for one resume's prefault outcome.
fn emit_prefault_metrics(outcome: PrefaultOutcome) {
    metrics::counter!(
        crate::metrics::RESUME_PREFAULT_TOTAL,
        "outcome" => outcome.label(),
    )
    .increment(1);
    if let PrefaultOutcome::Replayed { installed, skipped } = outcome {
        metrics::counter!(
            crate::metrics::RESUME_PREFAULT_CHUNKS_TOTAL,
            "result" => "installed",
        )
        .increment(installed as u64);
        metrics::counter!(
            crate::metrics::RESUME_PREFAULT_CHUNKS_TOTAL,
            "result" => "skipped",
        )
        .increment(skipped as u64);
    }
}

/// ADR 0019 / telemetry restoration (#526), review findings 3 + 7: the
/// `source="peer"` half of the peer-vs-GCS cache-fill split
/// ([`engram_chunk_store::cache::CHUNK_FILL_TOTAL`] — the `source="gcs"`
/// half lives in `engram-chunk-store::cache`'s leader-persist arm).
/// Shared by both loops that land migration-sourced chunks into the
/// local cache via `ChunkCache::put_no_evict`: `migration_prestage`'s
/// per-item loop and `pull_chunks_from_source`'s divergence pull —
/// finding 3 was that the latter wrote chunks uncounted, systematically
/// undercounting peer volume.
fn count_peer_chunk_fill(bytes: usize) {
    metrics::counter!(
        engram_chunk_store::cache::CHUNK_FILL_TOTAL,
        "source" => "peer",
    )
    .increment(1);
    metrics::counter!(
        engram_chunk_store::cache::CHUNK_FILL_BYTES_TOTAL,
        "source" => "peer",
    )
    .increment(bytes as u64);
}

#[async_trait]
impl SandboxBackend for PooledBackend {
    // Capability methods proxy to the wrapped backend — the pool is
    // a thin caching layer; whatever Process / VZ / FC reports
    // about itself is what callers see.
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        self.inner.harness_dial()
    }

    fn restore_memory_is_lazy_for(&self, fresh: bool) -> bool {
        self.inner.restore_memory_is_lazy_for(fresh)
    }

    fn bundle_dir(&self) -> &std::path::Path {
        // Delegates to the inner backend — the one source of truth this wrapper
        // also materializes into (`self.bundle_dir`, copied from here in `new`).
        self.inner.bundle_dir()
    }

    fn bundle_file_ext(&self) -> &'static str {
        // Delegates to the inner backend so the BundleStore materializes/sweeps
        // the SAME filename the backend attaches (squashfs, ADR 0096).
        self.inner.bundle_file_ext()
    }

    async fn guest_memory_stats(&self) -> Option<engram_core::traits::sandbox::GuestMemoryStats> {
        self.inner.guest_memory_stats().await
    }

    async fn create(&self, mut spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        // Issue #224: once SIGTERM has engaged terminal-abandon mode,
        // refuse new cold-creates outright. Unlike resume/rehydrate
        // (which re-serve a SURVIVING FC's already-attached device and
        // must hand it off to the successor), a cold create has no
        // surviving-VM contract — there is nothing to preserve, and
        // letting it run would spawn a brand-new NBD daemon racing the
        // abandon sweep. Reject early so the coordinator reschedules
        // onto a live host rather than orphaning a half-built sandbox.
        #[cfg(target_os = "linux")]
        if self.is_abandoning() {
            return Err(SandboxError::Vm(
                "host-agent is shutting down (terminal abandon mode); \
                 create rejected — reschedule onto another host"
                    .into(),
            ));
        }
        // Phase 5+: resolve OCI image_uri / harness_pack_uri to local
        // cached paths before warm-pool key derivation. Both paths
        // overwrite the legacy filesystem fields when set, so the
        // inner backend keeps working with rootfs_source / harness_
        // substrate unchanged. On cache miss we pull synchronously;
        // subsequent sessions for the same digest hit the cache and
        // pay only the FS cost.
        // `pending_nbd_state` carries the NBD daemon's state from
        // the `resolve_rootfs` call (which spawns it) into the
        // post-`inner.create` stash step. Linux-only — on macOS
        // resolve_rootfs always returns `()` for the second slot.
        #[cfg(target_os = "linux")]
        let mut pending_nbd_state: Option<crate::disk_daemon::NbdSandboxState> = None;
        // ADR 0014 follow-up: per-phase boot timing. Captured as
        // sum-of-durations because the image + harness branches are
        // independently optional (image-only specs skip the harness
        // pull and vice-versa). The histogram labels follow the
        // contract in `crate::metrics::SANDBOX_BOOT_SECONDS`.
        let phase_total = crate::time_source::metrics_now();
        let mut image_resolve = std::time::Duration::ZERO;
        let mut materialize = std::time::Duration::ZERO;
        let result: Result<SandboxId, SandboxError> = async {
            // ADR 0028 Fix B: explicit rootfs-manifest override wins
            // over image resolution — the disk-only cold-boot recovery
            // boots a fresh kernel against a session's evolved rootfs
            // lineage, so the image's own disk (and its pull) is
            // irrelevant; `spec.image_uri` stays as record-keeping.
            if let Some(manifest_ref) = spec.rootfs_manifest {
                let t = crate::time_source::metrics_now();
                let (path, _state) = self.resolve_rootfs_from_manifest(manifest_ref).await?;
                materialize += t.elapsed();
                spec.rootfs_source = Some(path);
                #[cfg(target_os = "linux")]
                {
                    pending_nbd_state = _state;
                }
            } else if let Some(cache) = &self.image_cache {
                if let Some(uri) = spec.image_uri.clone() {
                    let t = crate::time_source::metrics_now();
                    let cached = cache.ensure_image(&uri).await.map_err(|e| {
                        SandboxError::InvalidSpec(format!("image cache pull {uri}: {e}"))
                    })?;
                    image_resolve += t.elapsed();
                    tracing::debug!(uri = %uri, digest = %cached.digest, "image cache hit/pulled");
                    let t = crate::time_source::metrics_now();
                    let (path, _state) = self.resolve_rootfs(&uri, &cached).await?;
                    materialize += t.elapsed();
                    spec.rootfs_source = Some(path);
                    #[cfg(target_os = "linux")]
                    {
                        pending_nbd_state = _state;
                    }
                }
                // ADR 0021 P1.5: no harness substrate to build —
                // the harness binary travels in the rootfs at the
                // manifest-declared `[harness] exec` path.
            }

            // `fc_boot` is emitted by the inner FC backend itself
            // (see `engram_sandbox_firecracker::create`), with the
            // same metric name + label set. We don't double-record
            // here because that backend has finer-grained insight
            // into the sub-steps of the FC `PUT` calls if we want
            // to drill in later.
            // Stash any spawned NBD daemon under the freshly-assigned
            // sandbox id. `destroy()` removes the entry (Drop tears
            // down the daemon + returns the slot); `snapshot()` reads
            // it back to call `backend.flush()`.
            //
            // ADR 0016 Phase B: post-`inner.create()` is the first
            // point where the sandbox_id is known — install the
            // FlushScheduler now. The scheduler is keyed by sandbox_id
            // and resolves session_id via the publisher's own
            // sandbox→session lookup (so warm-pool sandboxes share the
            // same path without re-spawn machinery). Field-ordered
            // Drop on `NbdSandboxState` guarantees scheduler-cancel →
            // NBD-disconnect → slot-release.
            //
            // Issue #223 — CANCELLATION SAFETY (same hazard as the restore
            // path): the rootfs sidecar was patched to `/dev/nbdN` before
            // `inner.create`, so the booted FC reads that device, but the
            // `NbdSandboxState` only enters `nbd_sandboxes` AFTER create
            // returns. A handler future dropped between `inner.create` and the
            // insert drops `pending_nbd_state` under the live FC — disconnect
            // under the running guest + slot freed for cross-session reuse.
            // Run create + the insert inside a `tokio::spawn`ed task that MOVES
            // the NBD state in, so the daemon always lands in the map and
            // normal `destroy()` teardown owns it regardless of the request's
            // fate. The non-NBD path stays inline (no drop chain to lose).
            #[cfg(target_os = "linux")]
            let create_result: Result<SandboxId, SandboxError> = if let Some(mut state) =
                pending_nbd_state
            {
                let inner = self.inner.clone();
                let nbd_sandboxes = self.nbd_sandboxes.clone();
                let publisher = self.live_manifest_publisher.clone();
                let flush_config = self.flush_config.clone();
                let abandoning = self.abandoning.clone();
                let join = tokio::spawn(async move {
                    let sandbox_id = inner.create(spec).await?;
                    state.install_flush_scheduler(sandbox_id, publisher, flush_config);
                    // ADR 0019: open the cold-boot operation window. The guest's
                    // rootfs/substrate ext4-mount page-ins (served by this NBD
                    // backend) now attach `chunk.fetch` spans to the cold-boot
                    // trace until `start_agent` ends the window at agent_ready.
                    state.backend.operation_scope().begin("cold_boot");
                    // Issue #224: terminal-mode check (defense in depth past
                    // the early reject at the top of `create`). The flag may
                    // have flipped during the `inner.create` await; this task
                    // outlives a dropped handler future (issue #223), so it
                    // outlives the SIGTERM too. Abandon the data plane in-place
                    // rather than inserting after the sweep — the half-built
                    // sandbox has no successor contract, but its device must
                    // not be netlink-disconnected by a post-sweep normal drop.
                    if abandoning.load(std::sync::atomic::Ordering::SeqCst) {
                        tracing::warn!(
                            %sandbox_id,
                            "create completed during SIGTERM abandon; abandoning the \
                             NBD data plane in-place instead of inserting after the sweep",
                        );
                        state.abandon_for_shutdown();
                        return Ok::<SandboxId, SandboxError>(sandbox_id);
                    }
                    nbd_sandboxes.insert(sandbox_id, state);
                    Ok::<SandboxId, SandboxError>(sandbox_id)
                });
                join.await
                    .map_err(|e| SandboxError::Vm(format!("create task panicked: {e}").into()))?
            } else {
                self.inner.create(spec).await
            };
            #[cfg(not(target_os = "linux"))]
            let create_result: Result<SandboxId, SandboxError> = self.inner.create(spec).await;
            create_result
        }
        .await;

        let outcome = match &result {
            Ok(_) => "success",
            Err(SandboxError::InvalidSpec(_)) => "invalid_spec",
            Err(_) => "fc_error",
        };
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "image_resolve",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(image_resolve.as_secs_f64());
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "materialize",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(materialize.as_secs_f64());
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "create_total",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(phase_total.elapsed().as_secs_f64());
        result
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.inner.exec_stream(id, cmd).await
    }

    async fn write_files(
        &self,
        id: SandboxId,
        files: Vec<WriteFileSpec>,
    ) -> Result<Vec<WriteFileResult>, SandboxError> {
        self.inner.write_files(id, files).await
    }

    // ADR 0066: the port relay reaches agentd through the wrapped backend's
    // vsock (FC) — load-bearing in prod, where `self.inner` is FC.
    async fn open_guest_stream(
        &self,
        id: SandboxId,
        port: u32,
    ) -> Result<Option<engram_core::traits::sandbox::HarnessByteStream>, SandboxError> {
        self.inner.open_guest_stream(id, port).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // ADR 0045 D5: composed form — capture, then run the post phase
        // inline holding the capture lock (the periodic-checkpoint and
        // drain flavor; eviction uses snapshot_begin/snapshot_wait).
        let (_capture_guard, cap) = self.capture_phase(id).await?;
        // Lift the per-jail working-set trace into the blob store under the
        // session-canonical key so the next resume prefaults it (#517 keyed
        // the replay; the handler can't publish it itself — SIGKILLed on
        // destroy). Detached + best-effort; never blocks the capture.
        self.spawn_trace_publish(id);
        self.finisher().finish(id, cap).await
    }

    /// ADR 0045 D5 (rewritten for issue #529): the eviction flavor. Runs
    /// the capture, re-pauses the guest (it's being torn down), then
    /// makes the finalize inputs DURABLE — the drained NBD disk chunks
    /// (if any) to `<dest>/disk-pending/`, then an
    /// [`crate::eviction_finalize::EvictionFinalizeRecord`] to
    /// `<checkpoint_dir>/finalize/<snapshot_id>.json` — and only THEN
    /// returns. From here the finalize is a host-owned job
    /// (`eviction_finalize::run_eviction_finalize`, spawned below) that
    /// never touches the sandbox, the coordinator, or this call's
    /// stack again: it is a pure function of what was just persisted,
    /// re-drivable by a fresh host-agent process
    /// (`resume_pending_finalizes`) if this one dies mid-upload. The
    /// coordinator may mark the session Idle as soon as this returns —
    /// unlike the pre-#529 shape, that's now safe: the row will land via
    /// the heartbeat reconcile regardless of what happens to the
    /// coordinator or this process next.
    ///
    /// Gate: VZ/Process (`!supports_diff_checkpoints()`) and hosts with
    /// checkpointing disabled (`checkpoint_dir` unset) surface
    /// `InvalidSpec` — the coordinator's caller falls through to the
    /// composed `snapshot()` pipeline (`idle_evictor.rs`).
    async fn snapshot_begin(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        if !self.inner.supports_diff_checkpoints() || self.checkpoint_dir.is_none() {
            return Err(SandboxError::InvalidSpec(
                "host-durable eviction finalize requires diff-checkpoint support and a wired \
                 checkpoint_dir"
                    .into(),
            ));
        }
        // Idempotency: a scanner-retried eviction (coord-side timeout /
        // 409 / restart before the coordinator's own row-watcher noticed
        // completion) re-observes the still-in-flight finalize's
        // snapshot_id instead of re-capturing and racing itself.
        if let Some(existing) = self.pending_finalizes.get(&id) {
            return Ok(*existing);
        }
        let Some(session_id) = self.session_bindings.get(&id).map(|e| *e) else {
            return Err(SandboxError::InvalidSpec(
                "host-durable eviction finalize requires a session-bound sandbox".into(),
            ));
        };

        let (capture_guard, cap) = self.capture_phase(id).await?;
        // Publish the working-set trace for the next resume's prefault (see
        // `spawn_trace_publish`); detached, never blocks the eviction.
        self.spawn_trace_publish(id);
        // Idempotent re-pause; best-effort (a failure leaves the orphan
        // running until destroy, which is today's behavior).
        if let Err(e) = self.inner.pause(id).await {
            tracing::debug!(sandbox_id = %id, error = %e, "post-capture re-pause failed (benign)");
        }

        let SnapshotCapture {
            metadata,
            dest,
            chain_prev,
            paused_at,
            mut unwind,
        } = cap;
        let snapshot_id = metadata.id;

        // Extract + durably persist the drained disk-flush chunks (if
        // any) BEFORE returning. See `PendingDiskFlush::into_chunks` and
        // the module-level deviation note in `eviction_finalize.rs`: we
        // never call `flush_upload` on this handle — the sandbox is
        // about to be destroyed, so there is no live reader left for its
        // rebase side effect to matter to.
        #[cfg(target_os = "linux")]
        let (disk_pending_record, hot_disk_chunks) = match (
            unwind.disk_backend.take(),
            unwind.disk_pending.take(),
        ) {
            (Some(backend), Some(pending)) => {
                let base_manifest = backend.manifest_ref().await;
                let chunk_size = backend.chunk_size();
                let total_bytes = backend.total_bytes();
                let chunks = pending.into_chunks();
                if let Err(e) =
                    crate::eviction_finalize::persist_disk_pending_chunks(&dest, &chunks).await
                {
                    // The guest was already resumed by `capture_phase`
                    // (its `inner.snapshot`/`snapshot_diff` brought it
                    // back) — defusing is correct, not a stuck-paused
                    // guest. Clean up `dest` so this failure doesn't leak
                    // the multi-GiB local staging dir on every retry.
                    unwind.defuse();
                    drop(unwind);
                    // The Diff already consumed the dirty bitmap and the
                    // dir removal below deletes memory.diff — poison the
                    // chain so the retry captures Full (see
                    // `poison_checkpoint_chain_after_failed_diff`).
                    if chain_prev.is_some() {
                        poison_checkpoint_chain_after_failed_diff(
                            &self.checkpoint_chains,
                            self.chain_heads.as_deref(),
                            id,
                            "eviction disk-pending persist",
                        );
                    }
                    match tokio::fs::remove_dir_all(&dest).await {
                        Ok(_) => {}
                        Err(rm_err) => tracing::warn!(
                            sandbox_id = %id, dest = %dest.display(), error = %rm_err,
                            "snapshot_begin: disk-pending persist failed AND orphan dir cleanup failed",
                        ),
                    }
                    return Err(SandboxError::Snapshot(format!(
                        "persist disk-pending chunks: {e}"
                    )));
                }
                let record = Some(crate::eviction_finalize::DiskPendingRecord {
                    base_manifest,
                    chunk_size,
                    total_bytes,
                    chunks: chunks.iter().map(|(idx, hash, _)| (*idx, *hash)).collect(),
                });
                // ADR 0101 A: hand the drained bytes to the finalize job
                // in memory — `disk-pending/` above is the crash-redrive
                // journal, not the common read path.
                (record, Some(chunks))
            }
            _ => (None, None),
        };
        #[cfg(not(target_os = "linux"))]
        let (disk_pending_record, hot_disk_chunks): (
            Option<crate::eviction_finalize::DiskPendingRecord>,
            Option<Vec<(usize, engram_chunk_store::manifest::ChunkHash, bytes::Bytes)>>,
        ) = (None, None);

        // Ownership of the capture's recovery state transfers to the
        // finalize record + the spawned job from here — the same
        // "defuse the instant something durable/owned takes over" rule
        // `finish()` follows.
        unwind.defuse();
        drop(unwind);

        let record = crate::eviction_finalize::EvictionFinalizeRecord {
            snapshot_id,
            session_id,
            sandbox_id: id,
            image_version: metadata.image_version.clone(),
            size_bytes: metadata.size_bytes,
            paused_at,
            captured_at: metadata.created_at,
            dest: dest.clone(),
            chain_prev_ref: chain_prev.map(|(r, _)| r),
            disk_pending: disk_pending_record,
            aux_bundles: metadata.aux_bundles.clone(),
            stage: crate::eviction_finalize::FinalizeStage::default(),
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        };
        let Some(finalizer) = self.eviction_finalizer() else {
            // Gated above; unreachable in practice (checkpoint_dir just
            // got checked), but never destroy a fresh capture on a
            // defensive None — clean up and fail loudly instead.
            if record.chain_prev_ref.is_some() {
                poison_checkpoint_chain_after_failed_diff(
                    &self.checkpoint_chains,
                    self.chain_heads.as_deref(),
                    id,
                    "eviction finalizer unavailable",
                );
            }
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return Err(SandboxError::InvalidSpec(
                "checkpoint_dir disappeared between the gate check and record construction".into(),
            ));
        };
        if let Err(e) = record
            .persist(finalizer.fs.as_ref(), &finalizer.finalize_dir())
            .await
        {
            // Finding 4: mirror the disk-pending failure arm above and the
            // `None` arm just before it — a persist failure here must not
            // leak the multi-GiB local staging dir. The eviction scanner
            // retries ~30s apart, each attempt minting a fresh snapshot_id
            // + `dest`; without this cleanup that's an unbounded disk-fill
            // class on a host whose finalize_dir write path is unhealthy.
            // The removal deletes memory.diff, whose dirty set the Diff
            // already consumed — poison the chain so the retry is a Full.
            if record.chain_prev_ref.is_some() {
                poison_checkpoint_chain_after_failed_diff(
                    &self.checkpoint_chains,
                    self.chain_heads.as_deref(),
                    id,
                    "eviction finalize-record persist",
                );
            }
            let _ = tokio::fs::remove_dir_all(&dest).await;
            return Err(SandboxError::Snapshot(format!(
                "persist eviction finalize record: {e}"
            )));
        }
        metrics::counter!(crate::metrics::EVICTION_FINALIZE_PERSISTED_TOTAL).increment(1);
        self.pending_finalizes.insert(id, snapshot_id);

        tokio::spawn(async move {
            // The capture lock rides into the task: periodic checkpoints
            // stay locked out until the finalize completes (chain
            // bookkeeping is not concurrent-safe per sandbox) — same
            // guarantee the pre-#529 shape gave, now held for the whole
            // (re-drivable) job instead of just one process's attempt.
            crate::eviction_finalize::run_eviction_finalize(
                finalizer,
                record,
                capture_guard,
                hot_disk_chunks,
            )
            .await;
        });
        Ok(snapshot_id)
    }

    /// ADR 0045 D5: await the background post phase.
    ///
    /// Issue #221: idempotent + retryable. We CLONE the stored shared
    /// future and await the clone — we do NOT remove the map entry up
    /// front. A coordinator's finalize RPC is at-least-once: a deadline
    /// or pod restart drops the server-side handler future mid-await,
    /// and a raw-`JoinHandle` map (the old shape) would lose the only
    /// reader of a fully-uploaded snapshot and wedge finalize forever.
    /// With the shared future, a retried wait re-resolves to the same
    /// result, and two concurrent replicas both observe it. The entry
    /// is removed only after a *successful* consumption; an error is
    /// left in place so a retry can re-observe it (and supersession /
    /// destroy do the cleanup on the failure path).
    async fn snapshot_wait(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let shared = self
            .snapshot_waits
            .get(&id)
            .map(|w| w.shared.clone())
            .ok_or_else(|| {
                SandboxError::Snapshot(format!("no snapshot_begin in flight for sandbox {id}"))
            })?;
        // Await the CLONE — if this future is dropped (cancelled wait),
        // the entry and the underlying task are untouched, so a retry
        // re-clones and re-awaits.
        match shared.await {
            Ok(meta) => {
                // Consumed successfully — drop the slot so it doesn't
                // leak. A concurrent waiter that already cloned the
                // shared future still resolves from its own clone.
                self.snapshot_waits.remove(&id);
                Ok(meta)
            }
            // The post phase genuinely failed. Leave the entry in place
            // so a retry re-observes the error rather than a misleading
            // "no snapshot_begin in flight"; destroy / supersession
            // reclaims the slot. Unwrap the shared `Arc<SandboxError>`
            // back into an owned error for the caller.
            Err(e) => Err(match Arc::try_unwrap(e) {
                Ok(owned) => owned,
                Err(shared_err) => SandboxError::Snapshot(format!("{shared_err}")),
            }),
        }
    }

    /// ADR 0045 C2: the pre-pause presetup. Mints the export identity
    /// + packages the restore inputs derivable BEFORE the pause: the
    /// sidecar (live composition — nothing in it changes across the
    /// freeze), the inline CHAIN manifest (the session manifest ref is
    /// the chain's CURRENT durable ref: post-copy never re-chunks at
    /// capture, sealed chunks override via the peer at fault time, and
    /// the dest's first checkpoint is a safe Full = the durability
    /// catch-up), the live disk ref, and the hot set. NO pause, no
    /// fence — the guest runs until `migration_capture_postcopy`.
    async fn migration_presetup(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        let Some(peer) = self.migrate_peer_server() else {
            return Err(SandboxError::InvalidSpec(
                "no page server on this host — use snapshot-rehome".into(),
            ));
        };
        let Some((chain_ref, chain_manifest)) = self
            .checkpoint_chains
            .get(&id)
            .map(|c| (c.manifest_ref, c.manifest.clone()))
        else {
            return Err(SandboxError::InvalidSpec(
                "no checkpoint chain for this sandbox — use snapshot-rehome".into(),
            ));
        };
        if self.migrations.validate_open(id) {
            return Err(SandboxError::AlreadyExists);
        }
        // A lingering pending presetup is an ABANDONED move (the
        // coordinator died between the halves; nothing else can hold
        // one — the session lease serializes movers). Last-write-wins:
        // refusing here would brick every future move for this sandbox
        // until a host-agent restart.
        if let Some(stale) = self.pending_presetups.get(&id) {
            tracing::warn!(
                sandbox_id = %id,
                stale_export = %stale.export_id,
                "presetup superseding an abandoned pending presetup",
            );
        }
        if self.inner.post_copy_source_view(id).is_none() {
            return Err(SandboxError::InvalidSpec(
                "not a substrate sandbox (no base mapping) — use snapshot-rehome".into(),
            ));
        }

        let sidecar_json = self.inner.compose_live_sidecar(id, Some(chain_ref))?;
        let memory_manifest_json = serde_json::to_vec(&chain_manifest)
            .map_err(|e| SandboxError::Snapshot(format!("chain manifest json: {e}")))?;

        // INVARIANT (see `nbd_sandboxes`): clone the Arc + copy the
        // device path out of the guard, then drop it before the fsync
        // join and `manifest_ref().await` below.
        #[cfg(target_os = "linux")]
        let backend_dev = self
            .nbd_sandboxes
            .get(&id)
            .map(|entry| (entry.backend.clone(), entry.device_path().to_path_buf()));
        #[cfg(target_os = "linux")]
        let disk_manifest_ref = match backend_dev {
            Some((backend, dev)) => {
                // Disk post-copy pre-copy leg: push the host block
                // cache down into the daemon's dirty buffer WHILE the
                // guest still runs, so the capture's under-freeze
                // fsync only carries the since-presetup delta (and
                // any first-touch RMW base fetches happen off the
                // blackout). Best-effort — the capture's fsync is the
                // coherence-bearing one.
                let r = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&dev)?;
                    f.sync_all()
                })
                .await
                .map_err(|e| std::io::Error::other(format!("presetup fsync join: {e}")))
                .and_then(|inner| inner);
                if let Err(e) = r {
                    tracing::warn!(sandbox_id = %id, error = %e,
                        "presetup pre-pause NBD fsync failed (non-fatal; capture re-fsyncs)");
                }
                Some(backend.manifest_ref().await)
            }
            None => None,
        };
        #[cfg(not(target_os = "linux"))]
        let disk_manifest_ref: Option<engram_core::types::manifest::ManifestRef> = None;

        let export_id = crate::migration::MigrationRegistry::mint_export_id();
        let peer_token = crate::migration::MigrationRegistry::mint_export_id();
        self.pending_presetups.insert(
            id,
            PendingPresetup {
                export_id: export_id.clone(),
                peer_token: peer_token.clone(),
                chain_ref,
            },
        );

        Ok(engram_core::types::snapshot::MigrationPresetupOut {
            export_id,
            peer_token,
            peer_port: peer.port(),
            sidecar_json,
            memory_manifest_json,
            memory_manifest_ref: chain_ref,
            disk_manifest_ref,
            hot_chunks: self.read_hot_chunks(id),
        })
    }

    /// ADR 0045 C2: the blackout half. Pause → NBD host-cache fsync +
    /// dirty-tail drain (kept local) → fork-v3 vmstate-only package →
    /// pagemap scan → register the export (page server SEALS; the
    /// parked dest handler unblocks). The guest stays paused serving
    /// pages until commit (post-drain) or abort.
    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, export_id);
            Err(SandboxError::InvalidSpec(
                "post-copy capture is Linux-only".into(),
            ))
        }
        #[cfg(target_os = "linux")]
        {
            use engram_core::types::snapshot::PostCopyCaptureOut;
            // Issue #222: remove *only* if the export id matches. A plain
            // `remove(&id).filter(...)` evicts the entry unconditionally and
            // only then discards the returned value on mismatch — so a stale
            // straggler `capture(id, E1)` arriving after presetup re-minted E2
            // (last-write-wins, see `migration_presetup`) would delete the live
            // E2 entry and then fail, dooming the legitimate `capture(id, E2)`
            // to "no matching presetup". `remove_if` removes-and-returns only
            // when the predicate holds, atomically, so a mismatched straggler
            // leaves the live presetup intact and just errors.
            let Some((_, pending)) = self
                .pending_presetups
                .remove_if(&id, |_, p| p.export_id == export_id)
            else {
                return Err(SandboxError::InvalidSpec(
                    "no matching presetup for this capture (export id skew?)".into(),
                ));
            };
            // Issue #202: keep a copy of the consumed presetup so an
            // unwind can re-insert it — otherwise a coordinator retry
            // hits "no matching presetup" and the dest parks until its
            // 120 s budget burns out.
            let presetup_restore = pending.clone();
            let Some((chain_ref, chain_manifest)) = self
                .checkpoint_chains
                .get(&id)
                .map(|c| (c.manifest_ref, c.manifest.clone()))
            else {
                return Err(SandboxError::InvalidSpec(
                    "checkpoint chain vanished since presetup".into(),
                ));
            };
            if chain_ref != pending.chain_ref {
                // A checkpoint landed between presetup and capture: the
                // dest is restoring against a STALE manifest — its
                // resolve() view would skew vs. ALT_SOURCE. Refuse; the
                // coordinator retries the whole move.
                return Err(SandboxError::InvalidSpec(
                    "chain advanced between presetup and capture — retry the move".into(),
                ));
            }
            let Some(peer) = self.migrate_peer_server() else {
                return Err(SandboxError::InvalidSpec("page server vanished".into()));
            };
            let Some(view) = self.inner.post_copy_source_view(id) else {
                return Err(SandboxError::InvalidSpec(
                    "no post-copy source view (substrate mapping gone?)".into(),
                ));
            };
            if self.migrations.validate_open(id) {
                return Err(SandboxError::AlreadyExists);
            }

            // Checkpoint fence: held for the export's lifetime.
            let capture_guard = self.capture_lock(id).lock_owned().await;

            // Blackout leg 1: pause. (PR 10 decomposition.)
            let t_pause = crate::time_source::metrics_now();
            let paused_at = self.clock.now_utc();
            self.inner
                .pause(id)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("post-copy pause: {e}")))?;
            let pause_ms = t_pause.elapsed().as_millis() as u64;

            // Issue #202: arm the unwind guard the instant the guest is
            // paused. Any error/cancel before the export is registered
            // must resume the guest, clear the fence, re-queue the
            // (later-taken) seal, AND restore the consumed presetup. The
            // old `FenceGuard` only lowered the fence — it left the guest
            // frozen, dropped the seal, and consumed the presetup.
            let mut unwind = CaptureUnwind::new(self.inner.clone(), id);
            unwind.arm();
            unwind.presetup_restore = Some((id, presetup_restore, self.pending_presetups.clone()));

            // Blackout leg 2: disk COHERENCE (disk post-copy). Fence
            // flush publishes, push the host's block-device page cache
            // down into the daemon (the presetup's pre-pause fsync
            // shrank this to the since-presetup delta), quiesce
            // in-flight requests. The SEAL itself is taken LAST (after
            // vmstate + scan — the guest is paused, so the dirty map
            // is stable): it DRAINS the dirty buffer, so nothing
            // fallible may sit between the drain and the export
            // taking ownership (a dropped seal = the guest's disk
            // writes silently gone on the abort-resume). The unwind
            // guard (issue #202) re-queues the seal, clears the fence,
            // resumes the guest, and restores the presetup on every
            // pre-export error arm AND on wire cancellation — a fenced
            // backend whose capture failed would otherwise no-op
            // flushes forever (silent durability stall).
            let disk_entry = self
                .nbd_sandboxes
                .get(&id)
                .map(|e| (e.backend.clone(), e.device_path().to_path_buf()));
            let t_disk = crate::time_source::metrics_now();
            if let Some((backend, dev)) = &disk_entry {
                backend.set_migration_fence(true);
                // Issue #202: record the fence on the unwind guard.
                unwind.disk_backend = Some(backend.clone());
                unwind.fenced = true;
                let dev = dev.clone();
                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&dev)?;
                    f.sync_all()
                })
                .await
                .map_err(|e| SandboxError::Snapshot(format!("nbd flush join: {e}")))?
                .map_err(|e| SandboxError::Snapshot(format!("nbd host-cache flush: {e}")))?;
                backend.wait_idle().await;
            }
            let mut disk_drain_ms = t_disk.elapsed().as_millis() as u64;

            // Blackout leg 3: vmstate. Fork v3: state.bin + sidecar
            // only — the memory artifact never materializes (the whole
            // point).
            let t_vmstate = crate::time_source::metrics_now();
            let sidecar = self.inner.compose_live_sidecar(id, Some(chain_ref))?;
            let (snapshot_id, export_dir) = self
                .inner
                .snapshot_vmstate_only_package(id, &sidecar)
                .await?;
            let _ = snapshot_id;
            let vmstate_ms = t_vmstate.elapsed().as_millis() as u64;

            // The pagemap dirty map (blackout-critical; measured).
            let scan_started = crate::time_source::metrics_now();
            let base_path = crate::dirty_map::find_base_mapping(view.fc_pid, &view.uffd_base_dir)
                .map_err(|e| SandboxError::Snapshot(format!("base mapping scan: {e}")))?
                .ok_or_else(|| {
                    SandboxError::Snapshot(
                        "FC process maps no substrate base file — cannot post-copy".into(),
                    )
                })?;
            let vmas = crate::dirty_map::guest_vmas(view.fc_pid, &base_path)
                .map_err(|e| SandboxError::Snapshot(format!("guest vmas: {e}")))?;
            if vmas.is_empty() {
                return Err(SandboxError::Snapshot(
                    "no base-backed guest VMAs — cannot post-copy".into(),
                ));
            }
            let chunk_size = chain_manifest.chunk_size.as_u64();
            let total_bytes = chain_manifest.total_bytes;
            let seal =
                crate::dirty_map::scan_dirty_chunks(view.fc_pid, &vmas, chunk_size, total_bytes)
                    .map_err(|e| SandboxError::Snapshot(format!("pagemap scan: {e}")))?;
            let scan_ms = scan_started.elapsed().as_millis() as u64;
            let sealed_chunks = seal.count_ones();
            let total_chunks = seal.chunk_count;

            // durable_at: chunk idx -> the chain's hash at that offset
            // (ALT_SOURCE comparisons), plus the GetChunk allowlist.
            let mut durable_at: Vec<Option<engram_chunk_store::manifest::ChunkHash>> =
                vec![None; total_bytes.div_ceil(chunk_size) as usize];
            let allowed: std::collections::HashSet<engram_chunk_store::manifest::ChunkHash> =
                chain_manifest.chunks.iter().map(|c| c.hash).collect();
            for c in &chain_manifest.chunks {
                let idx = (c.offset / chunk_size) as usize;
                if let Some(slot) = durable_at.get_mut(idx) {
                    *slot = Some(c.hash);
                }
            }

            // The disk SEAL — taken last (see the leg-2 comment): the
            // drain empties `dirty`, so from here every error arm must
            // requeue before returning. Only the descriptor write and
            // the export insert sit in that window.
            let t_seal = crate::time_source::metrics_now();
            let (disk_seal, disk_seal_info) = if let Some((backend, _)) = &disk_entry {
                let (sealed, base_manifest, base_ref) = backend.seal_for_postcopy().await;
                let info = serde_json::json!({
                    "sealed_chunk_indices": sealed.indices(),
                    "manifest": base_manifest,
                    "disk_ref_bincode": bincode::serialize(&base_ref).ok(),
                });
                (Some(std::sync::Arc::new(sealed)), info)
            } else {
                (
                    None,
                    serde_json::json!({
                        "sealed_chunk_indices": Vec::<u64>::new(),
                        "manifest": serde_json::Value::Null,
                        "disk_ref_bincode": serde_json::Value::Null,
                    }),
                )
            };
            let sealed_disk_chunks = disk_seal.as_ref().map(|s| s.len()).unwrap_or(0) as u64;
            // Issue #202: the seal has DRAINED the dirty buffer — from
            // here the bytes live only in `disk_seal`. Move a reference
            // into the unwind guard so any error/cancel re-queues them
            // (the seal is Arc-shared; the export gets its own clone).
            // This subsumes the per-arm `requeue_postcopy_seal` calls
            // below AND covers the wire-cancellation case they couldn't.
            unwind.disk_seal = disk_seal.clone();
            // The seal descriptor rides the export for the dest's
            // fetch poller (base manifest content + sealed indices).
            // A NEW filename on purpose: an OLD destination still
            // polling `disk-manifest.json` gets a fetch error forever
            // → its FC load gate times out → NeverLoaded → zero-loss
            // abort-to-source, never a silently-stale disk.
            let descriptor_written = match serde_json::to_vec(&disk_seal_info) {
                Ok(bytes) => tokio::fs::write(export_dir.join("disk-seal.json"), bytes)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("write disk seal info: {e}"))),
                Err(e) => Err(SandboxError::Snapshot(format!("disk seal info: {e}"))),
            };
            // On error the unwind guard re-queues the seal, clears the fence,
            // resumes the guest, and restores the presetup.
            descriptor_written?;
            disk_drain_ms += t_seal.elapsed().as_millis() as u64;

            // Register BOTH exports under the same identity, then the
            // role. Page-server registration is what unparks the dest
            // handler (the SEAL push) — last, after everything that
            // could still fail.
            let state_served = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let last_activity = std::sync::Arc::new(std::sync::Mutex::new(self.clock.now_mono()));
            // Issue #216 Gap 2: the peer page server shares THIS clock so
            // its TCP-only NeedAt/GetChunk serves refresh the same TTL
            // anchor the dumb-host sweep reads via `expired()`.
            let peer_last_activity = last_activity.clone();
            let inserted = self.migrations.insert(crate::migration::MigrationExport {
                export_id: export_id.to_string(),
                sandbox_id: id,
                snapshot_dir: export_dir,
                allowed_chunks: allowed.clone(),
                disk_pending: None,
                disk_seal,
                clock: self.clock.clone(),
                created_at: self.clock.now_mono(),
                post_copy: true,
                state_served,
                last_activity,
                capture_guard,
            });
            if !inserted {
                // The unwind guard re-queues the seal (Arc clone),
                // clears the fence, resumes, and restores the presetup.
                return Err(SandboxError::AlreadyExists);
            }
            // Issue #202: the export now owns the fence + seal and the
            // dest is about to load — defuse the guard (abort unfences;
            // commit destroys the sandbox). After this point the
            // presetup must NOT be restored (the move is committing).
            unwind.defuse();
            self.set_migration_role(id, Some(crate::migration::MigrationRole::PostCopySource))
                .await;
            peer.register(crate::migrate_peer::PeerExport {
                export_id: export_id.to_string(),
                token: pending.peer_token,
                sandbox_id: id,
                fc_pid: view.fc_pid,
                vmas,
                seal,
                durable_at,
                allowed_chunks: allowed,
                chunk_size,
                total_bytes,
                serve: Default::default(),
                drained: std::sync::atomic::AtomicBool::new(false),
                last_activity: peer_last_activity,
                clock: self.clock.clone(),
            });

            tracing::info!(
                sandbox_id = %id,
                export_id,
                sealed_chunks,
                total_chunks,
                sealed_disk_chunks,
                pause_ms,
                disk_drain_ms,
                vmstate_ms,
                scan_ms,
                "post-copy capture sealed; guest frozen as page server (ADR 0045 C2)",
            );
            Ok(PostCopyCaptureOut {
                sealed_chunks,
                total_chunks,
                pause_ms,
                disk_drain_ms,
                vmstate_ms,
                scan_ms,
                sealed_disk_chunks,
                paused_at_unix_ms: paused_at.timestamp_millis(),
            })
        }
    }

    /// ADR 0045 C2 (destination): await BOTH drains' terminal
    /// outcomes — memory via the peer-mode handler's control socket,
    /// disk via the NBD backend's overlay subscription. The source is
    /// released only when the dest depends on it for NOTHING.
    /// `PeerLost` (either plane) pauses (poisons) the dest VM before
    /// returning — the caller rewinds.
    async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        use engram_core::types::snapshot::DrainOutcome;
        let Some(sock) = self.inner.post_copy_control_sock(id) else {
            return Err(SandboxError::InvalidSpec(
                "no post-copy control socket for this sandbox".into(),
            ));
        };
        // Subscribe BEFORE awaiting memory: a disk drain that fails
        // while we're parked on the control socket keeps its overlay
        // (lost-latched) for us to read; one that succeeds clears the
        // overlay, which reads as "nothing to wait for" — also right.
        #[cfg(target_os = "linux")]
        let disk_drain = self
            .nbd_sandboxes
            .get(&id)
            .and_then(|e| e.backend.postcopy_drain_subscribe());
        let outcome = tokio::task::spawn_blocking(move || -> Result<DrainOutcome, String> {
            let mut stream = std::os::unix::net::UnixStream::connect(&sock)
                .map_err(|e| format!("dial control sock: {e}"))?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(600)))
                .map_err(|e| format!("control sock timeout: {e}"))?;
            loop {
                let msg: engram_migrate_proto::HandlerControl =
                    engram_migrate_proto::read_frame(&mut stream)
                        .map_err(|e| format!("control frame: {e}"))?;
                match msg {
                    engram_migrate_proto::HandlerControl::DrainDone {
                        pulled,
                        alt_sourced,
                        zero_chunks,
                        ms,
                        faults,
                        fault_us,
                        fault_max_us,
                    } => {
                        // Restore-tail attribution: the fault-path
                        // totals are the serial P2P cost inside the FC
                        // load + early guest execution (cross-ref with
                        // the load_ms log).
                        tracing::info!(
                            faults,
                            fault_ms = fault_us / 1000,
                            fault_max_us,
                            "post-copy memory drain done (handler fault-path totals)",
                        );
                        return Ok(DrainOutcome::Done {
                            pulled,
                            alt_sourced,
                            zero_chunks,
                            ms,
                        });
                    }
                    engram_migrate_proto::HandlerControl::PeerLost { remaining, detail } => {
                        return Ok(DrainOutcome::PeerLost { remaining, detail });
                    }
                    engram_migrate_proto::HandlerControl::Sealed { .. }
                    | engram_migrate_proto::HandlerControl::DrainProgress { .. } => continue,
                }
            }
        })
        .await
        .map_err(|e| SandboxError::Snapshot(format!("drain wait join: {e}")))?
        .map_err(SandboxError::Snapshot)?;

        // Disk plane: the memory drain finishing doesn't make the dest
        // self-sufficient — sealed disk chunks may still live only in
        // the source's RAM. Skip when memory already lost (rewind
        // either way).
        #[cfg(target_os = "linux")]
        let outcome = match outcome {
            DrainOutcome::Done { .. } => match Self::await_disk_drain(disk_drain).await {
                Ok(_) => outcome,
                Err(detail) => DrainOutcome::PeerLost {
                    remaining: 0,
                    detail: format!("disk drain: {detail}"),
                },
            },
            lost => lost,
        };

        if let DrainOutcome::PeerLost { remaining, detail } = &outcome {
            tracing::error!(
                sandbox_id = %id,
                remaining,
                detail,
                "post-copy drain lost its peer — poisoning (pausing) the dest VM",
            );
            if let Err(e) = self.inner.pause(id).await {
                tracing::warn!(sandbox_id = %id, error = %e, "poison-pause failed");
            }
        } else {
            // Both drains complete: the dest no longer depends on the
            // source.
            self.set_migration_role(id, None).await;
        }
        Ok(outcome)
    }

    /// ADR 0045 C1: freeze for a live move. See `crate::migration`'s
    /// module docs for the lifecycle; the export holds the capture
    /// lock (= the checkpoint fence) and the NBD backend's
    /// migration_fence (= the flush/publish fence) until commit/abort.
    async fn migration_capture(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationCaptureOut, SandboxError> {
        use engram_core::types::snapshot::MigrationCaptureOut;
        let Some((chain_ref, chain_manifest)) = self
            .checkpoint_chains
            .get(&id)
            .map(|c| (c.manifest_ref, c.manifest.clone()))
        else {
            // No chain (host restarted, seed failed) — the composed
            // snapshot-rehome path handles it; signal fallback.
            return Err(SandboxError::InvalidSpec(
                "no checkpoint chain for this sandbox — use snapshot-rehome".into(),
            ));
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return Err(SandboxError::InvalidSpec(
                "no chunk store — use snapshot-rehome".into(),
            ));
        };
        let Some(cache) = self.chunk_cache.clone() else {
            return Err(SandboxError::InvalidSpec(
                "no chunk cache — use snapshot-rehome".into(),
            ));
        };
        if self.migrations.validate_open(id) {
            return Err(SandboxError::AlreadyExists);
        }

        // Checkpoint fence: held for the export's lifetime.
        let capture_guard = self.capture_lock(id).lock_owned().await;

        // Write-ahead invalidate of the durable chain-head record — the
        // C1 diff create below consumes the KVM dirty bitmap exactly
        // like capture_phase's (see the comment there). Note the C2
        // post-copy capture (`migration_capture_postcopy`) deliberately
        // does NOT invalidate: its fork-v3 vmstate-only snapshot never
        // touches the bitmap (persist.rs gates the memory dump on
        // `!vmstate_only`), and an aborted C2 resumes with bitmap AND
        // chain intact — removing the record there would needlessly
        // cost the survivor a Full after a later roll.
        if let Some(store) = &self.chain_heads {
            store.invalidate(id).map_err(|e| {
                SandboxError::Snapshot(format!("chain-head write-ahead invalidate: {e}"))
            })?;
        }

        match self.inner.wait_agent_ready(id).await {
            Ok(()) => {}
            Err(SandboxError::InvalidSpec(_)) => {}
            Err(e) => return Err(SandboxError::Snapshot(format!("wait_agent_ready: {e}"))),
        }
        let paused_at = self.clock.now_utc();
        self.inner
            .pause(id)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("migration pause: {e}")))?;

        // Issue #202: arm the unwind guard the instant the guest is
        // paused. From here every error/cancel until the export is
        // registered must resume the guest, clear the fence, and requeue
        // the drained dirty buffer — otherwise the session is bricked
        // (frozen + fence stuck + acked writes silently lost). The guard
        // owns the drained `disk_pending` through the window; we hand it
        // to the export only after the registry insert succeeds.
        let mut unwind = CaptureUnwind::new(self.inner.clone(), id);
        unwind.arm();

        // Disk: drain under the pause, land the pending tier in the
        // LOCAL cache, fence further flush publishes.
        // INVARIANT (see `nbd_sandboxes`): clone the Arc + copy the
        // device path out of the guard, then drop it before the
        // multi-second drain (`fsync` join + `flush_local` +
        // `flush_to_local_cache`) — a held guard parks contending
        // `destroy`/`create` workers on the shard's sync RwLock.
        #[cfg(target_os = "linux")]
        let backend_dev = self
            .nbd_sandboxes
            .get(&id)
            .map(|entry| (entry.backend.clone(), entry.device_path().to_path_buf()));
        #[cfg(target_os = "linux")]
        let (disk_manifest_json, disk_ref, disk_hashes) = if let Some((backend, dev)) = backend_dev
        {
            backend.set_migration_fence(true);
            // Issue #202: the fence is now raised — record it on the
            // guard so an unwind clears it (else `flush()` no-ops for
            // the sandbox's lifetime).
            unwind.disk_backend = Some(backend.clone());
            unwind.fenced = true;
            // Push the HOST's block-device page cache down to the
            // daemon before draining. FC's virtio-blk writes to
            // /dev/nbdN through the kernel page cache (drive
            // cache_type default = Unsafe: guest FLUSH does not
            // propagate), so without this fsync the drain captures
            // only what background writeback happened to push —
            // an ACTIVE guest's recent writes were still in the
            // host cache and the export shipped a chunk with
            // zeros/stale bytes where they belonged (the two-host
            // NBD e2e probe; idle evictions dodge it because a
            // quiescent session ages past the writeback interval).
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&dev)?;
                f.sync_all()
            })
            .await
            .map_err(|e| SandboxError::Snapshot(format!("nbd host-cache flush join: {e}")))?
            .map_err(|e| SandboxError::Snapshot(format!("nbd host-cache flush: {e}")))?;
            backend.wait_idle().await;
            let pending = backend
                .flush_local()
                .await
                .map_err(|e| SandboxError::Snapshot(format!("migration disk drain: {e}")))?;
            // Issue #202: the dirty buffer is now drained OUT of the
            // backend and lives only in `pending`. Move it into the
            // guard immediately so any subsequent error/cancel
            // requeues it (instead of dropping it on the floor). The
            // success path takes it back out before the export insert.
            unwind.disk_pending = Some(pending);
            let pending = unwind.disk_pending.as_ref().expect("just set");
            let (m, hashes) = backend
                .flush_to_local_cache(pending)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("migration disk cache: {e}")))?;
            let dref = backend.manifest_ref().await.next_version();
            (
                serde_json::to_vec(&m)
                    .map_err(|e| SandboxError::Snapshot(format!("disk manifest json: {e}")))?,
                dref,
                hashes,
            )
        } else {
            (
                Vec::new(),
                engram_core::types::manifest::ManifestRef::new(),
                Vec::new(),
            )
        };
        #[cfg(not(target_os = "linux"))]
        let (disk_manifest_json, disk_ref, disk_hashes) = (
            Vec::new(),
            engram_core::types::manifest::ManifestRef::new(),
            Vec::<engram_chunk_store::manifest::ChunkHash>::new(),
        );

        // FC diff capture. `snapshot_diff` resumes the guest on
        // success — re-pause immediately (the guest is mid-move; its
        // post-capture execution would be discarded anyway, exactly
        // the D5 argument).
        let create_res = self.inner.snapshot_diff(id).await;
        // The diff just consumed+reset the KVM dirty bitmap, but its
        // resulting manifest only becomes durable on the DESTINATION
        // (the re-chunk below sinks to the local cache; the dest's
        // catch-up publishes). If this export is later ABORTED (explicit
        // abort or the TTL sweep's AbortInPlace) the guest resumes here
        // with a bitmap baseline the local chain head does not describe
        // — a subsequent diff against it would silently omit every page
        // dirtied before this capture, the same corruption class the
        // failed-diff poison exists for. Retire the chain (and its
        // durable record, already write-ahead-removed above) NOW, on
        // success and failure alike: commit destroys the sandbox anyway,
        // and an abort costs one recovery Full instead of a
        // silently-incomplete diff. (Advancing the chain to the new ref
        // instead would be unsound: its chunks are cache-only until the
        // dest publishes.)
        let metadata = match create_res {
            Ok(m) => {
                if self.checkpoint_chains.remove(&id).is_some() {
                    tracing::info!(
                        sandbox_id = %id,
                        "C1 capture consumed the dirty bitmap; chain retired — an aborted \
                         move's next capture will be a FULL snapshot",
                    );
                }
                m
            }
            Err(e) => {
                poison_checkpoint_chain_after_failed_diff(
                    &self.checkpoint_chains,
                    self.chain_heads.as_deref(),
                    id,
                    "migration diff capture",
                );
                return Err(SandboxError::Snapshot(format!(
                    "migration diff capture: {e}"
                )));
            }
        };
        if let Err(e) = self.inner.pause(id).await {
            tracing::debug!(sandbox_id = %id, error = %e, "post-capture re-pause failed (benign)");
        }
        let dest = self.inner.snapshot_path_for(metadata.id);

        // Local-sink re-chunk: dirty memory chunks into the NVMe cache.
        let diff_path = dest.join("memory.diff");
        let ranges = crate::checkpoint::dirty_ranges(&diff_path)
            .map_err(|e| SandboxError::Snapshot(format!("migration dirty ranges: {e}")))?;
        let (mem_manifest, mem_hashes) = chunk_store
            .update_for_dirty_ranges_sparse_with_sink(
                &chain_manifest,
                &diff_path,
                &ranges,
                Some(&cache),
            )
            .await
            .map_err(|e| SandboxError::Snapshot(format!("migration re-chunk: {e}")))?;
        let mem_ref = chain_ref.next_version();
        let _ = fs::remove_file(&diff_path).await;
        patch_fc_manifest_memory_ref(
            &dest.join("manifest.json"),
            mem_ref,
            self.session_for_sandbox(id),
        )
        .await?;

        let export_id = crate::migration::MigrationRegistry::mint_export_id();
        // Allowlist the FULL session manifest, not just the chunks new
        // since the last durable row: the destination pulls the whole
        // divergent set host-to-host (LAN) in the background instead of
        // faulting/prefetching ~hundreds of chunks from GCS — the last
        // tail of the post-teleport crawl (prod canary 52d6d808: a 959-
        // chunk inline prefetch took 115 s via GCS). The fetch handler
        // serves cache-resident chunks directly and falls back to the
        // store for anything evicted.
        let mut allowed: std::collections::HashSet<engram_chunk_store::manifest::ChunkHash> =
            mem_hashes.iter().copied().collect();
        allowed.extend(mem_manifest.chunks.iter().map(|c| c.hash));
        allowed.extend(disk_hashes.iter().copied());
        // Issue #202: hand the drained `disk_pending` from the guard to
        // the export, which now owns it (abort re-queues it; commit drops
        // it once the dest has drained). The guard's remaining job is the
        // resume + unfence on the (now narrow) insert-failure arm. (On
        // non-Linux the field was never populated, so this is `None`.)
        let disk_pending = unwind.disk_pending.take();
        let inserted = self.migrations.insert(crate::migration::MigrationExport {
            export_id: export_id.clone(),
            sandbox_id: id,
            snapshot_dir: dest,
            allowed_chunks: allowed,
            disk_pending,
            disk_seal: None,
            clock: self.clock.clone(),
            created_at: self.clock.now_mono(),
            // C1 stop-and-copy export: the guest stays frozen and the
            // dest pulls eagerly; post-copy captures (C2) construct
            // their own export with post_copy: true.
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(self.clock.now_mono())),
            capture_guard,
        });
        if !inserted {
            // The early `validate_open` + the held `capture_guard` make
            // this arm effectively unreachable, but keep the guard armed
            // so its Drop still resumes the guest + clears the fence.
            return Err(SandboxError::AlreadyExists);
        }
        // Issue #202: the export now owns the drained disk state, the
        // fence, and the frozen guest (abort/commit drive them from
        // here). Defuse the guard — the capture succeeded.
        unwind.defuse();
        tracing::info!(
            sandbox_id = %id,
            export_id = %export_id,
            mem_ref = %mem_ref,
            mem_chunks = mem_hashes.len(),
            disk_chunks = disk_hashes.len(),
            "migration capture complete; sandbox frozen, export open (ADR 0045 C1)",
        );
        // ADR 0045 C2 (E2B fold): the source guest's hot set in fault
        // order, from the handler's per-jail trace dump. Best-effort —
        // an absent/stale/corrupt file just means an empty rider.
        let hot_chunks = self.read_hot_chunks(id);

        Ok(MigrationCaptureOut {
            export_id,
            memory_manifest_json: serde_json::to_vec(&mem_manifest)
                .map_err(|e| SandboxError::Snapshot(format!("mem manifest json: {e}")))?,
            disk_manifest_json,
            memory_manifest_ref: mem_ref,
            disk_manifest_ref: disk_ref,
            new_memory_chunk_hashes: mem_hashes.iter().map(|h| *h.as_bytes()).collect(),
            new_disk_chunk_hashes: disk_hashes.iter().map(|h| *h.as_bytes()).collect(),
            hot_chunks,
            snapshot_id: metadata.id,
            paused_at_unix_ms: paused_at.timestamp_millis(),
        })
    }

    /// ADR 0045 C1: stream an export's artifacts. Allowlist-gated.
    async fn migration_fetch(
        &self,
        export_id: &str,
        items: Vec<engram_core::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<engram_core::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        use engram_core::types::snapshot::{MigrationFrame, MigrationItem};
        // Resolve + validate under the registry ref, then drop it (the
        // stream must not hold a dashmap guard).
        let (snapshot_dir, allowed, disk_seal) = {
            let Some(export) = self.migrations.find_by_export_id(export_id) else {
                return Err(SandboxError::NotFound);
            };
            // Issue #216 Gap 1: every serve refreshes the TTL clock for
            // ALL exports (C1 + post-copy) — an actively-fetched export
            // is alive by definition (`expired()` anchors on
            // `last_activity` now). And StateBin leaving the export arms
            // the split-brain guard regardless of mode: once `state.bin`
            // has shipped the dest may be running this state, so the
            // source must NEVER self-resume — `ttl_verdict`'s
            // forbidden-unpause arm protects C1 equally (a >120 s C1
            // teleport that shipped state then expired must NOT be
            // un-paused in place).
            export.touch();
            if items.iter().any(|i| matches!(i, MigrationItem::StateBin)) {
                export
                    .state_served
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            (
                export.snapshot_dir.clone(),
                export.allowed_chunks.clone(),
                export.disk_seal.clone(),
            )
        };
        for item in &items {
            if let MigrationItem::Chunk(h) = item {
                let hash = engram_chunk_store::manifest::ChunkHash::from_bytes(*h);
                if !allowed.contains(&hash) {
                    return Err(SandboxError::InvalidSpec(format!(
                        "chunk {hash} is not in this export's allowlist"
                    )));
                }
            }
        }
        let cache = self.chunk_cache.clone();
        let store_fallback = self.chunk_store.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<MigrationFrame, SandboxError>>(16);
        tokio::spawn(async move {
            const FRAME: usize = 1024 * 1024;
            for (idx, item) in items.into_iter().enumerate() {
                let idx = idx as u32;
                let bytes: Result<bytes::Bytes, SandboxError> = match item {
                    MigrationItem::StateBin => fs::read(snapshot_dir.join("state.bin"))
                        .await
                        .map(bytes::Bytes::from)
                        .map_err(|e| SandboxError::Snapshot(format!("read state.bin: {e}"))),
                    MigrationItem::Sidecar => fs::read(snapshot_dir.join("manifest.json"))
                        .await
                        .map(bytes::Bytes::from)
                        .map_err(|e| SandboxError::Snapshot(format!("read sidecar: {e}"))),
                    MigrationItem::DiskManifest => {
                        fs::read(snapshot_dir.join("disk-manifest.json"))
                            .await
                            .map(bytes::Bytes::from)
                            .map_err(|e| SandboxError::Snapshot(format!("read disk manifest: {e}")))
                    }
                    MigrationItem::DiskSealInfo => fs::read(snapshot_dir.join("disk-seal.json"))
                        .await
                        .map(bytes::Bytes::from)
                        .map_err(|e| SandboxError::Snapshot(format!("read disk seal info: {e}"))),
                    MigrationItem::DiskChunkAt(idx) => match &disk_seal {
                        // The seal IS the allowlist: only sealed
                        // indices are servable, straight from RAM.
                        Some(sealed) => sealed.get(idx).ok_or_else(|| {
                            SandboxError::InvalidSpec(format!(
                                "disk chunk {idx} is not sealed on this export"
                            ))
                        }),
                        None => Err(SandboxError::InvalidSpec(
                            "this export has no disk seal".into(),
                        )),
                    },
                    MigrationItem::Chunk(h) => {
                        let hash = engram_chunk_store::manifest::ChunkHash::from_bytes(h);
                        match &cache {
                            Some(cache) => cache
                                .get(hash, || async {
                                    // Full-manifest pulls may name a chunk
                                    // this host evicted; it's durable in
                                    // the store (only the NEW chunks are
                                    // cache-only, and those were pinned by
                                    // the local-sink re-chunk).
                                    match &store_fallback {
                                        Some(cs) => cs.get_chunk(hash).await,
                                        None => Err(
                                            engram_chunk_store::error::ChunkStoreError::Internal(
                                                "export chunk must be cache-resident".into(),
                                            ),
                                        ),
                                    }
                                })
                                .await
                                .map_err(|e| {
                                    SandboxError::Snapshot(format!("export chunk {hash}: {e}"))
                                }),
                            None => Err(SandboxError::Snapshot("no chunk cache".into())),
                        }
                    }
                };
                match bytes {
                    Ok(bytes) => {
                        let total = bytes.len();
                        let mut off = 0usize;
                        loop {
                            let end = (off + FRAME).min(total);
                            let frame = MigrationFrame {
                                item_idx: idx,
                                offset: off as u64,
                                data: bytes.slice(off..end),
                                last: end == total,
                            };
                            if tx.send(Ok(frame)).await.is_err() {
                                return;
                            }
                            if end == total {
                                break;
                            }
                            off = end;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });
        use futures::StreamExt;
        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }

    /// ADR 0045 C1: the move landed — drop the export (releasing both
    /// fences) and destroy the frozen VM + its local snapshot dir.
    async fn migration_commit(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        // Atomic validate+remove: nothing serializes this RPC against
        // the dumb-host TTL sweep, so a non-atomic validate-then-remove
        // would let both racers pass `validate` and panic the loser's
        // `remove().expect()`. `remove_validated` makes the race loser
        // see `None` → clean NotFound (callers already handle Err).
        let Some(export) = self.migrations.remove_validated(id, export_id) else {
            return Err(SandboxError::NotFound);
        };
        // ADR 0045 C2: a post-copy commit also retires the page-server
        // export (the dest reported DrainDone — or the coordinator gave
        // up on it) and the source's role fence.
        if let Some(peer) = self.migrate_peer_server() {
            peer.remove(export_id);
        }
        self.note_migration_role(id, None);
        let snapshot_dir = export.snapshot_dir.clone();
        drop(export); // releases the capture guard (checkpoint fence)
        if let Err(e) = self.destroy(id).await {
            tracing::warn!(sandbox_id = %id, error = %e,
                "migration commit: destroy failed; orphan_reap will clean up");
        }
        let _ = fs::remove_dir_all(&snapshot_dir).await;
        tracing::info!(sandbox_id = %id, "migration committed; source destroyed (ADR 0045 C1)");
        Ok(())
    }

    /// ADR 0045 C1: the move failed — re-queue the drained disk tier,
    /// unfence, un-pause in place. Zero loss.
    async fn migration_abort(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        // ADR 0045 C2 split-brain note: an EXPLICIT abort carries the
        // coordinator's knowledge that the dest provably never loaded
        // the shipped state (the postcopy-never-loaded marker), so it
        // is allowed even after StateBin was fetched. The forbidden
        // arm is the dumb-host TTL's self-resume — gated by
        // `ttl_verdict`'s state_served input, never reaching this RPC.
        //
        // Atomic validate+remove (see migration_commit): the TTL sweep
        // and a coordinator abort RPC race with nothing serializing
        // them; `remove_validated` ensures exactly one consumes the
        // export and the loser gets a clean NotFound instead of a panic.
        let Some(export) = self.migrations.remove_validated(id, export_id) else {
            return Err(SandboxError::NotFound);
        };
        if let Some(peer) = self.migrate_peer_server() {
            peer.remove(export_id);
        }
        self.set_migration_role(id, None).await;
        // INVARIANT (see `nbd_sandboxes`): clone the Arc and drop the
        // guard before the `requeue_*` awaits.
        #[cfg(target_os = "linux")]
        let backend = self.nbd_sandboxes.get(&id).map(|e| e.backend.clone());
        #[cfg(target_os = "linux")]
        if let Some(backend) = backend {
            if let Some(pending) = export.disk_pending {
                backend.requeue_pending(pending).await;
            }
            // Disk post-copy: the sealed bytes go back into `dirty`
            // so the resumed guest's next flush captures them.
            if let Some(sealed) = export.disk_seal.as_deref() {
                backend.requeue_postcopy_seal(sealed).await;
            }
            backend.set_migration_fence(false);
        }
        let snapshot_dir = export.snapshot_dir.clone();
        let _ = fs::remove_dir_all(&snapshot_dir).await;
        drop(export.capture_guard);
        self.inner
            .resume(id)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("migration abort resume: {e}")))?;
        tracing::info!(sandbox_id = %id, "migration aborted; guest resumed in place (ADR 0045 C1)");
        Ok(())
    }

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        // ADR 0014 issue #1/#2: caller's downstream pipeline
        // (record_snapshot → destroy → mark Idle) succeeded; the
        // snapshot is now owned by the `snapshots` row. Clear our
        // tracking so a subsequent snapshot() for this sandbox treats
        // the prior artifacts as no-longer-our-responsibility (the
        // committed snapshot stays on disk and in BlobStorage; PG holds
        // the durable reference).
        if self.inflight_snapshots.remove(&id).is_none() {
            tracing::debug!(
                sandbox_id = %id,
                "commit_snapshot called with no in-flight snapshot tracked; no-op",
            );
        }
        Ok(())
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        // ADR 0014 issue #1/#2: caller's downstream pipeline failed;
        // tear down the snapshot artifacts we just produced before
        // returning. Idempotent: if there's no in-flight snapshot for
        // this sandbox (commit already ran, or abort already ran, or
        // we never produced one), this is a clean no-op.
        if !self.inflight_snapshots.contains_key(&id) {
            return Ok(());
        }
        self.abort_prior_inflight_snapshot(id).await
    }

    fn working_set_trace_path(&self, id: SandboxId) -> Option<std::path::PathBuf> {
        self.inner.working_set_trace_path(id)
    }

    fn prefault_stats_path(&self, id: SandboxId) -> Option<std::path::PathBuf> {
        self.inner.prefault_stats_path(id)
    }

    fn snapshot_path_for(&self, snapshot_id: engram_core::types::SnapshotId) -> std::path::PathBuf {
        self.inner.snapshot_path_for(snapshot_id)
    }

    // ADR 0028 Fix A: defer to the wrapped backend — a PooledBackend
    // over FC checkpoints, over VZ/Process doesn't.
    fn supports_diff_checkpoints(&self) -> bool {
        self.inner.supports_diff_checkpoints()
    }

    /// ADR 0018 commit 12m: forward pause to the wrapped backend.
    /// PooledBackend doesn't have its own pause concept — it just
    /// delegates to whatever VMM is underneath. This is the EXTERNAL
    /// orchestration surface (the coordinator's rung-2 park rides it);
    /// every internal capture path quiesces via `self.inner.pause`
    /// directly, under its own capture lock.
    ///
    /// ADR 0091: refuse (typed, retryable) while a capture holds this
    /// sandbox's capture lock instead of racing it on FC's single-
    /// threaded API socket. This race — park pause vs the periodic
    /// checkpoint's own pause/flush/resume — made rung-2 parking fail
    /// on EVERY observed idle eviction (2026-07-11 campaign: 4/4 fell
    /// through to a 15-19 min full checkpoint). The caller (idle
    /// evictor) treats it like the checkpoint driver's own skip: retry
    /// next nomination.
    async fn pause(&self, id: SandboxId) -> Result<(), SandboxError> {
        if self.capture_in_flight(id) {
            ::metrics::counter!(
                crate::metrics::RUNG2_PARK_FAILED_TOTAL,
                "reason" => "capture_in_flight",
            )
            .increment(1);
            // `Unavailable` round-trips the gRPC boundary as the typed
            // RETRYABLE variant (ADR 0050 C), which is exactly the
            // caller contract: try the park again next nomination.
            return Err(SandboxError::Unavailable(format!(
                "pause {id}: a capture is in flight; retry after it completes"
            )));
        }
        let res = self.inner.pause(id).await;
        if let Err(e) = &res {
            ::metrics::counter!(
                crate::metrics::RUNG2_PARK_FAILED_TOTAL,
                "reason" => "vmm_pause",
            )
            .increment(1);
            tracing::warn!(%id, error = %e, "external pause failed at the VMM");
        }
        res
    }

    /// ADR 0018 commit 12m: forward resume. Symmetric with pause.
    ///
    /// ADR 0098 P7 (#739 follow-up): the **un-pause data-plane gate**. A
    /// rung-cancel resume must never un-pause a guest onto a rootfs NBD device
    /// that THIS host-agent generation does not serve — the 731df805 outcome
    /// (a coord-list gap left a rung-parked survivor's device unclaimed, the
    /// stale-binding sweep disconnected its live rootfs, and the un-pause
    /// landed on a dead data plane → EIO/garbage on the live guest, even
    /// though the guest never left `paused`). The local-survivor rehydrate
    /// pass is the primary fix; this gate is the last line — even if some
    /// future listing bug recurs, we fail fast into the `evict_local → resume`
    /// ladder rather than serving dead-plane reads.
    async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let is_nbd_backed = self.inner.rootfs_device(id).is_some();
            let served = self.nbd_sandboxes.contains_key(&id);
            let ok = engram_host_core::resume_data_plane_served(is_nbd_backed, served);
            // Soft-invariant (ADR 0099 H6): logs the alertable line but never
            // diverts control — the explicit early-return below is what routes
            // the caller into recovery.
            engram_core::soft_invariant!(
                "un-pause-dead-plane",
                ok,
                "resume {id}: rootfs NBD device is not served by this host-agent \
                 generation; refusing to un-pause onto a dead data plane"
            );
            if !ok {
                return Err(SandboxError::Vm(
                    format!(
                        "resume {id}: rootfs NBD data plane not served by this generation; \
                         routing to evict_local → resume"
                    )
                    .into(),
                ));
            }
        }
        self.inner.resume(id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        // ADR 0038 B2: capture the source's memory manifest before
        // `metadata` is moved, then seed the chain sparse after the VM
        // is up — so the first post-resume periodic checkpoint is a
        // cheap diff, not a Full re-read of guest RAM (the UFFD fault
        // storm). Fresh creates seed the same way now (ADR 0045
        // seed-at-create) — see `restore_fresh` / `restore_base_for_session`.
        //
        // ADR 0045 C1: a migration restore pulls the frozen source's
        // export FIRST (state.bin + sidecar + chunks into the local
        // cache, inline manifests staged), restores from those local
        // artifacts, then seeds the chain from the inline v+1 content
        // and spawns the durability catch-up (awaited by the
        // coordinator via the existing `snapshot_wait`).
        let mut metadata = metadata;
        let migration = metadata.migration_source.take();
        if let Some(mig) = migration.as_ref().filter(|m| m.post_copy) {
            // ADR 0045 C2: the post-copy destination. Stage the
            // presetup's package (no export exists yet — the source
            // hasn't paused), arm the role fence, and spawn the
            // fetch poller that lands state.bin + the drained disk
            // manifest once the capture seals. The FC load gate
            // (restore_in_jail) waits on those files; the handler
            // parks at the page server for the SEAL.
            self.postcopy_stage(&metadata, mig).await?;
        } else if let Some(mig) = &migration {
            self.migration_prestage(&metadata, mig).await?;
            // Land the FULL session-manifest divergence BEFORE the
            // guest resumes — synchronously, over N parallel streams
            // from the SOURCE host. The background version lost the
            // race every time: the guest resumes the instant restore
            // returns, and its wake-up working set then faults at GCS
            // round-trip speed through the single-threaded fault loop
            // (~150 ms x ~200 chunks = the 25-45 s handshake band the
            // prod canaries kept hitting; forensics on 9f2c3ef9 show
            // the fault timeline directly). Paying ~3-6 s here at LAN
            // line rate deletes that tail: every post-resume fault
            // hits local NVMe.
            if let Some(cache) = self.chunk_cache.clone() {
                if let Ok(m) = serde_json::from_slice::<engram_chunk_store::Manifest>(
                    &mig.memory_manifest_json,
                ) {
                    let staged: std::collections::HashSet<_> = mig
                        .new_memory_chunk_hashes
                        .iter()
                        .map(|h| engram_chunk_store::manifest::ChunkHash::from_bytes(*h))
                        .collect();
                    let remaining: Vec<_> = m
                        .chunks
                        .iter()
                        .map(|c| c.hash)
                        .filter(|h| !staged.contains(h) && !cache.contains_on_disk(*h))
                        .collect();
                    // Hot set first: the guest's wake-up working set
                    // lands on NVMe before the long tail (E2B fold).
                    let remaining = Self::order_hot_first(remaining, &mig.hot_chunks);
                    if !remaining.is_empty() {
                        let n = remaining.len();
                        let t = crate::time_source::metrics_now();
                        match Self::pull_chunks_from_source(
                            &mig.source_addr,
                            &mig.export_id,
                            &remaining,
                            &cache,
                        )
                        .await
                        {
                            Ok(pulled) => tracing::info!(
                                pulled,
                                of = n,
                                elapsed_ms = t.elapsed().as_millis() as u64,
                                "migration divergence pulled from source (ADR 0045 C1)"
                            ),
                            Err(e) => tracing::warn!(
                                error = %e,
                                of = n,
                                "source divergence pull failed; faults serve via GCS"
                            ),
                        }
                    }
                }
            }
        } else if !metadata.peer_hints.is_empty() {
            // ADR 0095: peer-hinted ordinary resume — the coordinator
            // says a live sibling (the snapshot host) holds this
            // session's chunks on NVMe. Land the locally-missing set
            // BEFORE the guest resumes, synchronously, for exactly the
            // reason the C1 migration arm above does: the background
            // version loses the race and the wake-up working set then
            // faults at GCS round-trip speed. On the affinity host the
            // missing set is empty (everything resident) and this arm
            // is a stat walk; on a dead/saturated peer the pull
            // degrades per the bounded-dial contract and the remainder
            // faults via GCS — today's path, unchanged.
            self.peer_resume_prepass(&metadata).await;
        }
        let memory_ref = metadata.memory_manifest;
        let row_template = migration.as_ref().map(|_| metadata.clone());
        // Resume keeps the snapshot's pinned mounts — no per-session selection.
        let id = self
            .restore_with(
                metadata,
                /*fresh=*/ false,
                /*fork_disk_at_attach=*/ false,
                Vec::new(),
            )
            .await?;
        match migration {
            Some(mig) if mig.post_copy => {
                // ADR 0045 C2: NO chain seed — sealed pages installed
                // from the peer diverge from the chain content, so a
                // diff against it would be unsound; with no chain the
                // dest's first checkpoint is a safe FULL (that Full IS
                // the memory durability catch-up, driven by the
                // coordinator's finalize after the drain). Disk
                // durability + the publish gate ride the fetch poller.
                let _ = mig;
                self.set_migration_role(id, Some(crate::migration::MigrationRole::PostCopyDest))
                    .await;
            }
            Some(mig) => {
                self.migration_finish_restore(id, mig, row_template.expect("set above"))
                    .await?;
            }
            None => {
                if let Some(memory_ref) = memory_ref {
                    self.seed_checkpoint_chain_sparse(id, memory_ref).await;
                }
            }
        }
        self.spawn_prefault_stats_probe(id);
        Ok(id)
    }

    async fn restore_fresh(
        &self,
        metadata: SnapshotMetadata,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        // ADR 0045 seed-at-create: guest RAM right after a fresh restore
        // is byte-identical to the base snapshot's memory manifest, and
        // FC dirty-page tracking runs from the restore — exactly the
        // resume-seeding argument. Seeding here makes the session's
        // FIRST capture a diff (pages it actually dirtied) instead of a
        // Full dump+re-chunk of all guest RAM. FORKED seed: the source
        // manifest is shared across sessions, so the chain must own a
        // fresh lineage (see seed_checkpoint_chain_forked).
        let memory_ref = metadata.memory_manifest;
        let id = self
            .restore_with(
                metadata,
                /*fresh=*/ true,
                /*fork_disk_at_attach=*/ true,
                selected_mounts,
            )
            .await?;
        if let Some(memory_ref) = memory_ref {
            self.seed_checkpoint_chain_forked(id, memory_ref).await;
        }
        Ok(id)
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // Unregister from the local egress proxy first, so any
        // outstanding traffic from a still-alive guest stops being
        // rewritten. The destroy below tears down the VM; in the
        // brief window between the two, a closed-fail-by-default
        // registry would reject — which is the safe behavior.
        //
        // ADR 0016 Phase B 4: the `session_bindings.remove` MUST run
        // regardless of whether an egress proxy is wired — Phase B's
        // LiveManifestPublisher resolves sandbox→session via this
        // map, and leaking a (destroyed) sandbox_id binding would
        // make the publisher repeat stale publishes for a session
        // whose sandbox is gone. Sister-bug to the
        // `notify_session_policy` fix (commit 5163366): both
        // population AND cleanup must be unconditional now that the
        // map is shared with the publisher.
        let removed_session = self.session_bindings.remove(&id).map(|(_, sid)| sid);
        // ADR 0090: a destroyed survivor stops advertising quarantine —
        // the evict_local remediation (or any destroy) closes the loop.
        self.quarantined_survivors.remove(&id);
        self.unreachable_guests.remove(&id);
        if let Some(egress) = self.egress.as_ref() {
            if let Some(session_id) = removed_session {
                egress.registry.unregister(session_id);
            }
        }
        let result = self.inner.destroy(id).await;
        // Tear down the NBD daemon AFTER the inner backend has
        // closed the VM (so the kernel doesn't surface "device
        // busy" on disconnect). Drop on NbdSandboxState handles
        // disconnect → join → slot release. Done last so even
        // if inner.destroy errors, the daemon cleanup still runs.
        #[cfg(target_os = "linux")]
        {
            let _ = self.nbd_sandboxes.remove(&id);
            // A destroyed sandbox's shutdown spool must not outlive it
            // (the sandbox_id will never rehydrate again; a leftover
            // spool is dead weight on the hostPath volume).
            if let Some(root) = self.shutdown_spool_root() {
                let _ = crate::disk_daemon::spool::discard_spool(self.host_fs.as_ref(), &root, id)
                    .await;
            }
        }
        // ADR 0016 Phase A: drop the COW diagnostic timestamp so
        // the entry doesn't outlive its sandbox. A subsequent
        // `cow_state(id)` returns `None` (no NBD entry, no
        // snapshot timestamp) — same shape as a brand-new
        // sandbox.
        let _ = self.last_snapshot_unix_ms.remove(&id);
        let _ = self.checkpoint_pacing.remove(&id);
        // ADR 0028 Fix A: tear down the checkpoint chain. Durable RECORDS
        // deliberately survive destroy — an eviction's final checkpoint
        // must stay re-advertisable until the coord acks it (that's the
        // whole reconciliation point). GCS chunks are the durable truth;
        // ADR 0039: the chain is manifest-only now (no local rolling
        // image), so there's nothing on disk to remove here.
        //
        // Issue #529: the same invariant covers `eviction_finalize.rs`'s
        // `EvictionFinalizeRecord`/`CheckpointRecord{kind:EvictionFinal}`
        // (`<checkpoint_dir>/finalize/`, `.../records/`) and the
        // `pending_finalizes` idempotency map — none of the three are
        // touched here. In the normal flow that's moot: `run_terminal`
        // deletes the `EvictionFinalizeRecord` and clears
        // `pending_finalizes` itself, strictly BEFORE calling this
        // `destroy()` (see the ordering in `run_terminal`). Were `destroy`
        // ever invoked directly on a sandbox with a still-in-flight
        // finalize (outside that job's own terminal step — not a path any
        // caller in this repo takes today), the finalize job would keep
        // running unaffected: it is a pure function of `dest` + the chunk
        // store, never the live sandbox, and this method deletes neither.
        let _ = self.checkpoint_chains.remove(&id);
        if let Some(store) = &self.chain_heads {
            store.remove_best_effort(id);
        }
        let _ = self.capture_locks.remove(&id);
        // Issue #221: reclaim any unconsumed `snapshot_wait` slot. The
        // entry is now kept-until-consumed (so a cancelled coordinator
        // wait can retry), which means the coordinator's give-up path —
        // `abort_inflight_snapshot` + `destroy` — must clean it up here
        // or it leaks for the lifetime of the host. Abort the backing
        // upload task too (the sandbox is gone; the artifacts, if any,
        // are covered by the durable checkpoint record).
        if let Some((_, wait)) = self.snapshot_waits.remove(&id) {
            wait.abort.abort();
        }
        result
    }

    async fn start_agent(&self, id: SandboxId, mut agent: AgentSpec) -> Result<(), SandboxError> {
        // ADR 0021 P1.2: only the host-agent knows the per-host egress-
        // proxy CA, so it stamps the PEM onto the AgentSpec right
        // before the backend sees it. Each backend rides it into the
        // guest on the same `SpawnHarness` frame that spawns the
        // harness (2026-07 core-ops fold — one first-contact RPC
        // installs the CA and spawns, instead of a separate round
        // trip). Coord-supplied specs always arrive with
        // `host_ca_pem = None`; the host-agent fills it in here.
        if agent.host_ca_pem.is_none() {
            agent.host_ca_pem = self.egress.as_ref().map(|e| e.ca_cert_pem.clone());
        }
        let result = self.inner.start_agent(id, agent).await;
        // ADR 0019: agent_ready has fired (or failed) — close the cold-boot
        // operation window so steady-state session I/O falls back to
        // metrics-only (no per-fetch spans for a running session).
        #[cfg(target_os = "linux")]
        if let Some(state) = self.nbd_sandboxes.get(&id) {
            state.backend.operation_scope().end();
        }
        result
    }

    /// ADR 0084 §B: cold-base / warm-overlay split.
    ///
    /// `req.cold_base_plan` (computed entirely coordinator-side —
    /// `ColdBasePlan`'s doc) drives the stage plan:
    ///
    /// - `NotApplicable` (non-FC claiming host, or an FC host with no
    ///   `fc_snapshot_version` reported): unchanged single-stage path —
    ///   cold-boot (or the caller never even reaches this since a
    ///   non-diffing backend can't have been claimed a `Hit`/`Miss`; see
    ///   the hard-error gate below), run the hook if any, ONE snapshot
    ///   call at the end. Warm-less images take this same single path
    ///   naturally (`warm.is_none()` just skips the hook step) — no
    ///   special-casing needed since there's only ever one snapshot call
    ///   in this arm.
    /// - `Miss { content_key }`: cold-boot, and — ONLY if this is a WARM
    ///   image (a warm-less image's single final snapshot IS the cold
    ///   base, see above) — take an extra Full snapshot BEFORE the hook
    ///   runs (chain auto-seeds off it, `self.snapshot`'s own
    ///   `advance_checkpoint_state` post-capture bookkeeping — no new
    ///   plumbing needed), then run the hook, then the final overlay
    ///   snapshot (now a cheap Diff, chain already seeded).
    /// - `Hit { content_key, snapshot }`: `self.restore(snapshot)`
    ///   instead of cold-booting (chain auto-seeds sparse off the
    ///   restored base's OWN memory manifest — the same machinery a
    ///   plain session resume uses), run the hook, take the final
    ///   overlay snapshot (a cheap Diff). No cold boot at all.
    ///
    /// Every arm ends with exactly one snapshot call whose metadata
    /// becomes [`engram_core::types::capture_job::CaptureJobResult::
    /// snapshot`] — the artifact the enabled image points at.
    #[tracing::instrument(name = "host.build_base_snapshot", skip_all)]
    async fn build_base_snapshot(
        &self,
        req: engram_core::traits::sandbox::BuildBaseSnapshotRequest,
        progress: tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    ) -> Result<engram_core::types::capture_job::CaptureJobResult, SandboxError> {
        use engram_core::types::capture_job::{CaptureJobResult, CapturedColdBase, ColdBasePlan};

        let engram_core::traits::sandbox::BuildBaseSnapshotRequest {
            spec,
            warm,
            capture_env,
            capture_egress,
            cold_base_plan,
        } = req;

        // ADR 0084 decision 11: a `Hit`/`Miss` plan means placement
        // pinned this host as FC-capable (`fc_snapshot_version` +
        // `supports_diff_checkpoints`). If the actual backend can't
        // diff, that's a placement bug — hard error, never a silent
        // single-stage fallback.
        if !matches!(cold_base_plan, ColdBasePlan::NotApplicable)
            && !self.inner.supports_diff_checkpoints()
        {
            return Err(SandboxError::CaptureFailed(
                engram_core::types::CaptureFailure {
                    kind: engram_core::types::CaptureFailureKind::ColdBaseCapabilityMismatch,
                    stage: Some("assigned".to_string()),
                    tail: String::new(),
                    message: "claim carried a cold-base plan but this backend does not support \
                          diff memory snapshots (supports_diff_checkpoints() == false); this \
                          is a placement bug, not a transient condition"
                        .to_string(),
                },
            ));
        }

        // ADR 0021 P1.5: no stub-harness attach — the harness lives
        // in the rootfs of the image being captured, so the snapshot
        // is already complete without any second virtio-blk drive.

        // The config `[env]` (JAVA_HOME, PATH, …) the `[warm]` hook needs,
        // MERGED with the resolved capture-time env (`capture_env` — literals
        // plus the coordinator's SecretStore-resolved refs). capture_env wins
        // on a key collision: it's the operator's explicit capture override.
        // Captured before `spec` is moved into `create`; passed to the warm
        // hook below as its exec env so it runs with the image's environment
        // plus whatever secrets the warm boot needs.
        let mut session_env = spec.env.clone();
        session_env.extend(capture_env);
        let is_warm = warm.is_some();
        // Captured before `spec` moves into `create` — the balloon
        // reclaim target below derives from guest RAM size.
        let guest_mem_mib = u64::from(spec.memory.max_mib);

        // ---- stage 1: a LIVE VM at (or converging toward) agentd-ready
        // — restore the cold base on a Hit (no cold boot at all), else
        // cold-boot fresh (Miss / NotApplicable). ----
        //
        // ADR 0084 P1b: the teardown reconcile no longer exempts capture
        // VMs via a `base_captures` DashMap tracked here — the host-
        // agent's capture-job executor (`capture_job.rs`) tracks
        // `(job_id, sandbox_id)` for the whole job lifetime and the
        // reconcile consults IT instead (`CaptureJobExecutor::
        // is_live_sandbox`), so the exemption survives exactly as long
        // as a live job record says it should, not as long as this one
        // call happens to run.
        let id = match &cold_base_plan {
            ColdBasePlan::Hit { snapshot, .. } => {
                let memory_ref = snapshot.memory_manifest;
                // The cold base is SHARED across every capture that hits
                // it, so BOTH lineages must fork a private chain (same
                // rule as session-create off a shared base template):
                //
                // - memory: seed FORKED, not sparse — the first capture's
                //   overlay already published `base_id@v2`, so a second
                //   hit's diff would collide ("version conflict: attempted
                //   v2, latest is v2" — the exact FC-lane CI failure the
                //   forked seed fixed).
                // - disk: `fork_disk_at_attach=true` — the warm hook's
                //   writes flush through the NBD chain, and an unforked
                //   attach would tick (and race) the cold base's shared
                //   disk manifest chain exactly the same way.
                //
                // `fresh=false`: this is a resume of the cold base's
                // pinned device model, not a session fresh-create — no
                // per-session mount swap, resume memory semantics.
                let id = self
                    .restore_with(
                        (**snapshot).clone(),
                        /*fresh=*/ false,
                        /*fork_disk_at_attach=*/ true,
                        Vec::new(),
                    )
                    .await?;
                if let Some(memory_ref) = memory_ref {
                    self.seed_checkpoint_chain_forked(id, memory_ref).await;
                }
                // ADR 0088 addendum: a balloon-era cold base was dumped
                // with the balloon INFLATED — the deflate happens inside
                // the teardown-covered capture block below (so a deflate
                // failure destroys the restored VM instead of leaking it
                // to the reconcile).
                self.spawn_prefault_stats_probe(id);
                id
            }
            ColdBasePlan::Miss { .. } | ColdBasePlan::NotApplicable => self.create(spec).await?,
        };

        // Capture-time egress (ADR 0080): register the coordinator-assembled
        // policy for the capture VM's guest IP so the `[warm]` hook can reach
        // the network (e.g. OIDC discovery) — without it the proxy denies the
        // capture VM as an unknown guest. No-op when the coordinator granted
        // no egress. Torn down after the destroy below, on every path.
        let capture_egress = self.register_capture_egress(id, capture_egress).await;

        // Issue #539: `phase=boot` — the capture VM exists and is
        // booting (or, on a Hit, already live from the restore) toward
        // agentd-ready. Best-effort; a slow/dropped consumer must not
        // stall the capture.
        let boot_event = engram_core::types::CaptureProgress {
            phase: engram_core::types::CapturePhase::Boot,
            sandbox_id: Some(id),
            warm_stage: None,
            detail: None,
            output_tail: String::new(),
            warm_stages: Vec::new(),
        };
        let _ = progress.try_send(boot_event.clone());

        // Drive the capture to a snapshot, then ALWAYS tear the VM down —
        // a capture VM has no session and must not linger.
        let captured: Result<CaptureJobResult, SandboxError> = async {
            // Finding 1: a slow cold boot / chunk materialize has no
            // progress source of its own to renew the capture-claim lease
            // — resend the boot frame every 30s until agentd-ready (or we
            // bail). Dropped (stopping the ticker) the instant this leg
            // ends, on every path, including the early `?`-returns below.
            let boot_keepalive = spawn_leg_keepalive(progress.clone(), boot_event);
            // Wait for the guest to reach agentd-ready (bootstrap on
            // accept(), harness unmounted — the option-D capture point).
            // On a Hit restore the watch is already pre-set true (FC's
            // restore path); on VZ this returns InvalidSpec (no
            // readiness concept there).
            match self.inner.wait_agent_ready(id).await {
                Ok(()) => {}
                Err(SandboxError::InvalidSpec(_)) => {}
                Err(e) => return Err(e),
            }
            drop(boot_keepalive);

            // ADR 0088 addendum (adversarial-review fix): a balloon-era
            // cold base was dumped with the balloon INFLATED, so a Hit
            // restore comes up ballooned — deflate AND CONFIRM
            // (`balloon_release`'s contract) before anything runs in the
            // guest. Only the TYPED no-device outcome (`InvalidSpec` — a
            // legacy balloon-less base, or a non-FC backend) is a no-op;
            // any other failure fails the capture (inside this block, so
            // the teardown below destroys the VM rather than leaking it).
            if matches!(cold_base_plan, ColdBasePlan::Hit { .. }) {
                match self.inner.balloon_release(id).await {
                    Ok(()) => {}
                    Err(SandboxError::InvalidSpec(msg)) => {
                        tracing::debug!(
                            sandbox_id = %id,
                            detail = %msg,
                            "cold-base restore: no balloon device to deflate (legacy base)",
                        );
                    }
                    Err(e) => {
                        return Err(SandboxError::CaptureFailed(
                            engram_core::types::CaptureFailure {
                                kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                                stage: Some("booting".to_string()),
                                tail: String::new(),
                                message: format!(
                                    "balloon deflate after cold-base restore failed: {e} — \
                                     refusing to run the warm hook in a possibly-starved guest"
                                ),
                            },
                        ));
                    }
                }
            }

            // ADR 0084 §B3: a WARM image on a MISS needs its OWN cold
            // base minted before the hook runs (the hook must land on
            // top of an established base, not fold into the artifact's
            // only capture) — take a Full snapshot now. `self.snapshot`
            // takes Full here because no checkpoint chain exists yet for
            // this freshly-created sandbox; its own post-capture
            // bookkeeping (`advance_checkpoint_state`) auto-seeds the
            // chain off the Full's memory manifest, so the LATER overlay
            // snapshot below is automatically a cheap Diff — no new
            // pause/resume/seed plumbing needed here.
            //
            // Every other combination skips this: `NotApplicable` (no
            // cold-base concept), `Hit` (already have one — the restore
            // above seeded the chain sparse off IT), and warm-less
            // (its single final snapshot below IS the cold base).
            // ADR 0088 addendum: only the DUMP blocks the hook now. The
            // seed's finish() (the 40s–11.5min chunk+upload leg measured
            // in prod) runs on a spawned task concurrent with the warm
            // hook and is joined — success AND failure paths — before
            // the final snapshot below (join-before-final keeps the
            // final capture a Diff and keeps the durability barrier
            // ahead of the CaptureJobResult, exactly as the inline
            // shape did).
            let deferred_cold_base = if is_warm
                && matches!(cold_base_plan, ColdBasePlan::Miss { .. })
            {
                #[cfg(target_os = "linux")]
                if let Some(state) = self.nbd_sandboxes.get(&id) {
                    state.backend.operation_scope().end();
                }
                let cold_base_event = engram_core::types::CaptureProgress {
                    phase: engram_core::types::CapturePhase::Snapshot,
                    sandbox_id: Some(id),
                    warm_stage: None,
                    detail: Some("cold-base memory dump".to_string()),
                    output_tail: String::new(),
                    warm_stages: Vec::new(),
                };
                let _ = progress.try_send(cold_base_event.clone());
                let cold_base_keepalive = spawn_leg_keepalive(progress.clone(), cold_base_event);

                // ADR 0088 addendum: shrink the seed. Inflating the
                // balloon hands the guest's free pages back to the host
                // (`MADV_DONTNEED`), so the dense dump reads zeros there
                // and the all-zero 512 KiB elision drops them from the
                // memory manifest (~guest-RAM → ~touched-pages).
                // Fail-open is limited to the TYPED no-balloon outcome
                // (kill switch, legacy kernel, VZ) — any other reclaim
                // failure means the inflate target may have landed, and
                // `balloon_inflate_for_seed` normalizes (release-and-
                // confirm) or fails the capture rather than ever running
                // the warm hook in a possibly-starved guest
                // (adversarial-review fix). The reserve keeps the
                // paused-adjacent guest comfortably functional while
                // inflated.
                const BALLOON_RESERVE_MIB: u64 = 1536;
                let balloon_target = guest_mem_mib.saturating_sub(BALLOON_RESERVE_MIB);
                let inflated = balloon_inflate_for_seed(
                    self.inner.as_ref(),
                    id,
                    balloon_target,
                    std::time::Duration::from_secs(30),
                )
                .await
                .map_err(|e| {
                    SandboxError::CaptureFailed(engram_core::types::CaptureFailure {
                        kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                        stage: Some("booting".to_string()),
                        tail: String::new(),
                        message: format!(
                            "balloon state could not be normalized before the cold-base dump: \
                             {e} — refusing to run the warm hook in a possibly-starved guest"
                        ),
                    })
                })?;

                let deferred = self.snapshot_deferred(id).await.map_err(|e| {
                    SandboxError::CaptureFailed(engram_core::types::CaptureFailure {
                        kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                        stage: Some("booting".to_string()),
                        tail: String::new(),
                        message: format!("cold-base Full capture failed: {e}"),
                    })
                });
                drop(cold_base_keepalive);
                let deferred = deferred?;

                // FAIL-LOUD deflate: a warm hook in a balloon-starved
                // guest (~1.5 GiB effective) is a guaranteed slow OOM-
                // flavored failure 20 minutes later — better to fail in
                // seconds here. `balloon_release` CONFIRMS actual==0
                // before returning; `InvalidSpec` here is ALSO fatal
                // (the device demonstrably existed at inflate time).
                // Only reached when the inflate landed.
                if inflated {
                    if let Err(e) = self.inner.balloon_release(id).await {
                        // The deferred finish must still be settled
                        // (await-never-abort) before surfacing.
                        let seed = deferred.join().await;
                        tracing::warn!(sandbox_id = %id, seed_ok = seed.is_ok(), "balloon deflate failed; seed settled before aborting");
                        return Err(SandboxError::CaptureFailed(
                            engram_core::types::CaptureFailure {
                                kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                                stage: Some("booting".to_string()),
                                tail: String::new(),
                                message: format!(
                                    "balloon deflate after the cold-base dump failed: {e} — \
                                     refusing to run the warm hook in a memory-starved guest"
                                ),
                            },
                        ));
                    }
                }
                Some(deferred)
            } else {
                None
            };

            // Capture-time prewarm hook (image `[warm]`): run the warm
            // command in the live VM BEFORE the (overlay) snapshot, so a
            // process it leaves running (e.g. a `gradle --daemon`) is
            // frozen into the snapshot and every restored session
            // inherits it warm. The command must start its daemon
            // detached and exit; we run it to completion and gate the
            // capture on a clean exit.
            //
            // FAIL-LOUD: a non-zero exit or timeout aborts the capture
            // (and thus the enable) — we never ship a "cold" base snapshot
            // that a `[warm]` hook claimed to warm. The hook runs with the
            // manifest `[env]` MERGED with the resolved `capture_env` (the
            // capture VM's agentd has no durable session env, so both ride
            // the exec). capture_env carries the dev secrets a warm boot
            // needs (e.g. an `op` token, resolved coordinator-side); still
            // NO per-session secrets — those are session policy, injected
            // post-restore, not at capture.
            //
            // `warm_tail` survives a successful hook so a LATER
            // snapshot-phase failure can still report the hook's last
            // output — the diagnosis a `status None` / vsock-lost failure
            // used to lose entirely.
            let mut warm_tail = crate::warm_progress::OutputTail::default();
            // The hook result is NOT `?`-returned before the seed join
            // below — the deferred finish must always be awaited (never
            // dropped/aborted; see `snapshot_deferred`'s contract).
            let hook_result = match &warm {
                Some(warm) => self
                    .run_warm_hook(id, warm, &session_env, &progress)
                    .await
                    .map(Some),
                None => Ok(None),
            };

            // ---- JOIN BARRIER (ADR 0088 addendum) ----
            // Settle the deferred cold-base seed on success AND failure
            // paths. Under its own keepalive: the upload may still have
            // minutes left when a short hook finishes, and the claim
            // lease must not expire while we wait it out.
            let seed_result: Option<Result<SnapshotMetadata, SandboxError>> =
                match deferred_cold_base {
                    Some(deferred) => {
                        let upload_event = engram_core::types::CaptureProgress {
                            phase: engram_core::types::CapturePhase::Snapshot,
                            sandbox_id: Some(id),
                            warm_stage: None,
                            detail: Some("cold-base upload".to_string()),
                            output_tail: String::new(),
                            warm_stages: Vec::new(),
                        };
                        let _ = progress.try_send(upload_event.clone());
                        let upload_keepalive =
                            spawn_leg_keepalive(progress.clone(), upload_event);
                        let joined = deferred.join().await;
                        drop(upload_keepalive);
                        Some(joined)
                    }
                    None => None,
                };

            // Error priority: the hook's failure is the actionable one
            // (it aborts today too); a concurrent seed failure is logged
            // alongside rather than masking it.
            match hook_result {
                Ok(Some(tail)) => warm_tail = tail,
                Ok(None) => {}
                Err(e) => {
                    if let Some(Err(seed_err)) = &seed_result {
                        tracing::warn!(
                            sandbox_id = %id,
                            error = %seed_err,
                            "cold-base seed upload also failed while the warm hook was failing",
                        );
                    }
                    return Err(e);
                }
            }
            let minted_cold_base = match seed_result {
                None => None,
                Some(Ok(meta)) => Some(meta),
                Some(Err(e)) => {
                    return Err(SandboxError::CaptureFailed(
                        engram_core::types::CaptureFailure {
                            kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                            stage: Some("booting".to_string()),
                            tail: warm_tail.render(),
                            message: format!("cold-base capture failed: {e}"),
                        },
                    ))
                }
            };

            if warm.is_some() {
                // Incident 2026-07-10 mitigation: flush the guest's dirty
                // page cache to the (durably captured) disk BEFORE the
                // final snapshot. The memory image is REQUIRED to carry
                // dirty page cache faithfully — the fidelity gate
                // (`unsynced_warm_writes_survive_uffd_restore_and_cache_drop`)
                // asserts it does — but a base snapshot fans out to every
                // session of an image, so its DISK must not depend on
                // that: with the sync, the captured disk stands alone
                // (fsck-able, drop_caches-recoverable) even if a
                // memory-fidelity regression slips through. Warm captures
                // only: the hook is the sole producer of meaningful
                // unsynced state at capture (docker layers, PG pages, git
                // metadata — a warm-less capture's dirty window is bare
                // boot state). Fail-loud like the hook itself: a guest
                // that cannot sync is a guest we must not ship as a base.
                self.sync_guest_fs(id).await?;
            }
            // Close the cold-boot window (mirrors `start_agent`) before the
            // snapshot flush opens its own `snapshot` operation scope. A
            // no-op if the Miss+warm arm above already ended it (or if
            // this sandbox was restored, not cold-booted — no scope to
            // end either way).
            #[cfg(target_os = "linux")]
            if let Some(state) = self.nbd_sandboxes.get(&id) {
                state.backend.operation_scope().end();
            }
            // `phase=snapshot` — the warm hook (if any) is done; pause/flush/
            // chunk is next. Carries the warm tail forward so a live watcher
            // still sees it during the (usually short) snapshot phase.
            let snapshot_event = engram_core::types::CaptureProgress {
                phase: engram_core::types::CapturePhase::Snapshot,
                sandbox_id: Some(id),
                warm_stage: None,
                detail: None,
                output_tail: warm_tail.render(),
                warm_stages: Vec::new(),
            };
            let _ = progress.try_send(snapshot_event.clone());
            // Finding 1: pause/flush/chunk memory + upload a multi-GB
            // state blob to BlobStorage has historically run many minutes
            // on dev-brain with zero progress traffic once the warm hook
            // (if any) is done — same lease-staleness risk as the boot
            // leg above.
            let snapshot_keepalive = spawn_leg_keepalive(progress.clone(), snapshot_event);
            // Capture: pause → flush disk → chunk memory + upload
            // state.bin/sidecar to BlobStorage. This is the portable
            // artifact `create_session` restores from — a Diff overlay
            // when a chain was seeded above (warm Miss's pre-hook Full,
            // or the Hit restore's sparse seed), a Full when neither ran
            // (warm-less, or `NotApplicable`).
            let final_meta = self.snapshot(id).await.map_err(|e| {
                SandboxError::CaptureFailed(engram_core::types::CaptureFailure {
                    kind: engram_core::types::CaptureFailureKind::SnapshotFailed,
                    stage: None,
                    tail: warm_tail.render(),
                    message: e.to_string(),
                })
            });
            drop(snapshot_keepalive);
            let final_meta = final_meta?;

            let cold_base = match &cold_base_plan {
                ColdBasePlan::NotApplicable => None,
                ColdBasePlan::Hit {
                    content_key,
                    snapshot,
                } => Some(CapturedColdBase {
                    content_key: content_key.clone(),
                    // The (unchanged) existing row's own identity — NOT
                    // `final_meta` (the fresh overlay this attempt just
                    // took). `finalize_capture_job` must not re-upsert
                    // `cold_bases` for a Hit; this field is reported for
                    // symmetry/debuggability, not consumed as a write.
                    snapshot: (**snapshot).clone(),
                    freshly_captured: false,
                    miss_reason: None,
                }),
                ColdBasePlan::Miss {
                    content_key,
                    reason,
                } => Some(CapturedColdBase {
                    content_key: content_key.clone(),
                    // Warm Miss: the pre-hook Full minted above. Warm-less
                    // Miss: no pre-hook capture ran — `final_meta` IS the
                    // cold base (the single Full path, ADR §B3).
                    snapshot: minted_cold_base.unwrap_or_else(|| final_meta.clone()),
                    freshly_captured: true,
                    miss_reason: Some(*reason),
                }),
            };

            Ok(CaptureJobResult {
                snapshot: final_meta,
                cold_base,
            })
        }
        .await;

        // Best-effort teardown: a destroy failure must not mask a
        // successful capture (the metadata is already durable in
        // BlobStorage); the host reconcile pass GCs any leak.
        if let Err(e) = self.destroy(id).await {
            tracing::warn!(
                sandbox_id = %id,
                error = %e,
                "base-snapshot capture: destroy of capture VM failed; reconcile will GC",
            );
        }
        // Drop the capture-egress allowlist now that the VM is gone (no-op if
        // none was registered).
        self.unregister_capture_egress(capture_egress);
        captured
    }

    /// ADR 0080 §C: the `MaterializeImage` engine — validation, the
    /// ≤1-concurrent gate, and the pipeline all live in
    /// [`crate::materialize::run`]; this method owns only the pieces
    /// that are per-host state (the scratch root, the chunk store, the
    /// ambient OCI client, the gate).
    #[tracing::instrument(name = "host.materialize_image", skip_all, fields(image_uri))]
    async fn materialize_image(
        &self,
        image_uri: &str,
        platform_os: &str,
        platform_arch: &str,
        registry_auth: Option<engram_core::types::registry::ResolvedRegistryAuth>,
        min_disk_gib: u32,
        progress: tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>,
    ) -> Result<engram_core::types::MaterializedImage, SandboxError> {
        let Some(scratch) = self.materialize_scratch.clone() else {
            return Err(SandboxError::InvalidSpec(
                "this host has no materialize scratch dir wired (with_materialize_scratch)".into(),
            ));
        };
        let Some(chunk_store) = self.chunk_store.clone() else {
            return Err(SandboxError::InvalidSpec(
                "this host has no chunk store wired; cannot materialize images".into(),
            ));
        };
        // ≤1 concurrent materialize per host: an image pull + flatten +
        // pack saturates NVMe/network; queueing a second behind it
        // just serializes with extra memory pressure. `try_lock` (not
        // `lock`) so the second caller gets the retryable `busy` and
        // the coordinator re-picks a host.
        let gate = self.materialize_gate.clone();
        let Ok(_permit) = gate.try_lock() else {
            return Err(SandboxError::MaterializeFailed(
                engram_core::types::MaterializeFailure {
                    kind: engram_core::types::MaterializeFailureKind::Busy,
                    message: "a materialize is already running on this host (≤1 concurrent); \
                              retry re-picks a host"
                        .into(),
                },
            ));
        };
        crate::materialize::run(
            self.oci_client.clone(),
            &chunk_store,
            &scratch,
            image_uri,
            platform_os,
            platform_arch,
            registry_auth,
            min_disk_gib,
            progress,
        )
        .await
    }

    #[tracing::instrument(name = "host.restore_base_for_session", skip_all)]
    async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        // 1. Restore the base snapshot (cross-host materialize +
        //    load_snapshot). The VM comes up running with the bake-time
        //    harness baked into the rootfs at /opt/engram/harness/.
        //    Fresh flavor (ADR 0035 §3): aux bundles swap to the host's
        //    current generation so new sessions run the latest skills.
        let memory_ref = metadata.memory_manifest;
        let id = self
            .restore_with(
                metadata,
                /*fresh=*/ true,
                /*fork_disk_at_attach=*/ true,
                selected_mounts,
            )
            .await?;
        // ADR 0045 seed-at-create: the session's RAM == the base
        // manifest at this instant (see `restore_fresh`); seed the
        // chain so the first eviction diffs instead of Full-dumping.
        // FORKED: the base manifest is shared across every session of
        // the image — each chain must own its own lineage. The env
        // merge below dirties pages AFTER tracking started, so the
        // diff stays correct.
        if let Some(memory_ref) = memory_ref {
            self.seed_checkpoint_chain_forked(id, memory_ref).await;
        }

        // ADR 0080: let the captured agentd adopt the agentd bundle
        // generation the fresh flavor just patched into its slot —
        // BEFORE any session state (env, harness, policy) binds, so a
        // re-exec loses nothing. Non-fatal by design: a pre-ADR-0080
        // snapshot (agentd baked into the rootfs, no bundle slot), a
        // bundle staging hiccup, or a typed skew error all degrade to
        // the captured agentd — one generation stale beats a failed
        // create.
        match self.inner.refresh_agent(id).await {
            Ok(engram_core::traits::AgentRefresh::UpToDate) => {}
            Ok(engram_core::traits::AgentRefresh::Restarted) => {
                tracing::info!(
                    sandbox_id = %id,
                    "agentd refreshed onto the host's current bundle generation",
                );
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %id,
                    error = %e,
                    "agentd refresh degraded; session keeps the captured agentd",
                );
            }
        }

        // Inject the per-session env (manifest env + secrets + session
        // id). The base snapshot is shared, so per-session values can't
        // be baked into it — in cold-create they rode vm_spec.env.
        if !session_env.is_empty() {
            self.inner.merge_session_env(id, session_env).await?;
        }

        // ADR 0021 P1.5: option-D substrate swap retired. The harness
        // travels in the rootfs (manifest `[harness] exec`); there's
        // no per-session harness file to materialize or swap.
        Ok(id)
    }

    // ADR 0021 P1.5: `swap_harness_drive` wrapper retired with the
    // trait method.

    fn set_harness_sink(&self, sink: engram_core::traits::HarnessSink) {
        self.inner.set_harness_sink(sink);
    }

    fn set_forge_sink(&self, sink: engram_core::traits::ForgeSink) {
        self.inner.set_forge_sink(sink);
    }

    fn set_upload_sink(&self, sink: engram_core::traits::UploadSink) {
        self.inner.set_upload_sink(sink);
    }

    async fn notify_session_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        let sandbox_id = policy.sandbox_id;
        let session_id = policy.session_id;

        // ADR 0016 Phase B commit 4: the host-side
        // LiveManifestPublisher's SessionResolver reads
        // `session_bindings` to map sandbox_id → session_id at
        // publish time. Phase A's design tied population to the
        // egress-proxy branch, which means hosts without a wired
        // proxy (the integration-up.sh dev stack, prod hosts with
        // egress disabled) silently never populate the map — every
        // FlushScheduler publish then skips with "sandbox not
        // bound to a session" even though the session IS bound.
        //
        // Insert FIRST, unconditionally. The egress-proxy registration
        // below is still gated on `self.egress`; only the sandbox→
        // session bookkeeping is universal.
        self.session_bindings.insert(sandbox_id, session_id);

        let Some(egress) = self.egress.as_ref() else {
            // No proxy attached — egress is unfiltered. The
            // coordinator may still send policy frames (the
            // coordinator-side codepath doesn't know whether a host
            // happens to have a proxy); skip the egress-proxy
            // registration but the sandbox→session map insert above
            // still ran, so Phase B's publisher resolver sees the
            // binding.
            tracing::debug!(
                %sandbox_id,
                %session_id,
                "notify_session_policy: no local egress proxy, binding recorded for Phase B publisher only",
            );
            return Ok(());
        };
        crate::egress::register_policy(&egress.registry, policy)
            .map_err(|e| SandboxError::InvalidSpec(format!("translate egress policy: {e}")))?;
        tracing::debug!(
            %sandbox_id,
            %session_id,
            "egress policy registered with local proxy",
        );
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.inner.list().await
    }

    /// ADR 0068: proxy to the wrapped backend, same as every other
    /// capability method here — `FirecrackerBackend` overrides the
    /// trait default with the ground-truth manifest check; VZ/Process
    /// inherit the default (list-membership mirror). The pool itself
    /// tracks no independent liveness signal worth adding here.
    async fn probe_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        self.inner.probe_sandbox(id).await
    }

    async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
        self.inner.guest_endpoints(id).await
    }

    /// ADR 0014 follow-up: forward to inner. Without this, the
    /// SandboxBackend trait's default impl (return Ok(7681)) would
    /// run instead and the FC backend's actual vsock StartShell RPC
    /// to in-VM agentd would never fire — host's proxy_shell would
    /// then dial port 7681 blind and hit Connection refused whenever
    /// the bake's init didn't auto-start ttyd or the warm-restore
    /// stripped the listening socket. Prod 2026-05-20 session
    /// 5c8d0ce5: zero start_shell logs on the host-agent despite a
    /// completed proxy_shell GRPC roundtrip, exactly because this
    /// forward was missing.
    async fn start_shell(&self, id: SandboxId) -> Result<u16, SandboxError> {
        self.inner.start_shell(id).await
    }

    /// ADR 0080: forward to inner, exactly as `start_shell` does — without
    /// this the trait default (`Ok(UpToDate)`) would run and the FC
    /// backend's actual vsock RefreshAgent RPC to in-VM agentd would never
    /// fire, silently pinning every session to its capture-time agentd.
    async fn refresh_agent(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::traits::AgentRefresh, SandboxError> {
        self.inner.refresh_agent(id).await
    }

    /// ADR 0065: forward to inner, exactly as `start_shell` does — without
    /// this the trait default (`Ok(5900)`) would run and the FC/VZ backend's
    /// actual vsock StartBrowser RPC to in-VM agentd would never fire, so
    /// the host's `proxy_vnc` would dial a port nothing started.
    async fn start_browser(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
        self.inner.start_browser(id).await
    }

    /// ADR 0065: forward to inner (the trait default is a no-op).
    async fn stop_browser(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.stop_browser(id).await
    }

    /// ADR 0085: forward to inner, exactly as `start_browser` does — without
    /// this the trait default (`Ok(13337)`) would run and the FC/VZ backend's
    /// actual vsock StartIde RPC to in-VM agentd would never fire, so the
    /// orchestrator would relay to a port nothing started.
    async fn start_ide(&self, id: SandboxId) -> Result<u16, SandboxError> {
        self.inner.start_ide(id).await
    }

    /// ADR 0085: forward to inner (the trait default is a no-op).
    async fn stop_ide(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.stop_ide(id).await
    }

    /// ADR 0016 Phase A: COW diagnostic. Reads from the NBD-backed
    /// disk state (`nbd_sandboxes`) + the per-host `chunk_cache`
    /// for base-chunk locality + the in-memory `last_snapshot_unix_ms`
    /// tracker for the memory-tier RPO. `None` when:
    ///
    /// - The sandbox isn't NBD-attached (Process backend, macOS dev,
    ///   Linux with `nbd_pool` unwired). The disk tier has no
    ///   chunk-granularity view; the diagnostic surface reports
    ///   "not chunk-tracked" upstream by absence.
    /// - The sandbox_id isn't known here. Coord-side
    ///   `host_for_sandbox` already gates on this; a stray query
    ///   inherits the same shape.
    ///
    /// Cost: one map lookup for nbd state, two lock acquisitions on
    /// the `ChunkedDiskBackend` (dirty + state), one map traversal
    /// over `state.base.chunks` to count locals via
    /// `ChunkCache::contains`. Bounded by manifest size (~256–1024
    /// entries typical), dominated by FS `try_exists` cost. Caller
    /// caches the result for ~1s.
    async fn cow_state(&self, id: SandboxId) -> Option<engram_core::types::cow_state::CowState> {
        #[cfg(target_os = "linux")]
        {
            // INVARIANT (see `nbd_sandboxes`): clone the Arc and drop the
            // guard before the `cow_state_for_entry` await. This read runs
            // on the ~1 s heartbeat cadence; holding the guard across the
            // manifest read would let a contending writer on the same
            // shard stall the heartbeat worker.
            let backend = self.nbd_sandboxes.get(&id).map(|e| e.backend.clone())?;
            self.cow_state_for_entry(id, backend).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            None
        }
    }

    /// Bulk fan-out — one entry per NBD-attached sandbox. Skips any
    /// per-sandbox lookup whose state read fails (best effort). One
    /// dashmap iteration; the per-entry COW read holds its locks
    /// only for its own state, so concurrent NBD writers see at most
    /// a brief contention.
    async fn cow_state_all(&self) -> Vec<engram_core::types::cow_state::CowStateRecord> {
        #[cfg(target_os = "linux")]
        {
            let ids: Vec<(SandboxId, Arc<crate::disk_daemon::ChunkedDiskBackend>)> = self
                .nbd_sandboxes
                .iter()
                .map(|e| (*e.key(), e.value().backend.clone()))
                .collect();
            let mut out = Vec::with_capacity(ids.len());
            for (sandbox_id, backend) in ids {
                if let Some(state) = self.cow_state_for_entry(sandbox_id, backend).await {
                    out.push(engram_core::types::cow_state::CowStateRecord { sandbox_id, state });
                }
            }
            out
        }
        #[cfg(not(target_os = "linux"))]
        {
            Vec::new()
        }
    }

    /// ADR 0016 Phase B commit 4a — admin trigger for the
    /// FlushScheduler primitive. Forces an immediate flush on the
    /// chunked-disk backend; returns the new manifest_ref if any
    /// chunks were drained, `None` if the sandbox isn't NBD-attached
    /// or has zero dirty bytes.
    ///
    /// Coord's `POST /api/admin/sessions/:id/flush-now` calls this
    /// (via gRPC) and pipes the returned manifest_ref into
    /// `MetadataStore::update_live_disk_manifest` immediately —
    /// bypasses the publisher's coalescing drain so the round-trip
    /// is deterministic for tests and operators.
    async fn flush_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<Option<engram_core::types::manifest::ManifestRef>, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let backend = self
                .nbd_sandboxes
                .get(&id)
                .map(|entry| entry.backend.clone());
            let Some(backend) = backend else {
                return Ok(None);
            };
            let outcome = backend.flush().await.map_err(|e| {
                SandboxError::Vm(format!("flush_sandbox: chunked-disk flush: {e}").into())
            })?;
            if outcome.chunks_flushed == 0 {
                return Ok(None);
            }
            Ok(Some(outcome.manifest_ref))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Ok(None)
        }
    }
}

#[cfg(target_os = "linux")]
impl PooledBackend {
    /// ADR 0016 Phase B commit 7 / ADR 0044 K2 — restart-time
    /// rehydration. Coord hands the host a list of `(session_id,
    /// sandbox_id, effective_disk_manifest)` rows at registration
    /// time; this method rebuilds the chunked-disk DATA PLANE for
    /// one surviving sandbox.
    ///
    /// Steps:
    /// 1. Discover the survivor's existing `/dev/nbdN` from the
    ///    reattached FC's rootfs drive and CLAIM that exact slot.
    ///    The surviving FC holds an open fd to that device, so the
    ///    pre-netlink behavior here — acquiring a FRESH slot —
    ///    served a device nobody read while the survivor's real
    ///    disk stayed dead (prod 2026-06-11: guest rootfs EIO after
    ///    a pod roll).
    /// 2. Build `ChunkedDiskBackend` from the manifest_ref and hand
    ///    the kernel a fresh serve socket for the SAME device via
    ///    netlink `NBD_CMD_RECONFIGURE`; guest I/O parked under
    ///    `dead_conn_timeout` resumes.
    /// 3. Insert into `nbd_sandboxes`, install the FlushScheduler,
    ///    pre-populate `session_bindings[sandbox_id] = session_id`
    ///    so the LiveManifestPublisher's resolver finds the binding
    ///    on the first post-rehydrate flush.
    ///
    /// Skip (`Ok(false)`) on any short-circuit (no nbd_pool /
    /// chunk_store / chunk_cache, sandbox already present, or no
    /// block-device rootfs to re-serve). Callers log + move on.
    pub async fn rehydrate_sandbox(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        disk_manifest: engram_core::types::manifest::ManifestRef,
    ) -> Result<bool, SandboxError> {
        let (pool, chunk_store, chunk_cache) = match (
            self.nbd_pool.as_ref(),
            self.chunk_store.as_ref(),
            self.chunk_cache.as_ref(),
        ) {
            (Some(p), Some(s), Some(c)) => (p, s, c),
            _ => return Ok(false),
        };
        if self.nbd_sandboxes.contains_key(&sandbox_id) {
            tracing::debug!(
                %sandbox_id,
                "rehydrate skipped: nbd_sandboxes entry already present",
            );
            return Ok(false);
        }
        let Some(device) = self.inner.rootfs_device(sandbox_id) else {
            tracing::debug!(
                %sandbox_id,
                "rehydrate skipped: survivor has no block-device rootfs to re-serve",
            );
            return Ok(false);
        };
        let Some(slot) = pool.claim(&device).await else {
            // Not in the free pool: either the device isn't part of
            // this host's slot set, or something else already leased
            // it — both mean re-serving here would fight another
            // owner. Loud, because the survivor's disk stays dead.
            tracing::warn!(
                %sandbox_id,
                device = %device.display(),
                "rehydrate: survivor's NBD device could not be claimed from the slot \
                 pool; its disk stays unserved (recover via evict_local → resume)",
            );
            return Ok(false);
        };

        // Shutdown-spool peek (2026-07-16 RCA): if the predecessor
        // generation died with acked-but-un-uploaded chunks, it left
        // them spooled on the hostPath volume. Adopt them into the
        // fresh backend (seeded BEFORE the RECONFIGURE releases the
        // guest's parked I/O) instead of rolling the live guest's disk
        // back to the last published manifest.
        let spool_root = self.shutdown_spool_root();
        let mut attach_ref = disk_manifest;
        let mut seed_dirty: Option<Vec<(usize, Vec<u8>)>> = None;
        if let Some(root) = &spool_root {
            match crate::disk_daemon::spool::read_spool(self.host_fs.as_ref(), root, sandbox_id)
                .await
            {
                Ok(Some((meta, chunks)))
                    if meta.manifest_id == disk_manifest.manifest_id
                        && meta.version >= disk_manifest.version =>
                {
                    // meta.version can be AHEAD of coord's ref: the
                    // predecessor uploaded chunks + manifest but died
                    // before its coord publish landed. The manifest
                    // object is already durable in the blob store
                    // (upload precedes publish), so attach from the
                    // spool's ref — the store-ahead recovery the flush
                    // path's version-conflict retry also leans on.
                    attach_ref = meta.manifest_ref();
                    seed_dirty = Some(chunks);
                }
                Ok(Some((meta, _))) => {
                    tracing::warn!(
                        %sandbox_id,
                        spool_manifest = %meta.manifest_ref(),
                        coord_manifest = %disk_manifest,
                        "shutdown spool is stale or from a foreign lineage; discarding",
                    );
                    let _ = crate::disk_daemon::spool::discard_spool(
                        self.host_fs.as_ref(),
                        root,
                        sandbox_id,
                    )
                    .await;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(
                        %sandbox_id,
                        error = %e,
                        "shutdown spool unreadable; discarding — acked writes it \
                         held are rolled back",
                    );
                    let _ = crate::disk_daemon::spool::discard_spool(
                        self.host_fs.as_ref(),
                        root,
                        sandbox_id,
                    )
                    .await;
                }
            }
        }
        let adopted_spool = seed_dirty.is_some();

        let store_arc = Arc::new(chunk_store.clone());
        let mut state = match crate::disk_daemon::reattach_manifest(
            attach_ref,
            chunk_cache.clone(),
            store_arc,
            slot,
            self.flush_config.dirty_threshold_bytes,
            seed_dirty,
        )
        .await
        {
            Ok(state) => state,
            Err((slot, e)) => {
                // Reattach refused — a RECONFIGURE failure (a device
                // configured by a pre-netlink host-agent generation, or an
                // identifier mismatch), or the pre-RECONFIGURE verify-on-read
                // finding the adopted spool bytes missing from the backend.
                // The survivor's disk stays dead; the evict_local → resume
                // ladder recovers the session.
                //
                // PARK the slot rather than letting it drop back into the
                // general pool: the surviving FC may still hold an open fd
                // to this exact /dev/nbdN, so releasing it would let the
                // stale-binding sweep DISCONNECT it (immediate guest EIO)
                // or hand it to an unrelated session. Quarantine keeps the
                // reserved bit set so the device is unavailable to new
                // claims until the session is recovered out-of-band.
                tracing::warn!(
                    %sandbox_id,
                    device = %device.display(),
                    error = %e,
                    "rehydrate reattach failed; parking the survivor's NBD slot \
                     (quarantined, kept out of the pool) to protect a possibly-live \
                     device; recover via evict_local → resume",
                );
                slot.quarantine();
                // ADR 0090: don't just log the remediation — advertise the
                // survivor in every heartbeat so the coordinator actually
                // DRIVES evict_local → resume (pre-fix, nothing consumed
                // this WARN and the teardown reconciler's orphan path
                // SIGKILLed the healthy VM ~60s later).
                self.quarantined_survivors.insert(sandbox_id, session_id);
                return Err(SandboxError::Vm(
                    format!(
                        "rehydrate nbd reattach at {}: {e} \
                         (survivor disk unserved; slot quarantined; recover via \
                         evict_local → resume)",
                        device.display()
                    )
                    .into(),
                ));
            }
        };

        state.install_flush_scheduler(
            sandbox_id,
            self.live_manifest_publisher.clone(),
            self.flush_config.clone(),
        );

        // Issue #224: terminal-mode check. The await window above
        // (pool.claim → reattach_manifest's netlink RECONFIGURE) is
        // multi-second; SIGTERM can have raised the abandon flag and
        // drained the (then-empty-of-this-id) map while we were in it.
        // If so, inserting `state` here would leak a live NBD data
        // plane that the abandon sweep has already passed — process
        // exit would then run `NbdHandle::Drop` and netlink-disconnect
        // the survivor's device the successor is about to RECONFIGURE.
        // Abandon in-place instead: the device the RECONFIGURE just
        // re-established stays kernel-configured for the successor.
        if self.is_abandoning() {
            tracing::warn!(
                %sandbox_id,
                "rehydrate completed during SIGTERM abandon; abandoning the \
                 re-served NBD data plane in-place (kernel config left alive \
                 for the successor) instead of inserting after the sweep",
            );
            state.abandon_for_shutdown();
            return Ok(false);
        }
        self.nbd_sandboxes.insert(sandbox_id, state);

        // Pre-populate session_bindings so the LiveManifestPublisher
        // resolver finds the binding on the first post-rehydrate
        // flush. Without this, the scheduler would skip-publish
        // with "sandbox not bound" — same shape as the pre-commit-
        // 5163366 cold-create regression.
        self.session_bindings.insert(sandbox_id, session_id);

        // The seeded chunks are now owned by the live dirty tier (and
        // the scheduler installed above will upload them promptly);
        // drop the spool so a LATER generation can't re-adopt stale
        // bytes over a newer divergence.
        if adopted_spool {
            if let Some(root) = &spool_root {
                if let Err(e) = crate::disk_daemon::spool::discard_spool(
                    self.host_fs.as_ref(),
                    root,
                    sandbox_id,
                )
                .await
                {
                    tracing::warn!(
                        %sandbox_id,
                        error = %e,
                        "adopted shutdown spool could not be discarded",
                    );
                }
            }
        }

        tracing::info!(
            %session_id,
            %sandbox_id,
            manifest = %attach_ref,
            device = %device.display(),
            spool_adopted = adopted_spool,
            "rehydrated chunked-disk data plane (RECONFIGURE) for survivor sandbox",
        );
        Ok(true)
    }

    /// ADR 0016 Phase B commit 5 — pre-restore half of the
    /// resume-path NBD attach. Spawned from `restore()` before
    /// `inner.restore()` reads the sidecar.
    ///
    /// When all four conditions hold — Linux + `nbd_pool` +
    /// `chunk_store` + `chunk_cache` — AND the snapshot row carries
    /// a chunked `disk_manifest`, this returns `Ok(Some(state))`:
    ///
    /// 1. Build a `ChunkedDiskBackend` from the snapshot's disk
    ///    manifest. The base manifest IS the snapshot's manifest;
    ///    no flat-file materialization happens.
    /// 2. Acquire a `/dev/nbdN` slot + spawn the NBD daemon. State
    ///    carries `scheduler: None` (the FlushScheduler is installed
    ///    post-`inner.restore` when the new sandbox_id is known).
    /// 3. Patch the FC sidecar's `spec.rootfs_source` to the NBD
    ///    device path. `restore_canonical_symlinks` (called from
    ///    `inner.restore`) then installs BOTH canonical symlinks
    ///    (host + source-keyed per FC's `state.bin` embedded path)
    ///    pointing at /dev/nbdN. FC's `load_snapshot` reads through
    ///    the symlink → through virtio-blk → through the NBD daemon
    ///    → through `ChunkedDiskBackend::read`. No bytes ever resident.
    ///
    /// On any short-circuit (no nbd_pool, no disk_manifest, etc.),
    /// returns `Ok(None)` and the caller falls back to
    /// `materialize_disk_if_missing` (the pre-Phase-B path).
    ///
    /// Closes ADR 0016 §"Phase B failure mode to close":
    /// - Symptom 1: cow_state_all iterates `nbd_sandboxes`. The
    ///   resumed sandbox is now in that map → diagnostic returns
    ///   non-null.
    /// - Symptom 2: snapshot path expects the canonical chunked-
    ///   disk layout (symlinks + nbd_sandboxes entry). Both are
    ///   present after a NBD-attach resume.
    async fn prepare_resume_nbd_attach(
        &self,
        metadata: &SnapshotMetadata,
        src: &std::path::Path,
        fork_at_attach: bool,
    ) -> Result<Option<crate::disk_daemon::NbdSandboxState>, SandboxError> {
        let (pool, chunk_store, chunk_cache) = match (
            self.nbd_pool.as_ref(),
            self.chunk_store.as_ref(),
            self.chunk_cache.as_ref(),
        ) {
            (Some(p), Some(s), Some(c)) => (p, s, c),
            // Non-NBD configuration. Caller falls back to
            // materialize-to-file (the pre-Phase-B path).
            _ => return Ok(None),
        };
        let Some(disk_ref) = metadata.disk_manifest else {
            // Legacy snapshot without a chunked disk lineage — the
            // materialize-to-file fallback handles it (and is itself
            // a no-op when disk_manifest is None).
            return Ok(None);
        };

        // 1+2. Spawn NBD. attach_manifest builds the ChunkedDiskBackend
        //      rebased on `disk_ref` and starts the daemon. The
        //      returned NbdSandboxState carries scheduler=None;
        //      install_flush_scheduler runs post-restore.
        //
        // ADR 0045 C1: a migration restore staged its (not-yet-durable)
        // disk manifest inline — attach from that content; the store
        // would 404 on the provisional ref.
        let store_arc = Arc::new(chunk_store.clone());
        if let Some(inline) = self.inline_disk_manifests.get(&disk_ref) {
            let state = crate::disk_daemon::attach_manifest_content(
                disk_ref,
                inline.value(),
                chunk_cache.clone(),
                store_arc,
                pool,
                self.flush_config.dirty_threshold_bytes,
            )
            .await
            .map_err(|e| SandboxError::Snapshot(format!("nbd attach (migration): {e}")))?;
            // The sidecar patch is NOT optional here. Without it the
            // sidecar's `rootfs_source` still names the SOURCE host's
            // /dev/nbdN; `restore_canonical_symlinks` then aims both
            // canonical symlinks at that literal device and FC reopens
            // it — on a host whose slot allocator handed this restore
            // a different index, that's a DIFFERENT SESSION'S live
            // disk (prod canaries 5fa742b7/4391e591: zeros + "Exec
            // format error" on every uncached read, with a cross-
            // session write hazard). Single-session hosts masked it
            // because nbd0 lined up on both sides by coincidence.
            let device_path = state.device_path().to_path_buf();
            patch_sidecar_rootfs_source(src, &device_path).await?;
            tracing::info!(
                src = %src.display(),
                manifest = %disk_ref,
                device = %device_path.display(),
                "migration NBD attach: inline manifest served, sidecar patched to /dev/nbdN",
            );
            return Ok(Some(state));
        }
        // Fork vs tick is the CALLER's call (see `restore_with`'s
        // `fork_disk_at_attach`): a resume re-attaches the session's OWN
        // already-forked manifest id (tick), while a fresh create off a
        // base snapshot — or a capture VM restoring a shared cold base —
        // attaches a SHARED manifest and must fork a private lineage.
        // Hardcoding `false` here was the incident-2026-07-10 bug: every
        // base-restored session published onto the shared base chain.
        let state = crate::disk_daemon::attach_manifest(
            disk_ref,
            chunk_cache.clone(),
            store_arc,
            pool,
            self.flush_config.dirty_threshold_bytes,
            fork_at_attach,
        )
        .await
        .map_err(|e| SandboxError::Vm(format!("nbd attach_manifest (resume): {e}").into()))?;

        // 3. Patch the FC sidecar's `spec.rootfs_source` to the NBD
        //    device path. `restore_canonical_symlinks` (inside
        //    `inner.restore`) follows this field to install the
        //    canonical-rootfs symlinks; without the patch the symlink
        //    targets the bake-time path that doesn't exist on the
        //    receiver, FC errors, and the resume bails.
        let device_path = state.device_path().to_path_buf();
        patch_sidecar_rootfs_source(src, &device_path).await?;

        tracing::info!(
            src = %src.display(),
            manifest = %disk_ref,
            device = %device_path.display(),
            "resume NBD attach: ChunkedDiskBackend ready, sidecar patched to /dev/nbdN",
        );
        Ok(Some(state))
    }

    /// Compute a [`CowState`] from one NBD backend + the host's
    /// caches. Shared by [`SandboxBackend::cow_state`] and
    /// [`SandboxBackend::cow_state_all`] so the two never drift.
    async fn cow_state_for_entry(
        &self,
        id: SandboxId,
        backend: Arc<crate::disk_daemon::ChunkedDiskBackend>,
    ) -> Option<engram_core::types::cow_state::CowState> {
        let disk_manifest = backend.manifest_ref().await;
        let dirty_chunks = backend.dirty_chunks_count().await as u32;
        let dirty_bytes = backend.dirty_bytes().await;
        let last_flush_unix_ms = backend.last_flush_unix_ms();

        // Count base chunks (manifest entries) + how many are
        // resident on local NVMe. Fetched once; the manifest comes
        // from BlobStorage via the chunk store and is cached in
        // `ChunkedDiskBackend.state` so this is one cheap lookup
        // server-side.
        let (base_chunks, base_chunks_local) = if let Some(cache) = self.chunk_cache.as_ref() {
            match self.chunk_store.clone() {
                Some(store) => match store.get_manifest(disk_manifest).await {
                    Ok(manifest) => {
                        let mut local = 0u32;
                        for chunk in &manifest.chunks {
                            if cache.contains(chunk.hash).await {
                                local += 1;
                            }
                        }
                        (manifest.chunks.len() as u32, local)
                    }
                    Err(e) => {
                        tracing::debug!(
                            sandbox_id = %id,
                            manifest = %disk_manifest,
                            error = %e,
                            "cow_state: failed to read disk manifest for locality counts; \
                             falling back to base_chunks=0",
                        );
                        (0, 0)
                    }
                },
                None => (0, 0),
            }
        } else {
            // No chunk cache wired (dev / mode=all without
            // NVMe tier). Manifest size is still observable, but
            // "local vs remote" is meaningless without a cache —
            // report both as 0 so the renderer can show "no
            // locality tracking" rather than a confusing "0/0".
            (0, 0)
        };

        let last_snapshot_unix_ms = self
            .last_snapshot_unix_ms
            .get(&id)
            .map(|r| *r.value())
            .unwrap_or(0);

        Some(engram_core::types::cow_state::CowState {
            disk_manifest,
            dirty_chunks,
            dirty_bytes,
            last_flush_unix_ms,
            base_chunks,
            base_chunks_local,
            // Memory manifest is snapshot-bounded and lives on the
            // PG `snapshots` row, not host-side. Coord layers it on
            // when projecting the host's response into the
            // session-shaped endpoint (`GET /api/sessions/:id/cow-state`).
            memory_manifest: None,
            last_snapshot_unix_ms,
        })
    }
}

/// Candidate filter for [`PooledBackend::rehydrate_local_survivors`],
/// factored pure so the selection semantics are testable without an
/// NBD stack: a durable chain-head record is a local rehydrate
/// candidate iff its sandbox is live in the backend (reattach pass
/// found the FC process's config), is not already NBD-served (the
/// coord-list pass got there first), and the record knows its bound
/// session (a `session_id: None` record predates binding — nothing
/// sound to rehydrate under; the coord list remains its only path).
#[cfg(any(target_os = "linux", test))]
fn local_survivor_candidates(
    records: Vec<crate::checkpoint::ChainHeadRecord>,
    live: &std::collections::HashSet<SandboxId>,
    served: &std::collections::HashSet<SandboxId>,
) -> Vec<(
    SessionId,
    SandboxId,
    engram_core::types::manifest::ManifestRef,
)> {
    records
        .into_iter()
        .filter_map(|r| {
            // The pure predicate (ADR 0098 P7, `engram_host_core::reattach`):
            // live ∧ unserved ∧ session-bound. Extracted so the host-internal
            // simulator drives the #739 park→roll→register scenario over the
            // same decision core.
            let has_session = r.session_id.is_some();
            if !engram_host_core::is_local_survivor_candidate(
                live.contains(&r.sandbox_id),
                served.contains(&r.sandbox_id),
                has_session,
            ) {
                if live.contains(&r.sandbox_id) && !served.contains(&r.sandbox_id) && !has_session {
                    tracing::debug!(
                        sandbox_id = %r.sandbox_id,
                        "local survivor rehydrate: chain-head record has no session \
                         binding; leaving this sandbox to the coordinator list",
                    );
                }
                return None;
            }
            let session_id = r.session_id.expect("candidate implies a bound session");
            Some((session_id, r.sandbox_id, r.manifest_ref))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::image_cache::ImageBundle;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use engram_sandbox_process::ProcessBackend;

    /// Session 731df805 (2026-07-17): the local survivor-rehydrate
    /// candidate filter. Live + unserved + session-bound records are
    /// candidates regardless of what the coordinator's list said;
    /// dead sandboxes (record outlived the VM), already-served ones
    /// (coord list got there first), and session-less records are not.
    #[test]
    fn local_survivor_candidates_filters() {
        let mk = |session: Option<SessionId>| crate::checkpoint::ChainHeadRecord {
            sandbox_id: SandboxId::new(),
            manifest_ref: engram_core::types::manifest::ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 3,
            },
            session_id: session,
            updated_at: chrono::Utc::now(),
        };
        let live_unserved = mk(Some(SessionId::new()));
        let live_served = mk(Some(SessionId::new()));
        let dead = mk(Some(SessionId::new()));
        let live_sessionless = mk(None);

        let live: std::collections::HashSet<SandboxId> = [
            live_unserved.sandbox_id,
            live_served.sandbox_id,
            live_sessionless.sandbox_id,
        ]
        .into_iter()
        .collect();
        let served: std::collections::HashSet<SandboxId> =
            [live_served.sandbox_id].into_iter().collect();

        let expect = vec![(
            live_unserved.session_id.unwrap(),
            live_unserved.sandbox_id,
            live_unserved.manifest_ref,
        )];
        let got = local_survivor_candidates(
            vec![live_unserved, live_served, dead, live_sessionless],
            &live,
            &served,
        );
        assert_eq!(got, expect);
    }

    /// ADR 0019 / telemetry restoration (#526): the stats-file → outcome
    /// mapping `restore()` drives `engram_resume_prefault_total` off.
    /// Covers the three labeled outcomes, the alarm shape (absent file),
    /// and defensive handling of an unparseable file.
    #[test]
    fn classify_prefault_outcome_covers_all_three_labels() {
        // Absent file (read failed / never written) => the alarm.
        assert_eq!(
            classify_prefault_outcome(None),
            PrefaultOutcome::StatsMissing
        );

        // trace_loaded: false => expected "nothing to replay" case.
        let no_trace = br#"{"trace_loaded":false,"chunks_in_trace":0,"installed":0,"skipped":0,"duration_ms":3}"#;
        assert_eq!(
            classify_prefault_outcome(Some(no_trace)),
            PrefaultOutcome::NoTrace
        );

        // trace_loaded: true => replayed, carrying installed/skipped.
        let replayed = br#"{"trace_loaded":true,"chunks_in_trace":10,"installed":8,"skipped":2,"duration_ms":42}"#;
        assert_eq!(
            classify_prefault_outcome(Some(replayed)),
            PrefaultOutcome::Replayed {
                installed: 8,
                skipped: 2
            }
        );

        // Corrupt/unparseable bytes => treated the same as absent (a
        // handler that died mid-write is just as much an alarm as one
        // that never wrote at all).
        assert_eq!(
            classify_prefault_outcome(Some(b"not json")),
            PrefaultOutcome::StatsMissing
        );

        // Extra/superset fields (the prefault-admission-control
        // convergence schema, e.g. a `trace_source` field) must not
        // break parsing — #[serde(default)] + no `deny_unknown_fields`.
        let superset = br#"{"trace_loaded":true,"chunks_in_trace":1,"installed":1,"skipped":0,"duration_ms":1,"trace_source":"canonical"}"#;
        assert_eq!(
            classify_prefault_outcome(Some(superset)),
            PrefaultOutcome::Replayed {
                installed: 1,
                skipped: 0
            }
        );
    }

    /// Review finding 1 regression test: a file that lands mid-poll
    /// (mirroring the uffd-handler's background prefault write racing
    /// resume-return) must be picked up, not misclassified as
    /// `stats_missing` on the first miss.
    #[tokio::test]
    async fn prefault_stats_retry_picks_up_a_late_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefault-stats.json");
        let write_path = path.clone();
        // Simulate the handler's background thread: the file doesn't
        // exist yet when the poll starts, and lands ~30ms later (well
        // inside the poll's bound but after several immediate misses).
        // ATOMIC temp+rename, exactly like the real handler — a plain
        // `write` here let the 5ms poll observe a created-but-empty file
        // under parallel-test load (one observed flake, 2026-07-17).
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            let tmp = write_path.with_extension("json.tmp");
            tokio::fs::write(
                &tmp,
                b"{\"trace_loaded\":true,\"installed\":3,\"skipped\":0}",
            )
            .await
            .unwrap();
            tokio::fs::rename(&tmp, &write_path).await.unwrap();
        });
        let bytes = read_prefault_stats_with_retry_bounded(
            &path,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            classify_prefault_outcome(bytes.as_deref()),
            PrefaultOutcome::Replayed {
                installed: 3,
                skipped: 0
            }
        );
    }

    /// A file that never appears within the bound still classifies as
    /// `stats_missing` — the real alarm case (handler died / never
    /// fired) must survive the race-tolerance fix, just no longer fire
    /// on the first instant.
    #[tokio::test]
    async fn prefault_stats_retry_times_out_to_stats_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefault-stats.json");
        let bytes = read_prefault_stats_with_retry_bounded(
            &path,
            std::time::Duration::from_millis(5),
            std::time::Duration::from_millis(30),
        )
        .await;
        assert_eq!(
            classify_prefault_outcome(bytes.as_deref()),
            PrefaultOutcome::StatsMissing
        );
    }

    /// Review finding 1: a leg wrapped by `spawn_leg_keepalive` must keep
    /// resending the SAME event on the keepalive interval until the
    /// guard is dropped, and must stop immediately once it is. This is
    /// the mechanism that keeps the coordinator's fenced `enable_jobs`
    /// write (== the capture-claim lease renewal, since the blind ticker
    /// was deleted) alive during the boot/snapshot legs, which have no
    /// `[warm]`-hook progress traffic of their own to drive it.
    #[tokio::test]
    async fn leg_keepalive_resends_until_dropped() {
        // SAFETY: `cargo nextest run` (the repo's enforced test runner —
        // see `just check`) gives every test its own process, so mutating
        // process env here can't race a sibling test.
        std::env::set_var("ENGRAM_CAPTURE_KEEPALIVE_SECS", "1");

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let event = engram_core::types::CaptureProgress {
            phase: engram_core::types::CapturePhase::Boot,
            sandbox_id: None,
            warm_stage: None,
            detail: None,
            output_tail: "boot-in-progress".into(),
            warm_stages: Vec::new(),
        };
        let guard = spawn_leg_keepalive(tx, event.clone());

        // Two resends within the (test-shrunk) 1s interval.
        for _ in 0..2 {
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("keepalive must resend within the timeout")
                .expect("channel must stay open while the guard is alive");
            assert_eq!(got.phase, engram_core::types::CapturePhase::Boot);
            assert_eq!(got.output_tail, "boot-in-progress");
        }

        drop(guard);
        while rx.try_recv().is_ok() {
            // drain anything already in flight before the drop landed
        }
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        assert!(
            rx.try_recv().is_err(),
            "no further resend once the guard is dropped"
        );
    }

    /// A capture-shaped policy (ADR 0080: assembled coordinator-side —
    /// see `session_boot::assemble_capture_egress_policy`, where the
    /// posture-mapping tests now live) with a scoped allowlist must
    /// translate + register cleanly on the proxy.
    #[test]
    fn capture_shaped_allowlist_policy_registers_on_the_proxy() {
        let policy = SessionEgressPolicy {
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            guest_ip: "169.254.0.2".parse().unwrap(),
            network_allow_hosts: vec!["accounts.google.com".into()],
            network_allow_host_patterns: vec!["*.auth0.com".into()],
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            secret_mode: engram_core::types::image::SecretMode::Literal,
        };
        let registry = engram_egress_proxy::Registry::new();
        crate::egress::register_policy(&registry, policy)
            .expect("proxy must accept the capture-egress allowlist");
    }

    /// A capture-shaped allow-all policy must register, and the proxy must
    /// bypass an arbitrary host under it (the dev posture for an image whose
    /// warm boot needs unrestricted network).
    #[test]
    fn capture_shaped_allow_all_policy_bypasses_on_the_proxy() {
        let policy = SessionEgressPolicy {
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            guest_ip: "169.254.0.3".parse().unwrap(),
            network_allow_hosts: Vec::new(),
            network_allow_host_patterns: Vec::new(),
            allow_all: true,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            secret_mode: engram_core::types::image::SecretMode::Literal,
        };
        let registry = engram_egress_proxy::Registry::new();
        let guest_ip = policy.guest_ip;
        crate::egress::register_policy(&registry, policy).expect("register allow-all");
        let state = registry.lookup(guest_ip).expect("registered");
        assert!(matches!(
            state.decide("anything.example.com"),
            engram_egress_proxy::Decision::Bypass
        ));
    }

    fn live_spec(image: &str) -> SandboxSpec {
        SandboxSpec {
            image: image.into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    /// Review finding 1: a slow cold boot (agentd-ready takes a while)
    /// must keep emitting `phase=boot` `CaptureProgress` on the keepalive
    /// interval — not just the one frame at the very start — or the
    /// coordinator's fenced write (== the capture-claim lease renewal)
    /// goes stale and a peer re-claims mid-boot. No `[warm]` hook here:
    /// this specifically isolates the BOOT leg's own keepalive from
    /// `run_warm_hook`'s (already covered by other tests).
    #[tokio::test]
    async fn slow_boot_keeps_emitting_boot_phase_progress() {
        // SAFETY: see the ENGRAM_WARM_STALL_SECS precedent elsewhere in
        // this module — nextest gives every test its own process.
        std::env::set_var("ENGRAM_CAPTURE_KEEPALIVE_SECS", "1");

        struct Probe {
            staging: PathBuf,
        }
        struct SlowBootMock(Arc<Probe>);
        #[async_trait]
        impl SandboxBackend for SlowBootMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Ok(SandboxId::new())
            }
            async fn wait_agent_ready(&self, _: SandboxId) -> Result<(), SandboxError> {
                // Longer than 2 keepalive ticks (1s each): proves the
                // boot leg's own keepalive fires WHILE we're still
                // waiting, not just the single frame sent before this
                // call.
                tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
                Ok(())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                unreachable!("no [warm] hook in this test — exec_stream must not be called")
            }
            async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.0.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![7u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                let _ = id;
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.0.staging.join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            staging: tmp.path().join("snaps"),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(SlowBootMock(probe));
        let pooled =
            PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialized"));
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        pooled
            .build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("slow-boot"),
                    warm: None,
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            )
            .await
            .expect("capture with no warm hook must succeed");

        let boot_events = {
            let mut n = 0;
            while let Ok(ev) = progress_rx.try_recv() {
                if ev.phase == engram_core::types::CapturePhase::Boot {
                    n += 1;
                }
            }
            n
        };
        assert!(
            boot_events >= 2,
            "expected the boot leg's keepalive to resend at least once \
             during a 2.5s wait_agent_ready with a 1s interval, got {boot_events} boot events"
        );
    }

    /// Review finding 3: `Exit(None)` (the child died to a signal) MUST
    /// NOT be unconditionally labeled `WarmGlobalTimeout` — that's only
    /// correct when agentd's `timeout_ms` in-guest backstop actually
    /// fired. A hook killed by something else (guest OOM, a manual
    /// `kill -9`) well before its declared `timeout_secs` budget must
    /// classify as the distinct, unattributed `WarmKilled` kind, or an
    /// operator gets steered to raise `timeout_secs` for a failure that
    /// timeout had nothing to do with.
    #[tokio::test]
    async fn warm_hook_early_signal_kill_is_not_misclassified_as_global_timeout() {
        struct EarlySignalKillMock;
        #[async_trait]
        impl SandboxBackend for EarlySignalKillMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                // Exit(None) arrives almost immediately — nowhere near
                // the 30s `timeout_secs` budget below.
                let events =
                    futures::stream::iter(vec![engram_core::types::sandbox::ExecEvent::Exit(None)]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-early-kill".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!("a killed [warm] hook must fail the capture before snapshot runs")
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner: Arc<dyn SandboxBackend> = Arc::new(EarlySignalKillMock);
        let pooled = PooledBackend::new(inner);
        let warm = WarmConfig {
            command: vec!["true".into()],
            // Generous: proves the classification isn't just "we're near
            // the deadline", it's "we're nowhere near it".
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(16);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-early-kill"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must not hang");

        let Err(SandboxError::CaptureFailed(failure)) = result else {
            panic!("expected a structured CaptureFailed error, got {result:?}");
        };
        assert_eq!(
            failure.kind,
            engram_core::types::CaptureFailureKind::WarmKilled,
            "an Exit(None) far from the timeout budget must not be labeled WarmGlobalTimeout"
        );
    }

    /// Issue #539: a `[warm]` hook that emits one `start` progress line and
    /// then goes silent (no more stdout/stderr, no `Exit`) must be killed
    /// within the (test-shrunk) stall budget — not the old single opaque
    /// global timeout, which this test sets generously (30s) precisely so
    /// a pass proves the STALL path fired, not the global-timeout backstop.
    /// The resulting `SandboxError::CaptureFailed` must name the open
    /// stage and carry the hook's own output in its tail; the same
    /// information must also have reached a live `CaptureProgress` event
    /// on the `progress` channel before the terminal failure.
    #[tokio::test]
    async fn warm_hook_stall_fails_capture_with_stage_and_tail() {
        use futures::StreamExt;

        // SAFETY: `cargo nextest run` (the repo's enforced test runner —
        // see `just check`) gives every test its own process, so mutating
        // process env here can't race a sibling test.
        std::env::set_var("ENGRAM_WARM_STALL_SECS", "1");

        struct SilentAfterStartMock;
        #[async_trait]
        impl SandboxBackend for SilentAfterStartMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                let line = "::engram-warm:: event=start stage=deps-up msg=installing deps\n";
                let events =
                    futures::stream::iter(vec![engram_core::types::sandbox::ExecEvent::Stdout(
                        bytes::Bytes::from(line),
                    )])
                    .chain(futures::stream::pending());
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-stall".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!("a stalled [warm] hook must fail the capture before snapshot runs")
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner: Arc<dyn SandboxBackend> = Arc::new(SilentAfterStartMock);
        let pooled = PooledBackend::new(inner);
        let warm = WarmConfig {
            command: vec!["true".into()],
            // Generous global timeout: the test proves the STALL path (1s,
            // via ENGRAM_WARM_STALL_SECS above), not this backstop.
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-stall"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must fail within the stall budget, not hang past the test timeout");

        let Err(SandboxError::CaptureFailed(failure)) = result else {
            panic!("expected a structured CaptureFailed error, got {result:?}");
        };
        assert_eq!(
            failure.kind,
            engram_core::types::CaptureFailureKind::WarmStall
        );
        assert_eq!(failure.stage.as_deref(), Some("deps-up"));
        assert!(
            failure.tail.contains("deps-up"),
            "tail must carry the hook's own output: {}",
            failure.tail
        );

        let mut saw_live_stage = false;
        while let Ok(ev) = progress_rx.try_recv() {
            if ev.warm_stage.as_deref() == Some("deps-up") {
                saw_live_stage = true;
            }
        }
        assert!(
            saw_live_stage,
            "expected a live CaptureProgress event naming the stage before the terminal failure"
        );
    }

    /// Issue #539/#563 review: a stage's own `deadline_secs` must be
    /// enforced even when the hook is chatty the whole time — continuous
    /// heartbeats keep the STALL clock reset forever, so a pass here can
    /// only be explained by the stage-deadline arm of the watchdog, not
    /// the stall detector (which this test sets generously precisely to
    /// rule it out). The resulting `SandboxError::CaptureFailed` must
    /// carry the `WarmStageDeadline` kind, the open stage's name, and the
    /// hook's own output in its tail; the same stage must also have
    /// reached a live `CaptureProgress` event before the terminal
    /// failure.
    #[tokio::test]
    async fn warm_hook_stage_deadline_fails_capture_with_stage_and_tail() {
        use futures::StreamExt;

        // SAFETY: see the stall test above — nextest gives every test its
        // own process, so mutating process env here can't race a sibling
        // test. Generous on purpose: this test proves the STAGE-DEADLINE
        // path (1s, via the hook's own `deadline_secs=1`), not the stall
        // backstop.
        std::env::set_var("ENGRAM_WARM_STALL_SECS", "30");

        struct ChattyStuckStageMock;
        #[async_trait]
        impl SandboxBackend for ChattyStuckStageMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                use engram_core::types::sandbox::ExecEvent;
                let start =
                    "::engram-warm:: event=start stage=wedged-stage deadline_secs=1 msg=starting\n";
                let heartbeat =
                    "::engram-warm:: event=heartbeat stage=wedged-stage msg=still going\n";
                let events =
                    futures::stream::once(
                        async move { ExecEvent::Stdout(bytes::Bytes::from(start)) },
                    )
                    .chain(futures::stream::unfold((), move |()| async move {
                        // Fires far faster than both the 1s stage deadline and
                        // the 30s stall budget above, so the stall clock never
                        // comes close to expiring — only the stage deadline can
                        // explain a failure here.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        Some((ExecEvent::Stdout(bytes::Bytes::from(heartbeat)), ()))
                    }));
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-stage-deadline".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!(
                    "a stage-deadline-killed [warm] hook must fail the capture before snapshot runs"
                )
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner: Arc<dyn SandboxBackend> = Arc::new(ChattyStuckStageMock);
        let pooled = PooledBackend::new(inner);
        let warm = WarmConfig {
            command: vec!["true".into()],
            // Generous global timeout: the test proves the STAGE DEADLINE
            // path (1s, via the hook's own `deadline_secs=1`), not this
            // backstop.
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-stage-deadline"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must fail within the stage-deadline budget, not hang past the test timeout");

        let Err(SandboxError::CaptureFailed(failure)) = result else {
            panic!("expected a structured CaptureFailed error, got {result:?}");
        };
        assert_eq!(
            failure.kind,
            engram_core::types::CaptureFailureKind::WarmStageDeadline
        );
        assert_eq!(failure.stage.as_deref(), Some("wedged-stage"));
        assert!(
            failure.tail.contains("wedged-stage"),
            "tail must carry the hook's own output: {}",
            failure.tail
        );

        let mut saw_live_stage = false;
        while let Ok(ev) = progress_rx.try_recv() {
            if ev.warm_stage.as_deref() == Some("wedged-stage") {
                saw_live_stage = true;
            }
        }
        assert!(
            saw_live_stage,
            "expected a live CaptureProgress event naming the stage before the terminal failure"
        );
    }

    /// Review finding 5: a hook that streams newline-free output (gradle
    /// rich-console `\r` redraws, binary noise) must not grow
    /// `pending_stdout` unbounded — and, once the cap drops the
    /// unparseable noise, a real `::engram-warm::` line arriving right
    /// after must still parse cleanly (the cap doesn't wedge future
    /// parsing).
    #[tokio::test]
    async fn warm_hook_newline_free_noise_does_not_wedge_progress_parsing() {
        struct Probe {
            staging: PathBuf,
        }
        struct NoiseThenLineMock(Arc<Probe>);
        #[async_trait]
        impl SandboxBackend for NoiseThenLineMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                use engram_core::types::sandbox::ExecEvent;
                // Well past OutputTail::DEFAULT_CAP_BYTES (16 KiB), no
                // newline anywhere — the exact shape that grew
                // `pending_stdout` unbounded pre-fix.
                let noise = vec![b'x'; 64 * 1024];
                let events = futures::stream::iter(vec![
                    ExecEvent::Stdout(bytes::Bytes::from(noise)),
                    ExecEvent::Stdout(bytes::Bytes::from(
                        "::engram-warm:: event=start stage=after-noise\n",
                    )),
                    ExecEvent::Stdout(bytes::Bytes::from(
                        "::engram-warm:: event=done stage=after-noise\n",
                    )),
                    ExecEvent::Exit(Some(0)),
                ]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-noise".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.0.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![7u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                let _ = id;
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.0.staging.join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            staging: tmp.path().join("snaps"),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(NoiseThenLineMock(probe));
        let pooled = Arc::new(
            PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialized")),
        );
        let warm = WarmConfig {
            command: vec!["true".into()],
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            pooled.build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-noise"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            ),
        )
        .await
        .expect("must not hang on unbounded newline-free output");

        result.expect("a hook that exits 0 must succeed even after newline-free noise");

        let mut saw_stage = false;
        while let Ok(ev) = progress_rx.try_recv() {
            if ev.warm_stage.as_deref() == Some("after-noise") {
                saw_stage = true;
            }
        }
        assert!(
            saw_stage,
            "the real progress line after the noise must still parse"
        );
    }

    /// Issue #539: a `[warm]` hook that emits two stages and exits 0 must
    /// succeed, and the ordered `CaptureProgress` events observed on the
    /// channel must carry the full (closed) stage history.
    #[tokio::test]
    async fn warm_hook_two_stages_then_exit_zero_succeeds_with_ordered_progress() {
        struct Probe {
            staging: PathBuf,
        }
        struct TwoStageMock(Arc<Probe>);
        #[async_trait]
        impl SandboxBackend for TwoStageMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                use engram_core::types::sandbox::ExecEvent;
                let lines = [
                    "::engram-warm:: event=start stage=deps-up\n",
                    "::engram-warm:: event=done stage=deps-up\n",
                    "::engram-warm:: event=start stage=migrations\n",
                    "::engram-warm:: event=done stage=migrations\n",
                ]
                .concat();
                let events = futures::stream::iter(vec![
                    ExecEvent::Stdout(bytes::Bytes::from(lines)),
                    ExecEvent::Exit(Some(0)),
                ]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-two-stage".into(),
                    events: Box::pin(events),
                })
            }
            async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.0.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![7u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                let _ = id;
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.0.staging.join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe {
            staging: tmp.path().join("snaps"),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(TwoStageMock(probe.clone()));
        let pooled = Arc::new(
            PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialized")),
        );

        let warm = WarmConfig {
            command: vec!["true".into()],
            timeout_secs: Some(30),
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);

        pooled
            .build_base_snapshot(
                engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                    spec: live_spec("warm-two-stage"),
                    warm: Some(warm),
                    capture_env: Default::default(),
                    capture_egress: None,
                    cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
                },
                progress_tx,
            )
            .await
            .expect("a two-stage hook that exits 0 must succeed");

        let mut events = Vec::new();
        while let Ok(ev) = progress_rx.try_recv() {
            events.push(ev);
        }
        assert!(
            !events.is_empty(),
            "expected at least one CaptureProgress event"
        );
        // The last warm-phase event's stage history must show both stages
        // closed with outcome `Done`, in emission order.
        let last_warm = events
            .iter()
            .rev()
            .find(|e| e.phase == engram_core::types::CapturePhase::Warm)
            .expect("at least one phase=warm event");
        assert_eq!(last_warm.warm_stages.len(), 2);
        assert_eq!(last_warm.warm_stages[0].name, "deps-up");
        assert_eq!(
            last_warm.warm_stages[0].outcome,
            engram_core::types::WarmStageOutcome::Done
        );
        assert_eq!(last_warm.warm_stages[1].name, "migrations");
        assert_eq!(
            last_warm.warm_stages[1].outcome,
            engram_core::types::WarmStageOutcome::Done
        );
    }

    /// ADR 0084 §B: `build_base_snapshot`'s cold-base/warm-overlay stage
    /// plan. `ColdBaseMock` tracks `create`/`restore`/`snapshot` call
    /// counts so each test asserts the EXACT stage sequence its
    /// `ColdBasePlan` should drive, without needing to fake FC's real
    /// `memory.diff` sparse-range format (that fidelity belongs to the
    /// FC integration test, not this unit suite) — this mock has no
    /// `checkpoint_dir` wired, so every `snapshot()` call routes through
    /// `inner.snapshot()` (never `inner.snapshot_diff()`); what's under
    /// test here is purely the STAGE SEQUENCE (create-vs-restore,
    /// snapshot call count, the hard-error gate, and the result shape),
    /// not the Full/Diff cost distinction itself.
    mod cold_base_stage_plan {
        use super::*;
        use engram_core::traits::sandbox::BuildBaseSnapshotRequest;
        use engram_core::types::capture_job::{ColdBaseMissReason, ColdBasePlan};
        use engram_core::types::image::WarmConfig;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct ColdBaseMock {
            staging: PathBuf,
            create_calls: Arc<AtomicUsize>,
            restore_calls: Arc<AtomicUsize>,
            snapshot_calls: Arc<AtomicUsize>,
            destroy_calls: Arc<AtomicUsize>,
            supports_diff: bool,
        }

        impl ColdBaseMock {
            fn new(staging: PathBuf, supports_diff: bool) -> Self {
                Self {
                    staging,
                    create_calls: Arc::new(AtomicUsize::new(0)),
                    restore_calls: Arc::new(AtomicUsize::new(0)),
                    snapshot_calls: Arc::new(AtomicUsize::new(0)),
                    destroy_calls: Arc::new(AtomicUsize::new(0)),
                    supports_diff,
                }
            }
        }

        #[async_trait]
        impl SandboxBackend for ColdBaseMock {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                self.create_calls.fetch_add(1, Ordering::SeqCst);
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                id: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                // A trivial always-succeeds hook (mirrors `command =
                // ["true"]` — no `::engram-warm::` progress lines, just a
                // clean exit).
                use engram_core::types::sandbox::ExecEvent;
                let events = futures::stream::iter(vec![ExecEvent::Exit(Some(0))]);
                Ok(ExecStream {
                    sandbox_id: id,
                    exec_id: "exec-cold-base-mock".into(),
                    events: Box::pin(events),
                })
            }
            fn supports_diff_checkpoints(&self) -> bool {
                self.supports_diff
            }
            async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.staging.join(snapshot_id.to_string());
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![9u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"x")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("manifest.json"), b"{}")
                    .await
                    .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "t:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging.join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                self.restore_calls.fetch_add(1, Ordering::SeqCst);
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                self.destroy_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        fn warm_config() -> WarmConfig {
            WarmConfig {
                command: vec!["true".into()],
                timeout_secs: Some(30),
                workdir: None,
                env: Vec::new(),
                network: None,
            }
        }

        fn fake_snapshot() -> SnapshotMetadata {
            SnapshotMetadata {
                id: engram_core::SnapshotId::new(),
                size_bytes: 4096,
                created_at: chrono::Utc::now(),
                image_version: "cold-base:1".into(),
                disk_manifest: None,
                memory_manifest: None,
                base_memory_manifest: None,
                migration_source: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: vec![],
                paused_at: None,
                peer_hints: Vec::new(),
            }
        }

        async fn pooled_over(mock: ColdBaseMock) -> Arc<PooledBackend> {
            let tmp = tempfile::tempdir().unwrap();
            let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
            );
            let cs = engram_chunk_store::ChunkStore::new(blob);
            let inner: Arc<dyn SandboxBackend> = Arc::new(mock);
            let p = Arc::new(
                PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialized")),
            );
            std::mem::forget(tmp);
            p
        }

        /// `NotApplicable` (non-FC host / warm-less image): single-stage
        /// path — `create` once, `restore` never, exactly ONE `snapshot`
        /// call, and the result carries no `cold_base`.
        #[tokio::test]
        async fn not_applicable_is_single_stage_with_no_cold_base() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-not-applicable"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::NotApplicable,
                    },
                    progress_tx,
                )
                .await
                .expect("single-stage capture must succeed");

            assert_eq!(counters.create_calls.load(Ordering::SeqCst), 1);
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(counters.snapshot_calls.load(Ordering::SeqCst), 1);
            assert!(result.cold_base.is_none());
        }

        /// Warm-less image on an FC host with `ColdBasePlan::Miss`: still
        /// single-stage (ONE `snapshot` call — the single Full path IS
        /// the cold base, ADR §B3) — but the result now carries
        /// `cold_base` (so a LATER warm image with the same content can
        /// reuse it), `freshly_captured: true`, with the SAME snapshot
        /// metadata as the artifact.
        #[tokio::test]
        async fn warm_less_miss_is_single_stage_but_reports_cold_base_identity() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-warm-less-miss"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Miss {
                            content_key: "ck-1".into(),
                            reason: ColdBaseMissReason::NoCandidate,
                        },
                    },
                    progress_tx,
                )
                .await
                .expect("warm-less miss must succeed");

            assert_eq!(counters.create_calls.load(Ordering::SeqCst), 1);
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                counters.snapshot_calls.load(Ordering::SeqCst),
                1,
                "warm-less has no hook to protect a pre-hook cold-base capture for"
            );
            let cb = result.cold_base.expect("must report cold-base identity");
            assert_eq!(cb.content_key, "ck-1");
            assert!(cb.freshly_captured);
            assert_eq!(cb.miss_reason, Some(ColdBaseMissReason::NoCandidate));
            assert_eq!(
                cb.snapshot.id, result.snapshot.id,
                "the single Full capture IS both the cold base and the artifact"
            );
        }

        /// Warm image on a MISS: two `snapshot` calls (the pre-hook Full
        /// cold-base capture, then the post-hook overlay) and exactly one
        /// `create` — no cold boot avoided (there was nothing to reuse),
        /// but the cold base is minted BEFORE the hook runs.
        #[tokio::test]
        async fn warm_miss_takes_a_pre_hook_snapshot_then_the_overlay() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-warm-miss"),
                        warm: Some(warm_config()),
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Miss {
                            content_key: "ck-2".into(),
                            reason: ColdBaseMissReason::ChunksMissing,
                        },
                    },
                    progress_tx,
                )
                .await
                .expect("warm miss must succeed");

            assert_eq!(counters.create_calls.load(Ordering::SeqCst), 1);
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                counters.snapshot_calls.load(Ordering::SeqCst),
                2,
                "a WARM miss mints its own cold base before the hook, then takes the overlay"
            );
            let cb = result.cold_base.expect("must report the minted cold base");
            assert!(cb.freshly_captured);
            assert_eq!(cb.miss_reason, Some(ColdBaseMissReason::ChunksMissing));
            assert_ne!(
                cb.snapshot.id, result.snapshot.id,
                "the pre-hook cold base and the post-hook overlay are DISTINCT captures"
            );
        }

        /// A `Hit`: `restore` instead of `create` — no cold boot at all —
        /// and exactly one `snapshot` call (the overlay). The reported
        /// `cold_base` echoes the candidate's OWN (unchanged) identity,
        /// not the fresh overlay, and `freshly_captured` is `false`.
        #[tokio::test]
        async fn hit_restores_instead_of_creating() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), true);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);
            let candidate = fake_snapshot();
            let candidate_id = candidate.id;

            let result = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-hit"),
                        warm: Some(warm_config()),
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Hit {
                            content_key: "ck-3".into(),
                            snapshot: Box::new(candidate),
                        },
                    },
                    progress_tx,
                )
                .await
                .expect("hit path must succeed");

            assert_eq!(
                counters.create_calls.load(Ordering::SeqCst),
                0,
                "a Hit must never cold-boot"
            );
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                counters.snapshot_calls.load(Ordering::SeqCst),
                1,
                "only the post-hook overlay — no pre-hook capture needed, we already have one"
            );
            let cb = result
                .cold_base
                .expect("must report the cold-base identity");
            assert!(!cb.freshly_captured);
            assert_eq!(cb.miss_reason, None);
            assert_eq!(
                cb.snapshot.id, candidate_id,
                "a Hit echoes the EXISTING candidate's identity, not a new capture"
            );
            assert_ne!(
                cb.snapshot.id, result.snapshot.id,
                "the candidate's identity is distinct from the fresh overlay artifact"
            );
        }

        /// ADR decision 11: a `Hit`/`Miss` plan on a backend that can't
        /// actually diff is a placement bug — hard error, no cold boot
        /// ever attempted, never a silent single-stage fallback.
        #[tokio::test]
        async fn capability_mismatch_hard_errors_before_booting_anything() {
            let staging = tempfile::tempdir().unwrap();
            // supports_diff = false: this "FC" host has dirty-page
            // tracking disabled (or IS non-FC) — either way it can't
            // honor a Hit/Miss plan.
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), false);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let err = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-mismatch"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Miss {
                            content_key: "ck-4".into(),
                            reason: ColdBaseMissReason::NoCandidate,
                        },
                    },
                    progress_tx,
                )
                .await
                .expect_err("a capability mismatch must hard-error");

            match err {
                SandboxError::CaptureFailed(failure) => {
                    assert_eq!(
                        failure.kind,
                        engram_core::types::CaptureFailureKind::ColdBaseCapabilityMismatch
                    );
                    assert!(!failure.kind.is_retryable());
                }
                other => {
                    panic!("expected CaptureFailed(ColdBaseCapabilityMismatch), got {other:?}")
                }
            }
            assert_eq!(
                counters.create_calls.load(Ordering::SeqCst),
                0,
                "must fail BEFORE ever booting a VM"
            );
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
            assert_eq!(counters.destroy_calls.load(Ordering::SeqCst), 0);
        }

        /// A `Hit` combined with an actual capability mismatch is the
        /// SAME hard error as `Miss` — the gate checks the plan variant,
        /// not which specific variant it is.
        #[tokio::test]
        async fn hit_with_capability_mismatch_also_hard_errors() {
            let staging = tempfile::tempdir().unwrap();
            let mock = ColdBaseMock::new(staging.path().to_path_buf(), false);
            let counters = mock.clone();
            let pooled = pooled_over(mock).await;
            let (progress_tx, _rx) = tokio::sync::mpsc::channel(64);

            let err = pooled
                .build_base_snapshot(
                    BuildBaseSnapshotRequest {
                        spec: live_spec("cb-hit-mismatch"),
                        warm: None,
                        capture_env: Default::default(),
                        capture_egress: None,
                        cold_base_plan: ColdBasePlan::Hit {
                            content_key: "ck-5".into(),
                            snapshot: Box::new(fake_snapshot()),
                        },
                    },
                    progress_tx,
                )
                .await
                .expect_err("a capability mismatch must hard-error even on a Hit");

            assert!(matches!(
                err,
                SandboxError::CaptureFailed(f)
                    if f.kind == engram_core::types::CaptureFailureKind::ColdBaseCapabilityMismatch
            ));
            assert_eq!(counters.restore_calls.load(Ordering::SeqCst), 0);
        }
    }

    /// ADR 0045 C2 (E2B fold): hot chunks lead the pull set in fault
    /// order; the cold tail keeps its manifest order; unknown hot
    /// hashes (chunks already staged/cached) are simply absent.
    #[test]
    fn order_hot_first_leads_with_the_hot_set() {
        use engram_chunk_store::manifest::ChunkHash;
        let h = |b: u8| ChunkHash::of(&[b]);
        let remaining = vec![h(1), h(2), h(3), h(4), h(5)];
        // Hot order: 4 first, then 2; 9 is not in the pull set at all.
        let hot = vec![*h(4).as_bytes(), *h(9).as_bytes(), *h(2).as_bytes()];
        let got = PooledBackend::order_hot_first(remaining, &hot);
        assert_eq!(got, vec![h(4), h(2), h(1), h(3), h(5)]);

        // Empty hot set: untouched.
        let got = PooledBackend::order_hot_first(vec![h(7), h(6)], &[]);
        assert_eq!(got, vec![h(7), h(6)]);
    }

    /// ADR 0045 C1: migration_fetch is allowlist-gated and serves
    /// state.bin/sidecar/chunks as offset-framed streams.
    #[tokio::test]
    async fn migration_fetch_rejects_unlisted_hash_and_bad_export_id() {
        use engram_core::types::snapshot::MigrationItem;
        use futures::StreamExt;
        // A near-full dev/CI disk trips the cache's default free-space
        // floor and evicts the chunk this test `cache.put`s below before
        // `migration_fetch` can serve it. Nextest runs each test in its
        // own process, so this env override is safe (see
        // two_host_drain_wave.rs / migration_source.rs).
        std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let cache = engram_chunk_store::ChunkCache::new(
            engram_chunk_store::cache::ChunkCacheConfig::new(tmp.path().join("cache")),
        );
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs, tmp.path().join("materialize"))
            .with_chunk_cache(cache.clone());

        // Hand-build an export: one allowed cache-resident chunk +
        // state.bin/sidecar files.
        let allowed_bytes = vec![0x5Au8; 8192];
        let allowed = engram_chunk_store::manifest::ChunkHash::of(&allowed_bytes);
        cache.put(allowed, &allowed_bytes).await.unwrap();
        let unlisted = engram_chunk_store::manifest::ChunkHash::of(b"not-in-this-export");
        let export_dir = tmp.path().join("export");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("state.bin"), b"vmstate-bytes").unwrap();
        std::fs::write(export_dir.join("manifest.json"), b"{}").unwrap();
        let sandbox_id = SandboxId::new();
        let export_id = crate::migration::MigrationRegistry::mint_export_id();
        let guard_src = Arc::new(tokio::sync::Mutex::new(()));
        assert!(pooled.migrations.insert(crate::migration::MigrationExport {
            export_id: export_id.clone(),
            sandbox_id,
            snapshot_dir: export_dir,
            allowed_chunks: [allowed].into_iter().collect(),
            disk_pending: None,
            disk_seal: None,
            clock: std::sync::Arc::new(engram_core::traits::SystemClock::new()),
            created_at: std::time::Duration::ZERO,
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(
                engram_core::traits::Clock::now_mono(&engram_core::traits::SystemClock::new()),
            )),
            capture_guard: guard_src.clone().try_lock_owned().unwrap(),
        }));

        // Bad export id => NotFound.
        let Err(err) = pooled
            .migration_fetch("0000", vec![MigrationItem::StateBin])
            .await
        else {
            panic!("bad export must be refused");
        };
        assert!(matches!(err, SandboxError::NotFound));

        // Unlisted chunk => InvalidSpec, even with a valid export id.
        let Err(err) = pooled
            .migration_fetch(&export_id, vec![MigrationItem::Chunk(*unlisted.as_bytes())])
            .await
        else {
            panic!("unlisted hash must be refused");
        };
        assert!(matches!(err, SandboxError::InvalidSpec(_)));

        // Valid pull: state.bin + the allowed chunk, framed in order.
        let stream = pooled
            .migration_fetch(
                &export_id,
                vec![
                    MigrationItem::StateBin,
                    MigrationItem::Chunk(*allowed.as_bytes()),
                ],
            )
            .await
            .expect("valid fetch");
        let frames: Vec<_> = stream.map(|f| f.expect("frame")).collect().await;
        let item0: Vec<u8> = frames
            .iter()
            .filter(|f| f.item_idx == 0)
            .flat_map(|f| f.data.to_vec())
            .collect();
        assert_eq!(item0, b"vmstate-bytes");
        let item1: Vec<u8> = frames
            .iter()
            .filter(|f| f.item_idx == 1)
            .flat_map(|f| f.data.to_vec())
            .collect();
        assert_eq!(item1, allowed_bytes);
        assert!(frames.iter().any(|f| f.item_idx == 1 && f.last));
    }

    /// Issue #222: `migration_capture_postcopy` must remove the pending
    /// presetup ONLY when the export id matches. Presetups are
    /// last-write-wins (a coordinator retry re-mints a new export under
    /// the same sandbox id), so a stale straggler `capture(id, E1)`
    /// arriving after presetup re-minted E2 must NOT evict the live E2
    /// entry — it must just error, leaving E2 intact for the legitimate
    /// `capture(id, E2)`.
    ///
    /// Regression for the pre-fix `remove(&id).filter(...)`, which evicted
    /// the entry unconditionally and then discarded it on mismatch,
    /// dooming the legitimate capture to "no matching presetup".
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn capture_postcopy_export_mismatch_leaves_live_presetup_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner);

        let id = SandboxId::new();
        // E2 is the live presetup (the newer attempt's mint).
        let live_export = "E2".to_string();
        pooled.pending_presetups.insert(
            id,
            PendingPresetup {
                export_id: live_export.clone(),
                peer_token: "tok".into(),
                chain_ref: engram_core::types::manifest::ManifestRef::new(),
            },
        );

        // A straggler capture from the abandoned attempt (export E1).
        let Err(err) = pooled.migration_capture_postcopy(id, "E1").await else {
            panic!("stale-export straggler must be refused");
        };
        assert!(
            matches!(&err, SandboxError::InvalidSpec(m) if m.contains("no matching presetup")),
            "expected the no-matching-presetup error, got: {err:?}"
        );
        // The crux of #222: E2 must SURVIVE the mismatched straggler.
        assert!(
            pooled.pending_presetups.contains_key(&id),
            "the live presetup was evicted by a mismatched straggler (the #222 bug)"
        );
        assert_eq!(
            pooled.pending_presetups.get(&id).unwrap().export_id,
            live_export,
            "the surviving presetup must still be E2"
        );

        // The legitimate capture for E2 now gets PAST the presetup gate:
        // it consumes E2 (so it no longer returns "no matching presetup")
        // and instead fails at the next check — the checkpoint chain was
        // never registered for this synthetic sandbox.
        let Err(err) = pooled.migration_capture_postcopy(id, &live_export).await else {
            panic!("E2 capture must proceed past the presetup gate");
        };
        assert!(
            matches!(&err, SandboxError::InvalidSpec(m) if m.contains("checkpoint chain vanished")),
            "expected to pass the presetup gate and fail at the chain check, got: {err:?}"
        );
        // E2 was consumed by the matching capture.
        assert!(
            !pooled.pending_presetups.contains_key(&id),
            "the matching capture must consume the presetup"
        );
    }

    /// ADR 0045 C2 disk post-copy: `DiskChunkAt` serves a sealed
    /// chunk's raw bytes straight from the export's seal (the seal IS
    /// the allowlist — unsealed indices and seal-less exports are
    /// refused), and `DiskSealInfo` serves the descriptor file.
    #[tokio::test]
    async fn migration_fetch_serves_sealed_disk_chunks_by_index() {
        use engram_core::types::snapshot::MigrationItem;
        use futures::StreamExt;
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob.clone());
        let cache = engram_chunk_store::ChunkCache::new(
            engram_chunk_store::cache::ChunkCacheConfig::new(tmp.path().join("cache")),
        );
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs, tmp.path().join("materialize"))
            .with_chunk_cache(cache.clone());

        // A real seal: a tiny disk backend with one dirty chunk.
        let chunk_size = 4096u64;
        let store2 = Arc::new(engram_chunk_store::ChunkStore::new(blob));
        let base_bytes = vec![0xAAu8; chunk_size as usize];
        let h0 = store2.put_chunk(&base_bytes).await.unwrap();
        let manifest = engram_chunk_store::Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: engram_chunk_store::manifest::ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(chunk_size),
            total_bytes: 2 * chunk_size,
            chunks: vec![engram_chunk_store::manifest::ChunkRef {
                offset: 0,
                hash: h0,
            }],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let mref = engram_core::types::manifest::ManifestRef::new();
        store2.put_manifest(mref, &manifest).await.unwrap();
        let disk = crate::disk_daemon::ChunkedDiskBackend::new(
            mref,
            &manifest,
            engram_chunk_store::ChunkCache::new(engram_chunk_store::cache::ChunkCacheConfig::new(
                tmp.path().join("cache2"),
            )),
            store2,
            u64::MAX,
        )
        .unwrap();
        disk.write(0, &vec![0x42u8; chunk_size as usize])
            .await
            .unwrap();
        let (seal, _m, _r) = disk.seal_for_postcopy().await;
        assert_eq!(seal.indices(), vec![0]);

        let export_dir = tmp.path().join("export");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("state.bin"), b"vmstate-bytes").unwrap();
        std::fs::write(
            export_dir.join("disk-seal.json"),
            b"{\"seal\":\"descriptor\"}",
        )
        .unwrap();
        let sandbox_id = SandboxId::new();
        let export_id = crate::migration::MigrationRegistry::mint_export_id();
        let guard = Arc::new(tokio::sync::Mutex::new(()));
        assert!(pooled.migrations.insert(crate::migration::MigrationExport {
            export_id: export_id.clone(),
            sandbox_id,
            snapshot_dir: export_dir,
            allowed_chunks: Default::default(),
            disk_pending: None,
            disk_seal: Some(Arc::new(seal)),
            clock: std::sync::Arc::new(engram_core::traits::SystemClock::new()),
            created_at: std::time::Duration::ZERO,
            post_copy: true,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(
                engram_core::traits::Clock::now_mono(&engram_core::traits::SystemClock::new()),
            )),
            capture_guard: guard.clone().try_lock_owned().unwrap(),
        }));

        // Sealed index: raw bytes from RAM.
        let stream = pooled
            .migration_fetch(
                &export_id,
                vec![MigrationItem::DiskSealInfo, MigrationItem::DiskChunkAt(0)],
            )
            .await
            .expect("valid fetch");
        let frames: Vec<_> = stream.map(|f| f.expect("frame")).collect().await;
        let info: Vec<u8> = frames
            .iter()
            .filter(|f| f.item_idx == 0)
            .flat_map(|f| f.data.to_vec())
            .collect();
        assert_eq!(info, b"{\"seal\":\"descriptor\"}");
        let chunk: Vec<u8> = frames
            .iter()
            .filter(|f| f.item_idx == 1)
            .flat_map(|f| f.data.to_vec())
            .collect();
        assert_eq!(chunk.len(), chunk_size as usize);
        assert!(
            chunk.iter().all(|b| *b == 0x42),
            "the SEALED bytes, not base"
        );

        // Unsealed index: refused (streamed error).
        let stream = pooled
            .migration_fetch(&export_id, vec![MigrationItem::DiskChunkAt(1)])
            .await
            .expect("stream opens; the per-item error rides it");
        let results: Vec<_> = stream.collect().await;
        assert!(
            results.iter().any(|r| r.is_err()),
            "unsealed index must be refused",
        );
    }

    /// ADR 0045 C1: capture without a checkpoint chain signals the
    /// snapshot-rehome fallback (InvalidSpec), not a hard error.
    #[tokio::test]
    async fn migration_capture_without_chain_signals_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);
        let cache = engram_chunk_store::ChunkCache::new(
            engram_chunk_store::cache::ChunkCacheConfig::new(tmp.path().join("cache")),
        );
        let inner = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().join("sandboxes"),
        ));
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs, tmp.path().join("materialize"))
            .with_chunk_cache(cache)
            .with_checkpoint_dir(tmp.path().join("checkpoints"));
        let Err(err) = pooled.migration_capture(SandboxId::new()).await else {
            panic!("no chain must signal fallback");
        };
        assert!(matches!(err, SandboxError::InvalidSpec(_)));
    }

    // ──────────────────────────────────────────────────────────────
    // Issue #202: the capture unwind guard. A migration/snapshot
    // capture that errors or is cancelled AFTER pause + fence + drain
    // must NOT leave the guest paused, the fence stuck, or the drained
    // disk chunks lost. The guard's `Drop` runs the same recovery
    // `migration_abort` does. A successful capture defuses it (no-op).
    // ──────────────────────────────────────────────────────────────

    /// Inner backend double that records `resume` calls so the test can
    /// assert the unwind guard un-pauses the guest.
    struct ResumeSpy {
        resumes: std::sync::Arc<parking_lot::Mutex<Vec<SandboxId>>>,
    }
    #[async_trait]
    impl SandboxBackend for ResumeSpy {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
            PathBuf::new()
        }
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.resumes.lock().push(id);
            Ok(())
        }
    }

    /// Build a `ChunkedDiskBackend` over a 4-chunk sparse base with one
    /// dirty chunk written, plus its store. Mirrors the disk_daemon
    /// `build_backend` test helper.
    async fn unwind_test_disk_backend() -> (
        std::sync::Arc<crate::disk_daemon::ChunkedDiskBackend>,
        std::sync::Arc<engram_chunk_store::ChunkStore>,
        tempfile::TempDir,
    ) {
        use engram_chunk_store::manifest::{ChunkSize, ManifestKind, ManifestRef};
        use engram_chunk_store::{ChunkCache, ChunkStore, Manifest};
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(dir.path().to_path_buf()),
        );
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let mut cfg = engram_chunk_store::cache::ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes: chunk_size * 4,
            chunks: Vec::new(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let backend = Arc::new(
            crate::disk_daemon::ChunkedDiskBackend::new(
                manifest_ref,
                &manifest,
                cache,
                store.clone(),
                u64::MAX,
            )
            .unwrap(),
        );
        // One acked write → one dirty chunk.
        backend
            .write(0, &vec![0x42; chunk_size as usize])
            .await
            .unwrap();
        (backend, store, dir)
    }

    /// THE regression: an armed-but-not-defused guard (an error or a
    /// cancelled capture) must resume the guest, clear the fence, and
    /// re-queue the drained chunk so it lands in the next published
    /// manifest. Without the fix the guest stays paused forever, the
    /// fence no-ops every future flush, and the drained chunk is gone.
    #[tokio::test]
    async fn capture_unwind_on_drop_resumes_unfences_and_requeues() {
        let (backend, store, _dir) = unwind_test_disk_backend().await;
        let resumes = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let inner: Arc<dyn SandboxBackend> = Arc::new(ResumeSpy {
            resumes: resumes.clone(),
        });
        let id = SandboxId::new();

        // Simulate a migration capture up to the "point of no return":
        // fence raised, dirty buffer drained out into `pending`.
        backend.set_migration_fence(true);
        let pending = backend.flush_local().await.unwrap();
        // A fenced flush no-ops (the bug's consequence #2 surface).
        assert_eq!(
            backend.flush().await.unwrap().chunks_flushed,
            0,
            "fence raised: flush must no-op",
        );

        // Arm the guard with the drained state — then DROP it without
        // defusing, exactly as an `Err`/cancel between pause and the
        // export insert would.
        {
            let mut guard = CaptureUnwind::new(inner.clone(), id);
            guard.arm();
            guard.disk_backend = Some(backend.clone());
            guard.fenced = true;
            guard.disk_pending = Some(pending);
            // guard dropped here (never defused)
        }
        // Drop spawns the async recovery; let it run.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if !resumes.lock().is_empty() {
                break;
            }
        }

        assert_eq!(
            resumes.lock().clone(),
            vec![id],
            "unwind must resume the guest exactly once",
        );
        // Fence cleared + chunk re-queued ⟹ the next flush publishes it.
        let after = backend.flush().await.unwrap();
        assert_eq!(
            after.chunks_flushed, 1,
            "unwind must clear the fence AND re-queue the drained chunk",
        );
        // And that chunk is durable in the store (present in the next
        // published manifest's chunk set).
        let manifest = store.get_manifest(after.manifest_ref).await.unwrap();
        assert!(
            !manifest.chunks.is_empty(),
            "the re-queued write must appear in the next published manifest",
        );
    }

    /// The success path: a defused guard is an inert no-op — no resume,
    /// the fence stays as the owning export left it, and the pending it
    /// no longer holds is owned elsewhere (here: dropped by the test,
    /// standing in for the export taking ownership).
    #[tokio::test]
    async fn capture_unwind_defused_is_a_noop() {
        let (backend, _store, _dir) = unwind_test_disk_backend().await;
        let resumes = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let inner: Arc<dyn SandboxBackend> = Arc::new(ResumeSpy {
            resumes: resumes.clone(),
        });
        let id = SandboxId::new();

        backend.set_migration_fence(true);
        let pending = backend.flush_local().await.unwrap();

        {
            let mut guard = CaptureUnwind::new(inner.clone(), id);
            guard.arm();
            guard.disk_backend = Some(backend.clone());
            guard.fenced = true;
            guard.disk_pending = Some(pending);
            // Success path: the export takes ownership of the drained
            // pending, then the guard is defused.
            let _owned_by_export = guard.disk_pending.take();
            guard.defuse();
        }
        // Give any erroneously-spawned recovery task a chance to run.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            resumes.lock().is_empty(),
            "a defused guard must NOT resume the guest",
        );
        // Fence untouched by the guard (the export owns it now).
        assert_eq!(
            backend.flush().await.unwrap().chunks_flushed,
            0,
            "defused guard must leave the fence raised (export owns it)",
        );
    }

    #[tokio::test]
    async fn materialize_chunked_rootfs_round_trips_and_dedupes() {
        // ADR 0007 acceptance: a chunked manifest is sufficient to
        // reproduce the disk bit-for-bit, AND concurrent resolves
        // share the materialized file rather than each producing
        // their own.
        use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
        let tmp = tempfile::tempdir().unwrap();
        // Local-filesystem BlobStorage = production code path for
        // dev. Same trait the host-agent uses against GCS in prod.
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // A fake "ext4" — small but enough to span more than one
        // 16 MiB chunk so the chunking path is exercised end-to-end.
        let src = tmp.path().join("rootfs.ext4");
        let mut bytes = vec![0u8; 24 * 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        tokio::fs::write(&src, &bytes).await.unwrap();
        let manifest = cs.chunk_file(&src, ManifestKind::Disk, None).await.unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        let bundle = ImageBundle {
            schema_version: 1,
            disk_manifest: mref,
            bootstrap_disk_available: false,
        };
        let materialize_dir = tmp.path().join("materialized");
        let lock = Mutex::new(());

        let path1 = materialize_chunked_rootfs(
            &cs,
            None,
            &materialize_dir,
            "img:1",
            bundle.disk_manifest,
            &lock,
        )
        .await
        .unwrap();
        let restored = tokio::fs::read(&path1).await.unwrap();
        assert_eq!(restored, bytes, "byte-for-byte mismatch");

        // Second call short-circuits via the size check — same path,
        // no rewrite.
        let mtime_before = std::fs::metadata(&path1).unwrap().modified().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let path2 = materialize_chunked_rootfs(
            &cs,
            None,
            &materialize_dir,
            "img:1",
            bundle.disk_manifest,
            &lock,
        )
        .await
        .unwrap();
        assert_eq!(path1, path2);
        let mtime_after = std::fs::metadata(&path2).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "second call must not rewrite the file",
        );
    }

    /// ADR 0007 / Phase 5: snapshot wrap chunks an FC-style
    /// `memory.bin` into the chunk store and patches the FC sidecar
    /// `manifest.json` with the resulting `memory_manifest` ref.
    /// Validates that:
    ///  - `SnapshotMetadata.memory_manifest` carries the ref
    ///  - `manifest.json` gained a `memory_manifest` field
    ///  - the manifest in the chunk store is byte-equal to memory.bin
    ///    when materialised back
    ///
    /// Uses a fake inner backend (writes the snapshot artifacts to
    /// `dest` like FC does on a real snapshot, but skips the actual
    /// VM machinery) so the test runs cross-platform.
    #[tokio::test]
    async fn snapshot_chunks_fc_memory_bin_and_patches_manifest() {
        use engram_chunk_store::{ChunkStore, ManifestKind};
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();
        let _dest = tmp.path().join("snap-1");

        // 1. Synthetic memory.bin: 1 MiB of distinct, non-zero
        //    content so chunk_file produces multiple chunks. Two
        //    512 KiB chunks (default memory chunk size).
        let mut bytes = vec![0u8; 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i % 200) + 1) as u8; // skip zero so chunks aren't elided
        }

        // 2. Inner backend stub: writes memory.bin + state.bin +
        //    manifest.json to its own per-snapshot dir (ADR 0007
        //    Phase 6 contract), returns a bare SnapshotMetadata
        //    with memory_manifest=None (the FC bare-snapshot shape).
        struct FakeFcBackend {
            payload: Vec<u8>,
            staging_root: PathBuf,
        }
        impl FakeFcBackend {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for FakeFcBackend {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"state-bin-placeholder")
                    .await
                    .unwrap();
                // Mirror FC's manifest.json shape minimally.
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1",
                        "rootfs_source": null,
                        "image_uri": null,
                        "harness_pack_uri": null,
                        "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64},
                        "disk": {"max_gib": 1},
                        "ttl": null,
                        "env": {},
                        "workdir": null,
                        "harness_substrate": null,
                        "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        // 3. Wire the PooledBackend with a chunk store.
        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeFcBackend {
            payload: bytes.clone(),
            staging_root: tmp.path().join("fc-snaps"),
        });
        let materialize_dir = tmp.path().join("materialized");
        let pooled = PooledBackend::new(inner).with_chunk_store(cs.clone(), materialize_dir);

        // 4. Take the snapshot. PooledBackend's wrap chunks
        //    memory.bin and patches manifest.json.
        let metadata = pooled.snapshot(SandboxId::new()).await.unwrap();
        let dest = pooled.snapshot_path_for(metadata.id);
        let manifest_ref = metadata
            .memory_manifest
            .expect("PooledBackend must populate memory_manifest");

        // 5. manifest.json now carries the same ref under
        //    `memory_manifest`.
        let mj: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(dest.join("manifest.json")).await.unwrap())
                .unwrap();
        let patched_ref: engram_core::types::manifest::ManifestRef =
            serde_json::from_value(mj["memory_manifest"].clone())
                .expect("memory_manifest field must round-trip");
        assert_eq!(patched_ref, manifest_ref);

        // 6. The chunk store materializes the manifest back to the
        //    exact bytes.
        let manifest = cs.get_manifest(manifest_ref).await.unwrap();
        assert!(matches!(manifest.kind, ManifestKind::Memory));
        assert_eq!(manifest.total_bytes, bytes.len() as u64);
        let recovered = tmp.path().join("recovered.bin");
        cs.materialize_to_file(&manifest, &recovered).await.unwrap();
        let recovered_bytes = tokio::fs::read(&recovered).await.unwrap();
        assert_eq!(recovered_bytes, bytes, "memory bytes round-trip");
    }

    /// ADR 0014: PooledBackend's snapshot wrap uploads state.bin +
    /// sidecar.json to BlobStorage and stamps the metadata with the
    /// portable blob keys. Mirrors the chunked-memory test pattern
    /// above but verifies the new state/sidecar upload step rather
    /// than the existing memory-chunk step.
    #[tokio::test]
    async fn snapshot_uploads_state_and_sidecar_to_blob_storage() {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // FakeFcBackend writes the three artifacts (memory.bin,
        // state.bin, manifest.json) into its staging dir, mirroring
        // what real FC `create_snapshot` produces.
        #[derive(Clone)]
        struct FakeFcBackend {
            payload: Vec<u8>,
            staging_root: PathBuf,
        }
        impl FakeFcBackend {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for FakeFcBackend {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"fake-state-bin-bytes")
                    .await
                    .unwrap();
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob.clone());
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeFcBackend {
            payload,
            staging_root: tmp.path().join("fc-snaps"),
        });
        let pooled = PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("mat"));

        let source_sandbox = SandboxId::new();
        let metadata = pooled.snapshot(source_sandbox).await.unwrap();

        // Portable fields populated.
        assert_eq!(
            metadata.source_sandbox_id,
            Some(source_sandbox),
            "PooledBackend must stamp source_sandbox_id",
        );
        let state_key = metadata
            .state_blob_key
            .as_ref()
            .expect("state_blob_key must be set after upload");
        let sidecar_key = metadata
            .sidecar_blob_key
            .as_ref()
            .expect("sidecar_blob_key must be set after upload");
        assert!(state_key.contains(&metadata.id.to_string()));
        assert!(sidecar_key.contains(&metadata.id.to_string()));

        // Blobs are actually in BlobStorage.
        let downloaded_state: bytes::Bytes = blob.get(state_key).await.unwrap();
        assert_eq!(&downloaded_state[..], b"fake-state-bin-bytes");
        let downloaded_sidecar: bytes::Bytes = blob.get(sidecar_key).await.unwrap();
        let sidecar: serde_json::Value = serde_json::from_slice(&downloaded_sidecar).unwrap();
        assert_eq!(sidecar["format"], "fc");
        assert_eq!(sidecar["spec"]["image"], "test:1");
    }

    /// ADR 0014: PooledBackend's restore wrap pulls state.bin +
    /// sidecar from BlobStorage when the local files are missing
    /// (cross-host restore case). Validated by deleting the local
    /// staging files between snapshot and restore — restore must
    /// rebuild them before delegating to inner.
    #[tokio::test]
    async fn restore_materializes_missing_state_and_sidecar_from_blob_storage() {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // Inner backend that lets us drive snapshot then asserts on
        // restore that state.bin + manifest.json are present on disk
        // by the time it's invoked.
        struct AssertingInner {
            staging_root: PathBuf,
            payload: Vec<u8>,
            saw_state: parking_lot::Mutex<Option<Vec<u8>>>,
            saw_sidecar: parking_lot::Mutex<Option<Vec<u8>>>,
        }
        impl AssertingInner {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for AssertingInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"upload-me-state")
                    .await
                    .unwrap();
                let sidecar = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "t", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&sidecar).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "t".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, meta: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                // Capture what was on disk at the moment inner.restore ran.
                let dest = self.dir_for(meta.id);
                let state = tokio::fs::read(dest.join("state.bin"))
                    .await
                    .expect("state.bin must be materialised before inner.restore");
                let sidecar = tokio::fs::read(dest.join("manifest.json"))
                    .await
                    .expect("manifest.json must be materialised before inner.restore");
                *self.saw_state.lock() = Some(state);
                *self.saw_sidecar.lock() = Some(sidecar);
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob.clone());
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let inner = Arc::new(AssertingInner {
            staging_root: tmp.path().join("fc-snaps"),
            payload: payload.clone(),
            saw_state: parking_lot::Mutex::new(None),
            saw_sidecar: parking_lot::Mutex::new(None),
        });
        let inner_dyn: Arc<dyn SandboxBackend> = inner.clone();
        let pooled = PooledBackend::new(inner_dyn).with_chunk_store(cs, tmp.path().join("mat"));

        // Snapshot — uploads state + sidecar to BlobStorage.
        let metadata = pooled.snapshot(SandboxId::new()).await.unwrap();
        let staging = inner.dir_for(metadata.id);

        // Simulate cross-host: delete the local staging files so
        // restore has nothing to read from disk. ADR 0039: the capture
        // already removed memory.bin after chunking it — assert that
        // (regression guard for the 61G leak fix), then restore
        // re-materializes it from the chunked manifest.
        assert!(
            !staging.join("memory.bin").exists(),
            "ADR 0039: Full capture must remove the local memory.bin after chunking",
        );
        tokio::fs::remove_file(staging.join("state.bin"))
            .await
            .unwrap();
        tokio::fs::remove_file(staging.join("manifest.json"))
            .await
            .unwrap();

        // Restore — wrap must re-download state.bin + sidecar
        // before delegating to inner.
        pooled.restore(metadata).await.unwrap();

        let seen_state = inner.saw_state.lock().clone().expect("inner.restore ran");
        assert_eq!(&seen_state[..], b"upload-me-state");
        let seen_sidecar = inner.saw_sidecar.lock().clone().expect("inner.restore ran");
        let parsed: serde_json::Value = serde_json::from_slice(&seen_sidecar).unwrap();
        assert_eq!(parsed["format"], "fc");
    }

    /// D4 (2026-07-17 corruption path, session 03e6535e): a chunked
    /// snapshot whose `disk_manifest` is None takes no NBD attach and no
    /// sidecar patch, so FC would reopen the capture-time LITERAL
    /// `/dev/nbdN` still named in the sidecar — a dead or FOREIGN device on
    /// the receiving host. On a host that runs the NBD data plane, the
    /// resume must REFUSE rather than boot onto it. (Runs on macOS: the
    /// guard sits at the platform-neutral `!took_nbd_path` join.)
    #[tokio::test]
    async fn resume_refuses_a_stale_literal_nbd_rootfs_when_no_attach_happened() {
        use engram_chunk_store::{ChunkCache, ChunkCacheConfig, ChunkStore};
        use engram_storage_local::LocalBlobStorage;
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempfile::tempdir().unwrap();

        // Inner whose snapshot writes a sidecar naming a literal /dev/nbd7
        // rootfs with disk_manifest=None (the corruption shape), and whose
        // `restore` must never be reached.
        struct LiteralNbdSidecarInner {
            staging_root: PathBuf,
            restored: Arc<AtomicBool>,
        }
        impl LiteralNbdSidecarInner {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for LiteralNbdSidecarInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), b"mem")
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"state")
                    .await
                    .unwrap();
                // The load-bearing bit: a sidecar that names a literal NBD
                // device as the rootfs source.
                let sidecar = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "t", "rootfs_source": "/dev/nbd7", "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&sidecar).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 3,
                    created_at: chrono::Utc::now(),
                    image_version: "t".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                self.restored.store(true, Ordering::SeqCst);
                Err(SandboxError::InvalidSpec(
                    "inner.restore must not be reached".into(),
                ))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob);
        let cache = ChunkCache::new(ChunkCacheConfig::new(tmp.path().join("cache")));
        // A fake `/dev/nbd7` path — the allocator only validates the string,
        // never opens the device (the guard fires before any slot claim).
        let pool =
            crate::disk_daemon::NbdSlotAllocator::from_paths(vec![PathBuf::from("/dev/nbd7")])
                .unwrap();
        let restored = Arc::new(AtomicBool::new(false));
        let inner: Arc<dyn SandboxBackend> = Arc::new(LiteralNbdSidecarInner {
            staging_root: tmp.path().join("fc-snaps"),
            restored: restored.clone(),
        });
        // The full NBD-data-plane triple → `host_runs_nbd_data_plane()` true.
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs, tmp.path().join("mat"))
            .with_chunk_cache(cache)
            .with_nbd_pool(pool);

        let md = pooled.snapshot(SandboxId::new()).await.unwrap();
        assert!(md.disk_manifest.is_none(), "fixture precondition");

        let err = pooled.restore(md).await.expect_err("restore must refuse");
        assert!(
            matches!(err, SandboxError::Snapshot(_)),
            "expected a Snapshot refusal, got {err:?}",
        );
        assert!(
            format!("{err}").contains("/dev/nbd7"),
            "the refusal names the stale literal device: {err}",
        );
        assert!(
            !restored.load(Ordering::SeqCst),
            "inner.restore must never run for a refused resume",
        );
    }

    /// D5 (2026-07-17 corruption path, session 03e6535e): a sandbox with an
    /// NBD-backed rootfs but NO `nbd_sandboxes` entry is a post-pod-roll
    /// survivor whose disk server is gone. Snapshotting it would silently
    /// skip the disk drain and record `disk_manifest=None` — dropping the
    /// session's acked disk writes. The capture must REFUSE. Linux-only:
    /// the guard + `nbd_sandboxes` are `cfg(target_os = "linux")`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn capture_refuses_an_untracked_nbd_rootfs_survivor() {
        use engram_chunk_store::{ChunkCache, ChunkCacheConfig, ChunkStore};
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // Inner reporting an NBD-backed rootfs device (the survivor's live
        // spec). `snapshot` is never reached — the guard fires first.
        struct NbdRootfsInner;
        #[async_trait]
        impl SandboxBackend for NbdRootfsInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn supports_diff_checkpoints(&self) -> bool {
                true
            }
            fn rootfs_device(&self, _id: SandboxId) -> Option<PathBuf> {
                Some(PathBuf::from("/dev/nbd7"))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                panic!("inner.snapshot must not be reached — the guard fires first")
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                std::env::temp_dir().join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob);
        let cache = ChunkCache::new(ChunkCacheConfig::new(tmp.path().join("cache")));
        let pool =
            crate::disk_daemon::NbdSlotAllocator::from_paths(vec![PathBuf::from("/dev/nbd7")])
                .unwrap();
        let pooled = PooledBackend::new(Arc::new(NbdRootfsInner))
            .with_chunk_store(cs, tmp.path().join("mat"))
            .with_chunk_cache(cache)
            .with_nbd_pool(pool)
            .with_checkpoint_dir(tmp.path().join("ckpt"));

        let sandbox_id = SandboxId::new();
        pooled.session_bindings.insert(sandbox_id, SessionId::new());
        // Deliberately do NOT insert into `nbd_sandboxes` — the survivor
        // whose disk server died with the rolled pod.

        let err = pooled
            .snapshot(sandbox_id)
            .await
            .expect_err("capture must refuse an untracked NBD-rootfs sandbox");
        assert!(
            matches!(err, SandboxError::Snapshot(_)),
            "expected a Snapshot refusal, got {err:?}",
        );
        assert!(
            format!("{err}").contains("nbd_sandboxes"),
            "the refusal explains the missing NBD tracking: {err}",
        );
    }

    /// Snapshot wrap is a no-op when no chunk store is wired —
    /// metadata + manifest.json stay exactly as the inner backend
    /// produced them. Guards against accidental coupling between
    /// `with_chunk_store` and the snapshot path.
    #[tokio::test]
    async fn snapshot_no_chunk_store_passes_through_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir));
        let pooled = PooledBackend::new(inner);

        let _id = pooled.create(live_spec("warm-test")).await.unwrap();
        // ProcessBackend's snapshot writes the directory structure
        // without a memory.bin (no guest RAM concept). The wrap
        // skips chunking silently and metadata.memory_manifest
        // stays None.
        let metadata = pooled.snapshot(_id).await.expect("ProcessBackend snapshot");
        assert!(
            metadata.memory_manifest.is_none(),
            "no chunk_store wired must leave memory_manifest unset",
        );
    }

    /// ADR 0007 Phase 5: when `restore()` is called on a snapshot
    /// dir whose `memory.bin` is missing locally but whose sidecar
    /// JSON carries a `memory_manifest`, PooledBackend materialises
    /// the file from chunks before delegating to the inner backend.
    /// Validates by:
    ///   1. Build a chunk store + put a small memory manifest
    ///   2. Stage a snapshot dir with state.bin + manifest.json
    ///      (memory_manifest set, but NO memory.bin)
    ///   3. Call PooledBackend::restore
    ///   4. Assert memory.bin now exists at byte-equal content
    #[tokio::test]
    async fn restore_materializes_missing_memory_bin_from_chunks() {
        use engram_chunk_store::{ChunkStore, ManifestKind};
        use engram_core::types::manifest::ManifestRef;

        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // Plant a synthetic memory.bin (1 MiB of non-zero pattern)
        // and chunk it into the store. `chunk_file` skips all-zero
        // blocks; we want the test to exercise non-empty chunks.
        let src_path = tmp.path().join("source-memory.bin");
        let bytes: Vec<u8> = (0..(1024 * 1024)).map(|i| ((i % 251) + 1) as u8).collect();
        tokio::fs::write(&src_path, &bytes).await.unwrap();
        let manifest = cs
            .chunk_file(&src_path, ManifestKind::Memory, None)
            .await
            .unwrap();
        let manifest_ref = ManifestRef::new();
        cs.put_manifest(manifest_ref, &manifest).await.unwrap();

        // Inner backend: records the metadata it was called with
        // so we can assert PooledBackend's wrap fired the
        // materialize step BEFORE inner.restore. Owns a staging
        // root that snapshot_path_for derives from — the wrap
        // looks the path up from there to stage memory.bin.
        struct CapturingInner {
            captured: parking_lot::Mutex<Option<SnapshotMetadata>>,
            staging_root: PathBuf,
        }
        #[async_trait]
        impl SandboxBackend for CapturingInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
            async fn restore(&self, m: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                *self.captured.lock() = Some(m);
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner = Arc::new(CapturingInner {
            captured: parking_lot::Mutex::new(None),
            staging_root: tmp.path().join("inner-snaps"),
        });

        // Allocate the snapshot_id up front so we can stage its
        // dir at the path snapshot_path_for will return.
        let snapshot_id = engram_core::SnapshotId::new();
        let snap_dir = inner.snapshot_path_for(snapshot_id);
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        tokio::fs::write(snap_dir.join("state.bin"), b"state-placeholder")
            .await
            .unwrap();
        let manifest_json = serde_json::json!({
            "sandbox_id": uuid::Uuid::new_v4(),
            "created_at": chrono::Utc::now(),
            "spec": {
                "image": "test:1",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 64},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "fc",
            "memory_manifest": manifest_ref,
        });
        tokio::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest_json).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            !snap_dir.join("memory.bin").exists(),
            "precondition: memory.bin must be absent before restore"
        );

        let pooled = PooledBackend::new(inner.clone())
            .with_chunk_store(cs.clone(), tmp.path().join("materialized"));

        // Restore — should materialise memory.bin then delegate.
        let metadata = SnapshotMetadata {
            id: snapshot_id,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: Some(manifest_ref),
            base_memory_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
        };
        pooled.restore(metadata.clone()).await.unwrap();

        // Inner backend saw the metadata; memory.bin is now
        // present and byte-equal to the original.
        assert_eq!(
            inner.captured.lock().as_ref().map(|m| m.id),
            Some(snapshot_id)
        );
        let recovered = tokio::fs::read(snap_dir.join("memory.bin"))
            .await
            .expect("memory.bin must exist post-restore");
        assert_eq!(recovered, bytes, "materialized bytes round-trip");
    }

    /// Cross-host trace replay: when a snapshot carries a
    /// `trace_host_hint` on its sidecar JSON, the FC restore path
    /// passes that as `--prefault-trace <host>` to the UFFD
    /// handler so the recorded trace replays on a different host.
    /// Locked at the wire-shape level here — we assert the field
    /// round-trips through serde so a refactor can't accidentally
    /// drop it. (End-to-end replay needs a live VM; that's the
    /// follow-up.)
    #[tokio::test]
    async fn snapshot_skips_materialize_when_memory_bin_already_present() {
        // Negative case: memory.bin already exists locally, so
        // the materialize branch must be a no-op. Guards against a
        // refactor that accidentally truncates the file.
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);

        let snap_dir = tmp.path().join("snap");
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        // Pre-existing memory.bin with sentinel bytes.
        let original = b"original-memory-bytes";
        tokio::fs::write(snap_dir.join("memory.bin"), original)
            .await
            .unwrap();
        // Manifest.json claims a different memory_manifest. The
        // materialize branch MUST skip because the local file is
        // present.
        let manifest_json = serde_json::json!({
            "sandbox_id": uuid::Uuid::new_v4(),
            "created_at": chrono::Utc::now(),
            "spec": {
                "image": "test:1",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 64},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "fc",
            "memory_manifest": {"manifest_id": uuid::Uuid::new_v4(), "version": 1},
        });
        tokio::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await
        .unwrap();

        // Phase 6: the test backend now owns where snap_dir lives.
        // Use a CapturingInner that points snapshot_path_for at the
        // dir we staged, so PooledBackend's wrap finds the
        // already-existing memory.bin and the materialize branch
        // short-circuits.
        struct StagedInner {
            snap_dir: PathBuf,
        }
        #[async_trait]
        impl SandboxBackend for StagedInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                self.snap_dir.clone()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                // Inner restore is irrelevant for this test — we
                // only assert the materialize branch was a no-op.
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
        }
        let inner: Arc<dyn SandboxBackend> = Arc::new(StagedInner {
            snap_dir: snap_dir.clone(),
        });
        let pooled =
            PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialized"));
        let metadata = SnapshotMetadata {
            id: engram_core::SnapshotId::new(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
            base_memory_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
        };
        let _ = pooled.restore(metadata).await;
        let after = tokio::fs::read(snap_dir.join("memory.bin")).await.unwrap();
        assert_eq!(after, original, "memory.bin must not be overwritten");
    }

    /// Cache wiring: when a `ChunkCache` is provided,
    /// `materialize_chunked_rootfs` routes chunk reads through it.
    /// We verify by materializing twice — once to populate, once
    /// against a "broken" store (drop the chunks underneath). The
    /// second materialize succeeds iff reads served from the cache.
    #[tokio::test]
    async fn materialize_chunked_rootfs_uses_chunk_cache_when_present() {
        use engram_chunk_store::{
            cache::ChunkCacheConfig, ChunkCache, ChunkStore, ManifestKind, ManifestRef,
        };

        // A near-full dev/CI disk trips the cache's default free-space
        // floor and evicts the chunks the first `materialize_chunked_rootfs`
        // warms below before the second (store-deleted) call can read them
        // back. Nextest runs each test in its own process, so this env
        // override is safe (see two_host_drain_wave.rs / migration_source.rs).
        std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");

        let tmp = tempfile::tempdir().unwrap();
        let blob_root = tmp.path().join("blob");
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
        );
        let cs = ChunkStore::new(blob);
        let cache = ChunkCache::new(ChunkCacheConfig::new(tmp.path().join("chunk-cache")));

        // Plant a small source + manifest.
        let src = tmp.path().join("source.ext4");
        let bytes: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&src, &bytes).await.unwrap();
        let manifest = cs.chunk_file(&src, ManifestKind::Disk, None).await.unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        let bundle = ImageBundle {
            schema_version: 1,
            disk_manifest: mref,
            bootstrap_disk_available: false,
        };
        let materialize_dir = tmp.path().join("materialized");
        let lock = Mutex::new(());

        // First materialize warms the cache.
        let path1 = materialize_chunked_rootfs(
            &cs,
            Some(&cache),
            &materialize_dir,
            "img:cached",
            bundle.disk_manifest,
            &lock,
        )
        .await
        .unwrap();
        let restored = tokio::fs::read(&path1).await.unwrap();
        assert_eq!(restored, bytes, "first materialize must reproduce bytes");

        // Delete the materialized file and the underlying blob store —
        // the cache should still have everything we need.
        tokio::fs::remove_file(&path1).await.unwrap();
        let chunks_dir = blob_root.join("chunks");
        tokio::fs::remove_dir_all(&chunks_dir).await.unwrap();

        // Second materialize would fail if it hit the store; succeeds
        // when reads are served from the cache.
        let path2 = materialize_chunked_rootfs(
            &cs,
            Some(&cache),
            &materialize_dir,
            "img:cached",
            bundle.disk_manifest,
            &lock,
        )
        .await
        .expect("cache should serve chunks after store is gone");
        let restored2 = tokio::fs::read(&path2).await.unwrap();
        assert_eq!(restored2, bytes, "cached materialize must reproduce bytes");
    }

    /// Tier 1 regression guard for the ADR 0007 chunked-lifecycle
    /// rollout. Exercises the full `PooledBackend::create()` flow with
    /// a real `ImageCache` + `ChunkStore` wired, asserting that an
    /// `image_uri`-driven spec lands a chunked-materialized path on
    /// the inner backend's spec — not the OCI-pulled `rootfs.ext4`.
    ///
    /// Covered ground:
    /// - `ImageCache.ensure_image()` returns a `CachedImage.bundle`
    ///   when the on-disk artifact carries a `bundle.json`.
    /// - `PooledBackend.resolve_rootfs()` dispatches to the chunk
    ///   store rather than the cached file.
    /// - The materialized file lands at the deterministic content-
    ///   addressed path, and its bytes match the chunked source.
    /// - The inner backend's `create()` receives `spec.rootfs_source`
    ///   pointing at that materialized path.
    #[tokio::test]
    async fn create_with_image_uri_resolves_chunked_path_on_inner() {
        use crate::image_cache::ImageCache;
        use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
        use engram_core::traits::SandboxBackend;
        use engram_core::types::sandbox::{
            AgentSpec, CpuLimit, DiskLimit, ExecRequest, ExecStream, MemoryLimit,
        };
        use engram_core::types::snapshot::SnapshotMetadata;
        use engram_core::SandboxId;
        use parking_lot::Mutex as PlMutex;
        use std::path::PathBuf;

        // 1. Shared BlobStorage + ChunkStore, mirroring `--mode=all`.
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // 2. Plant a fake rootfs in the chunk store + produce a
        //    manifest. Smaller than the materialize round-trip test
        //    because we just need the wiring to flow, not multi-
        //    chunk content addressing — that's covered above.
        let source = tmp.path().join("source.ext4");
        let bytes: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&source, &bytes).await.unwrap();
        let manifest = cs
            .chunk_file(&source, ManifestKind::Disk, None)
            .await
            .unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        // 3. Stand up an ImageCache rooted under tmp, then plant a
        //    fake "OCI-pulled" artifact + a uri-map entry pointing at
        //    it. ensure_image will short-circuit via the cache-hit
        //    path without attempting a real OCI fetch.
        let cache_root = tmp.path().join("oci-cache");
        let oci = engram_oci::OciClient::new(Arc::new(engram_oci::AnonymousResolver));
        let cache = ImageCache::open(cache_root.clone(), oci).await.unwrap();
        let digest = "sha256:deadbeefcafefeedfacefeed";
        let image_dir = cache.image_dir_for_test(digest);
        tokio::fs::create_dir_all(&image_dir).await.unwrap();
        tokio::fs::write(
            image_dir.join("manifest.toml"),
            b"name = \"chunked-test\"\n",
        )
        .await
        .unwrap();
        // Drop a bytes-equal `rootfs.ext4` alongside so the cache-hit
        // existence check passes; the chunked path supersedes it.
        tokio::fs::write(image_dir.join("rootfs.ext4"), &bytes)
            .await
            .unwrap();
        let bundle_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "disk_manifest": mref,
        }))
        .unwrap();
        tokio::fs::write(image_dir.join("bundle.json"), bundle_bytes)
            .await
            .unwrap();
        cache.prime_image_for_test("test:1", digest);

        // 4. Capturing inner backend — records the spec it receives.
        struct Capturing {
            captured: PlMutex<Option<SandboxSpec>>,
        }
        #[async_trait]
        impl SandboxBackend for Capturing {
            async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
                *self.captured.lock() = Some(spec);
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                _id: SandboxId,
                _cmd: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                std::env::temp_dir()
                    .join("engram-test-snaps")
                    .join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(
                &self,
                _id: SandboxId,
                _agent: AgentSpec,
            ) -> Result<(), SandboxError> {
                Ok(())
            }
        }
        let captured = Arc::new(Capturing {
            captured: PlMutex::new(None),
        });
        let inner: Arc<dyn SandboxBackend> = captured.clone();

        // 5. PooledBackend wires both image cache + chunk store +
        //    materialize dir. We only care about the single create
        //    path here.
        let materialize_dir = tmp.path().join("materialized");
        let pooled = PooledBackend::new(inner)
            .with_image_cache(cache)
            .with_chunk_store(cs.clone(), materialize_dir.clone());

        // 6. Drive a session-create with image_uri set.
        let mut spec = SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: Some("test:1".into()),
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        };
        spec.image_uri = Some("test:1".into());
        let _id = pooled.create(spec).await.unwrap();

        // 7. The inner backend must have received the chunked
        //    materialized path, NOT the OCI-pulled rootfs.ext4.
        let received = captured.captured.lock().clone().expect("inner not called");
        let received_rootfs = received.rootfs_source.expect("rootfs_source not set");
        let expected =
            materialize_dir.join(format!("{}-v{}.ext4", mref.manifest_id, mref.version,));
        assert_eq!(
            received_rootfs, expected,
            "PooledBackend must route chunked manifests through materialize_dir, not the OCI rootfs.ext4",
        );

        // 8. The materialized file exists and is byte-equal.
        let materialized = tokio::fs::read(&expected).await.unwrap();
        assert_eq!(materialized, bytes, "materialized file bytes mismatch");
    }

    // ──────────────────────────────────────────────────────────────
    // ADR 0014 follow-up (prod 2026-05-20 session 5c8d0ce5):
    // PooledBackend MUST forward start_shell / guest_endpoints to its
    // inner backend. The SandboxBackend trait has default impls for
    // these (Ok(7681) / None respectively) that exist for backends
    // without that capability (process, VZ-without-netns). When
    // PooledBackend wraps a FirecrackerBackend that DOES implement
    // them, NOT forwarding silently routes through the trait defaults
    // and the real FC capability never fires — observed in prod: zero
    // start_shell logs on the host-agent despite a completed
    // proxy_shell GRPC call, exactly because PooledBackend.start_shell
    // was using the trait default.
    // ──────────────────────────────────────────────────────────────

    mod inner_forwarding_tests {
        use super::*;
        use parking_lot::Mutex;

        /// SandboxBackend mock that RECORDS every call to the methods
        /// PooledBackend is supposed to forward. We assert against
        /// the captured counters/values after invoking PooledBackend's
        /// surface.
        struct SpyInner {
            start_shell_calls: Mutex<Vec<SandboxId>>,
            start_browser_calls: Mutex<Vec<SandboxId>>,
            stop_browser_calls: Mutex<Vec<SandboxId>>,
            start_ide_calls: Mutex<Vec<SandboxId>>,
            stop_ide_calls: Mutex<Vec<SandboxId>>,
            guest_endpoints_calls: Mutex<Vec<SandboxId>>,
            /// Non-default response values so we can verify the
            /// forward returned the inner's value, not the trait
            /// default.
            shell_port: u16,
            browser_port: u16,
            ide_port: u16,
            guest_endpoints_value: Option<GuestEndpoints>,
        }
        impl SpyInner {
            fn new() -> Self {
                Self {
                    start_shell_calls: Mutex::new(Vec::new()),
                    start_browser_calls: Mutex::new(Vec::new()),
                    stop_browser_calls: Mutex::new(Vec::new()),
                    start_ide_calls: Mutex::new(Vec::new()),
                    stop_ide_calls: Mutex::new(Vec::new()),
                    guest_endpoints_calls: Mutex::new(Vec::new()),
                    // Pick non-default values so a "trait default ran
                    // instead of our override" failure shows up as a
                    // value mismatch, not just a counter mismatch.
                    shell_port: 31337,
                    // Non-default browser port (the trait default is 5900);
                    // a fall-through would return 5900 with a zero counter.
                    browser_port: 45900,
                    // Non-default ide port (the trait default is 13337);
                    // a fall-through would return 13337 with a zero counter.
                    ide_port: 43337,
                    guest_endpoints_value: Some(GuestEndpoints {
                        egress_identity: "10.200.0.42".parse().unwrap(),
                        dial_ip: "10.200.0.2".parse().unwrap(),
                        netns: Some("engr-vm-spytest".into()),
                        vsock_uds: None,
                    }),
                }
            }
        }
        #[async_trait]
        impl SandboxBackend for SpyInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                PathBuf::new()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_shell(&self, id: SandboxId) -> Result<u16, SandboxError> {
                self.start_shell_calls.lock().push(id);
                Ok(self.shell_port)
            }
            async fn start_browser(
                &self,
                id: SandboxId,
            ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
                self.start_browser_calls.lock().push(id);
                Ok(engram_core::traits::sandbox::BrowserStart {
                    port: self.browser_port,
                    warning: None,
                })
            }
            async fn stop_browser(&self, id: SandboxId) -> Result<(), SandboxError> {
                self.stop_browser_calls.lock().push(id);
                Ok(())
            }
            async fn start_ide(&self, id: SandboxId) -> Result<u16, SandboxError> {
                self.start_ide_calls.lock().push(id);
                Ok(self.ide_port)
            }
            async fn stop_ide(&self, id: SandboxId) -> Result<(), SandboxError> {
                self.stop_ide_calls.lock().push(id);
                Ok(())
            }
            async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
                self.guest_endpoints_calls.lock().push(id);
                self.guest_endpoints_value.clone()
            }
        }

        /// Regression guard: PooledBackend.start_shell MUST forward
        /// to its inner backend's start_shell. Without forwarding,
        /// the trait default returns Ok(7681) without ever touching
        /// the inner — host-agent's proxy_shell then dials port
        /// 7681 blind even when the FC backend would have correctly
        /// minted the connection (warm restore in a netns, lazy
        /// ttyd spawn, etc.). Prod 2026-05-20 session 5c8d0ce5 was
        /// exactly this: zero `start_shell:` log lines on the
        /// host-agent despite a successful proxy_shell GRPC roundtrip.
        #[tokio::test]
        async fn pooled_backend_forwards_start_shell_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let port = pooled.start_shell(id).await.unwrap();

            // Inner.start_shell received the sandbox id.
            let calls = inner.start_shell_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.start_shell must forward to inner; got {} calls",
                calls.len(),
            );
            // The returned port is the inner's value, NOT the trait
            // default (7681). If we'd accidentally fallen through to
            // the default, the value would be 7681 and the inner's
            // counter would still be 0.
            assert_eq!(
                port, inner.shell_port,
                "must return inner's port (proves the forward, not the default)",
            );
        }

        /// ADR 0065 regression guard: PooledBackend.start_browser MUST
        /// forward to its inner backend. The trait default returns
        /// Ok(5900) WITHOUT touching the inner, so a deleted/regressed
        /// delegate would hand `proxy_vnc` a port (5900) that nothing
        /// started — exactly the start_shell failure class this module
        /// memorializes, applied to the VNC path.
        #[tokio::test]
        async fn pooled_backend_forwards_start_browser_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let start = pooled.start_browser(id).await.unwrap();

            // Inner.start_browser received the sandbox id.
            let calls = inner.start_browser_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.start_browser must forward to inner; got {} calls",
                calls.len(),
            );
            // The returned port is the inner's value, NOT the trait
            // default (5900). A fall-through would return 5900 and leave
            // the inner's counter at 0.
            assert_eq!(
                start.port, inner.browser_port,
                "must return inner's port (proves the forward, not the 5900 default)",
            );
        }

        /// ADR 0065 regression guard: PooledBackend.stop_browser MUST
        /// forward to its inner backend. The trait default is a no-op, so
        /// a deleted delegate would silently never tear down the in-guest
        /// browser stack (leaking Xvfb/chromium/x11vnc) while reporting
        /// success — and the inner's counter would stay at 0.
        #[tokio::test]
        async fn pooled_backend_forwards_stop_browser_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            pooled.stop_browser(id).await.unwrap();

            let calls = inner.stop_browser_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.stop_browser must forward to inner; got {} calls",
                calls.len(),
            );
        }

        /// ADR 0085 regression guard: PooledBackend.start_ide MUST forward
        /// to its inner backend. The trait default returns Ok(13337)
        /// WITHOUT touching the inner, so a deleted/regressed delegate
        /// would hand the orchestrator a port nothing started — the same
        /// failure class as start_shell/start_browser above.
        #[tokio::test]
        async fn pooled_backend_forwards_start_ide_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let port = pooled.start_ide(id).await.unwrap();

            let calls = inner.start_ide_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.start_ide must forward to inner; got {} calls",
                calls.len(),
            );
            // The returned port is the inner's value, NOT the trait
            // default (13337). A fall-through would return 13337 and leave
            // the inner's counter at 0.
            assert_eq!(
                port, inner.ide_port,
                "must return inner's port (proves the forward, not the 13337 default)",
            );
        }

        /// ADR 0085 regression guard: PooledBackend.stop_ide MUST forward
        /// to its inner backend. The trait default is a no-op, so a deleted
        /// delegate would silently never tear down the in-guest code-server
        /// while reporting success — and the inner's counter would stay at 0.
        #[tokio::test]
        async fn pooled_backend_forwards_stop_ide_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            pooled.stop_ide(id).await.unwrap();

            let calls = inner.stop_ide_calls.lock().clone();
            assert_eq!(
                calls,
                vec![id],
                "PooledBackend.stop_ide must forward to inner; got {} calls",
                calls.len(),
            );
        }

        /// Regression guard, consolidated (issue #541): PooledBackend
        /// MUST forward guest_endpoints to its inner backend. This one
        /// method now carries what used to be three separate forwards
        /// (guest_ip / netns_name_for / vm_internal_ip) — asserting
        /// full value equality against a GuestEndpoints whose fields
        /// are all non-default (egress_identity != dial_ip, netns
        /// Some) means a fall-through to the trait's `None` default,
        /// or a forward that drops a field, both fail on value, not
        /// just call count.
        #[tokio::test]
        async fn pooled_backend_forwards_guest_endpoints_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let endpoints = pooled.guest_endpoints(id).await;

            let calls = inner.guest_endpoints_calls.lock().clone();
            assert_eq!(calls, vec![id], "must forward to inner");
            assert_eq!(
                endpoints,
                inner.guest_endpoints_value.clone(),
                "must return inner's value (proves the forward, not the default-None)",
            );
        }
    }

    // ──────────────────────────────────────────────────────────────
    // ADR 0045 C2 teleport: egress policy + DNS interception must be
    // re-established on the destination host before a teleported
    // session resumes (issue #240). The destination host-agent learns
    // the per-session policy via `notify_session_policy`; after that
    // call the local filtering proxy MUST resolve the guest IP to the
    // session's allow-list. If it doesn't, a teleported guest runs
    // unfiltered on the destination — exactly the regression #240
    // closes.
    // ──────────────────────────────────────────────────────────────
    mod teleport_egress_policy_tests {
        use super::*;
        use engram_core::types::egress::SessionEgressPolicy;
        use engram_core::types::image::SecretMode;
        use std::net::Ipv4Addr;

        /// Minimal inner backend — teleport policy re-application only
        /// touches PooledBackend's own `notify_session_policy` override
        /// (it registers against the wired egress proxy); the inner is
        /// never called.
        struct NoopInner;
        #[async_trait]
        impl SandboxBackend for NoopInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                PathBuf::new()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
        }

        /// Spawn a real `HostEgress` on an ephemeral loopback port with
        /// a self-generated local-disk CA. This is the same proxy the
        /// production host-agent stands up; we only need its registry.
        async fn spawn_test_egress() -> (HostEgress, tempfile::TempDir) {
            let dir = tempfile::tempdir().expect("tempdir");
            let source: Arc<dyn engram_egress_proxy::CaSource> = Arc::new(
                engram_egress_proxy::LocalDiskCaSource::new(dir.path().join("egress-ca")),
            );
            // Port 0 ⇒ OS-assigned ephemeral port (no fixed-port
            // collisions when the suite runs in parallel). DNS proxy
            // disabled (`None`): these tests exercise only the egress
            // registry, and a fixed DNS port would collide across the
            // parallel suite now that a bind failure is fatal (ADR 0083).
            let bind: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
            let egress = HostEgress::spawn(source, bind, None, None, None)
                .await
                .expect("spawn egress");
            (egress, dir)
        }

        fn policy_for(
            session_id: SessionId,
            sandbox_id: SandboxId,
            guest_ip: Ipv4Addr,
            allow: &[&str],
        ) -> SessionEgressPolicy {
            SessionEgressPolicy {
                session_id,
                sandbox_id,
                guest_ip,
                network_allow_hosts: allow.iter().map(|s| s.to_string()).collect(),
                network_allow_host_patterns: Vec::new(),
                allow_all: false,
                secrets: Vec::new(),
                injects: Vec::new(),
                observes: Vec::new(),
                secret_mode: SecretMode::Literal,
            }
        }

        /// The destination host-agent, on receiving the teleported
        /// session's policy, MUST register it with the local proxy so
        /// the guest IP resolves to the allow-list. Pre-#240 prod ran
        /// with the proxy globally disabled, so this path was a no-op
        /// ("no local egress proxy, binding recorded for Phase B
        /// publisher only") and a teleported guest had unfiltered
        /// egress. This test pins the now-mandatory behavior.
        #[tokio::test]
        async fn teleport_dest_reestablishes_egress_filter_before_resume() {
            let (egress, _dir) = spawn_test_egress().await;
            let registry = egress.registry.clone();

            let pooled = PooledBackend::new(Arc::new(NoopInner) as Arc<dyn SandboxBackend>)
                .with_egress(Arc::new(egress));

            let session_id = SessionId::new();
            let sandbox_id = SandboxId::new();
            let guest_ip = Ipv4Addr::new(10, 200, 0, 2);

            // Before the dest applies policy, the proxy knows nothing
            // about this guest IP — a connection would be refused.
            assert!(
                registry.lookup(guest_ip).is_none(),
                "pre-condition: dest proxy must not know the teleported guest yet",
            );

            // The dest receives the teleported session's egress policy
            // (the same call the coordinator makes on the destination
            // before finishing the resume).
            pooled
                .notify_session_policy(policy_for(
                    session_id,
                    sandbox_id,
                    guest_ip,
                    &["api.anthropic.com"],
                ))
                .await
                .expect("dest must accept and apply the teleported egress policy");

            // The filter is now LIVE on the destination: the guest IP
            // resolves to a session whose allow-list permits ONLY the
            // policy's host and refuses everything else (incl. a DNS-
            // exfil target). DNS interception keys off the same
            // `network_allow` HostList the resolver consults, so this
            // also proves DNS filtering is in force for the guest.
            let state = registry
                .lookup(guest_ip)
                .expect("dest proxy must resolve the teleported guest IP after policy apply");
            assert_eq!(state.session_id, session_id);
            assert!(
                state.network_allow.matches("api.anthropic.com"),
                "allow-listed host must be permitted post-teleport",
            );
            assert!(
                !state.network_allow.matches("evil.example.com"),
                "non-allow-listed host (exfil target) must be refused post-teleport",
            );
        }

        /// Idempotent re-application: a teleport may re-issue the
        /// policy (e.g. parachute fallback re-homes the session). The
        /// dest must update cleanly, never leaving a stale unfiltered
        /// binding or two conflicting entries for the same IP.
        #[tokio::test]
        async fn teleport_dest_policy_reapply_is_idempotent() {
            let (egress, _dir) = spawn_test_egress().await;
            let registry = egress.registry.clone();
            let pooled = PooledBackend::new(Arc::new(NoopInner) as Arc<dyn SandboxBackend>)
                .with_egress(Arc::new(egress));

            let session_id = SessionId::new();
            let sandbox_id = SandboxId::new();
            let guest_ip = Ipv4Addr::new(10, 200, 0, 2);

            pooled
                .notify_session_policy(policy_for(session_id, sandbox_id, guest_ip, &["a.example"]))
                .await
                .unwrap();
            // Re-home: same IP, tighter allow-list.
            pooled
                .notify_session_policy(policy_for(session_id, sandbox_id, guest_ip, &["b.example"]))
                .await
                .unwrap();

            let state = registry.lookup(guest_ip).expect("guest resolves");
            assert!(state.network_allow.matches("b.example"));
            assert!(
                !state.network_allow.matches("a.example"),
                "re-applied policy must replace the prior allow-list, not union it",
            );
        }
    }

    // ──────────────────────────────────────────────────────────────
    // ADR 0014 issue #1/#2: commit + abort snapshot lifecycle tests.
    // ──────────────────────────────────────────────────────────────

    mod snapshot_lifecycle_tests {
        use super::*;
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        /// Stripped-down FC-shaped inner that writes the three
        /// canonical files (memory.bin, state.bin, manifest.json) into
        /// a sandbox-id-keyed (NOT snapshot-id-keyed — we want to see
        /// PooledBackend driving the snapshot_id path) staging root.
        struct LifecycleInner {
            staging_root: PathBuf,
        }
        impl LifecycleInner {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for LifecycleInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), vec![0u8; 4096])
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"fake-state-bin")
                    .await
                    .unwrap();
                // Minimal but parseable sidecar (the post-snapshot
                // path validates JSON; we don't care about content).
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    base_memory_manifest: None,
                    migration_source: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
                    paused_at: None,
                    peer_hints: Vec::new(),
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        fn build_pooled(
            tmp: &tempfile::TempDir,
        ) -> (PooledBackend, Arc<dyn engram_core::traits::BlobStorage>) {
            let blob: Arc<dyn engram_core::traits::BlobStorage> =
                Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
            let cs = ChunkStore::new(blob.clone());
            let inner: Arc<dyn SandboxBackend> = Arc::new(LifecycleInner {
                staging_root: tmp.path().join("fc-snaps"),
            });
            let pooled = PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("mat"));
            (pooled, blob)
        }

        /// Sanity: snapshot() leaves the local dir on disk and the
        /// per-snapshot opaque blobs in BlobStorage. commit_snapshot
        /// clears the in-flight tracking *without* deleting them — they
        /// belong to the PG row now.
        #[tokio::test]
        async fn commit_snapshot_keeps_artifacts() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, blob) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            let metadata = pooled.snapshot(sandbox_id).await.unwrap();
            let dir = pooled.inner.snapshot_path_for(metadata.id);
            let state_key = engram_chunk_store::snapshot_blob::state_blob_key(metadata.id);
            let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(metadata.id);

            assert!(dir.exists(), "snapshot dir must exist after snapshot()");
            assert!(blob.exists(&state_key).await.unwrap());
            assert!(blob.exists(&sidecar_key).await.unwrap());

            pooled.commit_snapshot(sandbox_id).await.unwrap();

            // After commit: dir + blobs stay (PG-owned now); tracking
            // cleared.
            assert!(dir.exists(), "commit must NOT delete the snapshot dir");
            assert!(blob.exists(&state_key).await.unwrap());
            assert!(blob.exists(&sidecar_key).await.unwrap());
            assert!(!pooled.inflight_snapshots.contains_key(&sandbox_id));
        }

        /// ADR 0028 addendum: abort_snapshot tears down the LOCAL dir
        /// (host-disk hygiene — the 4 GiB-per-retry leak) but NO LONGER
        /// deletes the per-snapshot blobs. Those are governed by the
        /// snapshot-blob GC sweep now, pinned by the `snapshots` row;
        /// deleting them inline here was the engine of the recurring
        /// "recoverable=true but blobs gone" brick (a concurrent producer
        /// aborting another's already-recorded snapshot).
        #[tokio::test]
        async fn abort_snapshot_removes_dir_keeps_blobs() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, blob) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            let metadata = pooled.snapshot(sandbox_id).await.unwrap();
            let dir = pooled.inner.snapshot_path_for(metadata.id);
            let state_key = engram_chunk_store::snapshot_blob::state_blob_key(metadata.id);
            let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(metadata.id);

            pooled.abort_snapshot(sandbox_id).await.unwrap();

            assert!(!dir.exists(), "abort must rm -rf the local snapshot dir");
            assert!(
                blob.exists(&state_key).await.unwrap(),
                "abort must NOT delete the state.bin blob — it's GC-governed now",
            );
            assert!(
                blob.exists(&sidecar_key).await.unwrap(),
                "abort must NOT delete the sidecar.json blob — it's GC-governed now",
            );
            assert!(!pooled.inflight_snapshots.contains_key(&sandbox_id));
        }

        /// abort_snapshot must be idempotent — double-call after the
        /// in-flight tracking is gone is a no-op, not an error.
        #[tokio::test]
        async fn abort_snapshot_is_idempotent() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, _) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            // No snapshot taken yet → abort is a clean no-op.
            pooled.abort_snapshot(sandbox_id).await.unwrap();

            // Snapshot, abort, then abort again.
            pooled.snapshot(sandbox_id).await.unwrap();
            pooled.abort_snapshot(sandbox_id).await.unwrap();
            pooled
                .abort_snapshot(sandbox_id)
                .await
                .expect("double-abort must be a no-op");
        }

        /// The retry case: a second snapshot() for the same sandbox
        /// tears down the first attempt's LOCAL dir BEFORE producing the
        /// new one (host-disk hygiene — the 4 GiB-per-retry leak). ADR
        /// 0028 addendum: it no longer deletes the prior's durable blobs
        /// inline — those are reaped by the snapshot-blob GC sweep (the
        /// prior is orphaned, no row, so it's swept after grace).
        /// Deleting them inline was the engine of the recurring brick.
        #[tokio::test]
        async fn retry_snapshot_overwrites_prior_attempt() {
            let tmp = tempfile::tempdir().unwrap();
            let (pooled, blob) = build_pooled(&tmp);

            let sandbox_id = SandboxId::new();
            let first = pooled.snapshot(sandbox_id).await.unwrap();
            let first_dir = pooled.inner.snapshot_path_for(first.id);
            let first_state_key = engram_chunk_store::snapshot_blob::state_blob_key(first.id);
            assert!(first_dir.exists());
            assert!(blob.exists(&first_state_key).await.unwrap());

            // Simulate coord crash between snapshot and commit by NOT
            // calling either commit or abort — then retry.
            let second = pooled.snapshot(sandbox_id).await.unwrap();
            let second_dir = pooled.inner.snapshot_path_for(second.id);
            let second_state_key = engram_chunk_store::snapshot_blob::state_blob_key(second.id);

            // Second snapshot has a fresh id and exists on disk + blob.
            assert_ne!(first.id, second.id);
            assert!(second_dir.exists());
            assert!(blob.exists(&second_state_key).await.unwrap());

            // First attempt's LOCAL dir is GONE — overwrite-in-place.
            assert!(
                !first_dir.exists(),
                "retry must rm the prior attempt's local snapshot dir",
            );
            // ...but its BLOB stays — durable-blob deletion is the GC
            // sweep's job now (the prior is orphaned with no row, so the
            // sweep reaps it after grace). Deleting it inline here was
            // the brick race.
            assert!(
                blob.exists(&first_state_key).await.unwrap(),
                "retry must NOT delete the prior attempt's blob — GC-governed now",
            );
        }
    }

    /// Issue #200 regression: the snapshot/migration paths must not hold
    /// a `nbd_sandboxes` `DashMap` guard across an `.await`.
    ///
    /// Why this matters: a `DashMap` `Ref`/`RefMut` guard pins the
    /// shard's **synchronous** `RwLock`. A `.get()` read guard held
    /// across a multi-second `.await` (GCS upload ~32 s, paused-VM
    /// drains, device fsyncs) blocks every contending writer
    /// (`destroy`'s `remove`, `create`/`rehydrate`'s `insert`) hashing
    /// to the same shard: each parks its tokio worker thread for the
    /// whole await, and dashmap-6's writer-preference then blocks new
    /// readers on that shard too. With ≥ worker_threads collisions the
    /// runtime deadlocks and the host is declared dead.
    ///
    /// The bug is fundamentally "is a shard lock held at the await
    /// suspension point?" — a boolean property of the code shape, not a
    /// timing race. We probe it deterministically: `try_get_mut`
    /// returns [`TryResult::Locked`] iff a guard is currently
    /// outstanding on that key's shard (a read guard blocks the write
    /// lock attempt). So at the exact point where the production code
    /// `.await`s — i.e. where a concurrent writer would attempt its
    /// `insert`/`remove` — we assert the shard is FREE under the fixed
    /// clone-then-drop pattern and LOCKED under the old held-guard
    /// pattern. The second assertion proves the probe actually
    /// distinguishes the two shapes (no vacuous pass); the first is the
    /// regression guard.
    #[test]
    fn issue_200_no_dashmap_guard_held_across_await_in_snapshot_path() {
        use dashmap::try_result::TryResult;
        use dashmap::DashMap;
        use std::sync::Arc;

        let map: DashMap<SandboxId, Arc<()>> = DashMap::new();
        let id = SandboxId::new();
        map.insert(id, Arc::new(()));

        // Sanity: with no guard outstanding, the shard write lock is
        // free — `try_get_mut` succeeds.
        assert!(
            matches!(map.try_get_mut(&id), TryResult::Present(_)),
            "precondition: shard must be free with no guard held",
        );

        // FIX shape (the pattern this PR applies at every snapshot/
        // migration site): clone the Arc out of the guard, drop the
        // guard, THEN do the long work. `.map(..)` consumes the `Ref`,
        // so the shard lock is released before `backend` is used.
        let backend: Option<Arc<()>> = map.get(&id).map(|e| e.value().clone());
        // <-- production `.await` happens here, on the owned `backend`.
        // A concurrent `destroy`/`create` writer reaching the shard now
        // must NOT be blocked: the shard is free.
        assert!(
            !matches!(map.try_get_mut(&id), TryResult::Locked),
            "issue #200: a concurrent shard writer MUST NOT be blocked at \
             the await point — clone-then-drop must release the guard first",
        );
        // Use the owned Arc so the clone models the real flush/upload.
        assert!(backend.is_some());

        // BUG shape (what the listed sites did before this PR): hold the
        // `Ref` guard across the long work. This is the deadlock seed —
        // a concurrent writer IS blocked while the guard is live.
        {
            let _guard = map.get(&id).expect("entry present");
            // <-- the buggy code `.await`ed HERE while `_guard` is alive.
            // A concurrent writer's `insert`/`remove` would block on the
            // shard's sync write lock, parking its worker thread.
            assert!(
                matches!(map.try_get_mut(&id), TryResult::Locked),
                "bug model check: a held DashMap guard MUST block a \
                 concurrent shard writer — if this fails, `try_get_mut` is \
                 not detecting the held lock and the regression probe is \
                 invalid",
            );
            // guard drops here, releasing the shard.
        }
        // After the guard drops the shard is free again.
        assert!(matches!(map.try_get_mut(&id), TryResult::Present(_)));
    }

    // ---- Issue #221: snapshot_wait must be idempotent / retryable ----

    /// Minimal in-process `PooledBackend` for the `snapshot_wait`
    /// lifecycle tests — no chunk store / NBD wired (we drive
    /// `snapshot_waits` directly, bypassing the Linux-gated capture
    /// pipeline so the contract under test runs on every platform).
    fn lifecycle_backend() -> PooledBackend {
        let tmp = tempfile::tempdir().unwrap();
        let inner = Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        // keep `tmp` alive for the backend's lifetime
        std::mem::forget(tmp);
        PooledBackend::new(inner)
    }

    /// A `SnapshotMetadata` carrying a recognizable id for assertions.
    fn fake_metadata() -> SnapshotMetadata {
        SnapshotMetadata {
            id: engram_core::types::SnapshotId::new(),
            size_bytes: 4242,
            created_at: chrono::Utc::now(),
            image_version: "test-image:v1".into(),
            disk_manifest: None,
            memory_manifest: None,
            base_memory_manifest: None,
            migration_source: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: Vec::new(),
            paused_at: None,
            peer_hints: Vec::new(),
        }
    }

    /// Stand-in for `snapshot_begin`'s producer: spawn a delayed
    /// "upload" task and stash it as a retryable `SnapshotWait`, exactly
    /// as the real producer sites do. `delay` models the tens-of-seconds
    /// upload that outlives the coordinator's RPC deadline.
    fn begin_delayed_upload(
        pooled: &PooledBackend,
        id: SandboxId,
        meta: SnapshotMetadata,
        delay: std::time::Duration,
    ) {
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            Ok::<_, SandboxError>(meta)
        });
        pooled
            .snapshot_waits
            .insert(id, SnapshotWait::from_handle(handle));
    }

    /// THE regression for #221: a `snapshot_wait` whose await is dropped
    /// (the coordinator's gRPC deadline fires / its pod restarts
    /// mid-call) must NOT strip the result. A retry after the upload
    /// completes must still return the metadata.
    ///
    /// Before the fix `snapshot_wait` removed the map entry up front, so
    /// the dropped await orphaned the in-flight upload and the retry hit
    /// the permanent "no snapshot_begin in flight" error.
    #[tokio::test]
    async fn snapshot_wait_survives_a_cancelled_wait_and_a_retry_returns_metadata() {
        let pooled = lifecycle_backend();
        let id = SandboxId::new();
        let meta = fake_metadata();
        let want = meta.id;
        // Upload outlives the (very short) "deadline" below.
        begin_delayed_upload(&pooled, id, meta, std::time::Duration::from_millis(150));

        // First wait under a short timeout — simulates tonic dropping
        // the server-side handler future. The future is dropped here.
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            pooled.snapshot_wait(id),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the first wait must time out (be cancelled)"
        );

        // The slot must still be present after the cancellation.
        assert!(
            pooled.snapshot_waits.contains_key(&id),
            "cancelled wait must NOT remove the slot (issue #221 root cause)",
        );

        // Retry — the upload has since completed; we must get the
        // metadata back, not "no snapshot_begin in flight".
        let got = pooled
            .snapshot_wait(id)
            .await
            .expect("retry after a cancelled wait must return the metadata");
        assert_eq!(got.id, want);

        // A successful consume reclaims the slot.
        assert!(
            !pooled.snapshot_waits.contains_key(&id),
            "successful consumption must remove the slot",
        );
    }

    /// At-least-once: two concurrent waiters (e.g. two coordinator
    /// replicas) must both resolve to the same metadata.
    #[tokio::test]
    async fn snapshot_wait_two_concurrent_waiters_both_resolve() {
        let pooled = Arc::new(lifecycle_backend());
        let id = SandboxId::new();
        let meta = fake_metadata();
        let want = meta.id;
        begin_delayed_upload(&pooled, id, meta, std::time::Duration::from_millis(30));

        let a = {
            let p = pooled.clone();
            tokio::spawn(async move { p.snapshot_wait(id).await })
        };
        let b = {
            let p = pooled.clone();
            tokio::spawn(async move { p.snapshot_wait(id).await })
        };
        let (ra, rb) = tokio::join!(a, b);
        assert_eq!(ra.unwrap().unwrap().id, want);
        assert_eq!(rb.unwrap().unwrap().id, want);
        assert!(
            !pooled.snapshot_waits.contains_key(&id),
            "slot reclaimed once consumed",
        );
    }

    /// Lifecycle: a fresh `SnapshotWait` insert supersedes an
    /// unconsumed prior and aborts its backing task (the existing
    /// abort-prior contract preserved across the type change).
    #[tokio::test]
    async fn snapshot_wait_supersession_aborts_the_prior() {
        let pooled = lifecycle_backend();
        let id = SandboxId::new();

        // A prior wait that would never finish on its own. We watch a
        // sentinel future racing the long sleep so we can prove the
        // abort actually fired (the abort is asynchronous — `abort()`
        // signals, the runtime cancels on the next poll).
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let never = tokio::spawn(async move {
            // tx drops (sending Err to rx) the instant this task is
            // cancelled OR completes — but the 1h sleep means only
            // cancellation can drop it within the test.
            let _drop_signals_cancellation = tx;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok::<_, SandboxError>(fake_metadata())
        });
        let prior = SnapshotWait::from_handle(never);
        pooled.snapshot_waits.insert(id, prior);

        // Supersede with a fast one (mirrors snapshot_begin's insert +
        // abort-prior path).
        let meta = fake_metadata();
        let want = meta.id;
        if let Some(p) = pooled.snapshot_waits.insert(
            id,
            SnapshotWait::from_handle(tokio::spawn(async move { Ok::<_, SandboxError>(meta) })),
        ) {
            p.abort.abort();
        }
        // The prior task is cancelled: its `tx` is dropped, so the rx
        // resolves to a `RecvError` rather than hanging on the 1h sleep.
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
        assert!(
            matches!(cancelled, Ok(Err(_))),
            "prior task must be aborted on supersession (got {cancelled:?})",
        );

        // The current (superseding) wait still resolves.
        let got = pooled.snapshot_wait(id).await.unwrap();
        assert_eq!(got.id, want);
    }

    /// Lifecycle: `destroy` reclaims an unconsumed slot (the
    /// coordinator's give-up path) and aborts the backing upload — no
    /// leak. Without the destroy cleanup the kept-until-consumed slot
    /// would leak for the host's lifetime.
    #[tokio::test]
    async fn destroy_reclaims_unconsumed_snapshot_wait_slot() {
        let pooled = lifecycle_backend();
        let id = SandboxId::new();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let never = tokio::spawn(async move {
            let _drop_signals_cancellation = tx;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok::<_, SandboxError>(fake_metadata())
        });
        let wait = SnapshotWait::from_handle(never);
        pooled.snapshot_waits.insert(id, wait);

        // ProcessBackend has no such sandbox; destroy's inner result is
        // irrelevant to the slot-cleanup contract under test.
        let _ = pooled.destroy(id).await;

        assert!(
            !pooled.snapshot_waits.contains_key(&id),
            "destroy must remove the snapshot_wait slot (issue #221 leak guard)",
        );
        // The backing upload task is cancelled by destroy.
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
        assert!(
            matches!(cancelled, Ok(Err(_))),
            "destroy must abort the backing upload task (got {cancelled:?})",
        );
    }

    // ---- Issue #529: host-durable eviction finalize ----
    use crate::checkpoint::CheckpointRecord;
    use parking_lot::Mutex as PlMutex;
    use std::path::Path;

    /// A `BlobStorage` whose FIRST `put_streaming` blocks until
    /// released — used to freeze a spawned eviction finalize job
    /// mid-leg so a test can observe it still pending, deterministically
    /// (not racing the job's own completion).
    struct GateFirstPut {
        inner: engram_storage_local::LocalBlobStorage,
        gate: Arc<tokio::sync::Notify>,
        armed: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl engram_core::traits::BlobStorage for GateFirstPut {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::traits::ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            if !self.armed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.gate.notified().await;
            }
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::ByteStream, engram_core::error::BlobError> {
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), engram_core::error::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(
            &self,
            prefix: &str,
        ) -> Result<Vec<String>, engram_core::error::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    /// A minimal FC-shaped capture backend for the eviction-finalize
    /// tests: `supports_diff_checkpoints() -> true` (the gate
    /// `snapshot_begin` checks before ever routing here) and a
    /// `snapshot` that writes the same three artifacts real FC leaves in
    /// its staging dir (memory.bin, state.bin, manifest.json) — the
    /// `SnapshotFinisher`/`eviction_finalize` legs' contract.
    #[derive(Clone)]
    struct FakeCaptureBackend {
        payload: Vec<u8>,
        staging_root: PathBuf,
        destroy_calls: Arc<PlMutex<Vec<SandboxId>>>,
    }
    impl FakeCaptureBackend {
        fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.staging_root.join(id.to_string())
        }
        async fn write_capture(&self, dest: &Path) {
            tokio::fs::create_dir_all(dest).await.unwrap();
            tokio::fs::write(dest.join("memory.bin"), &self.payload)
                .await
                .unwrap();
            tokio::fs::write(dest.join("state.bin"), b"state-bin-placeholder")
                .await
                .unwrap();
            let manifest = serde_json::json!({
                "sandbox_id": uuid::Uuid::new_v4(),
                "created_at": chrono::Utc::now(),
                "spec": {
                    "image": "test:1", "rootfs_source": null, "image_uri": null,
                    "harness_pack_uri": null, "cpu": {"vcpus": 1}, "memory": {"max_mib": 64},
                    "disk": {"max_gib": 1}, "ttl": null, "env": {}, "workdir": null,
                    "harness_substrate": null, "network": {}
                },
                "format": "fc"
            });
            tokio::fs::write(
                dest.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .await
            .unwrap();
        }
    }
    #[async_trait]
    impl SandboxBackend for FakeCaptureBackend {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn supports_diff_checkpoints(&self) -> bool {
            true
        }
        async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            let snapshot_id = engram_core::SnapshotId::new();
            let dest = self.dir_for(snapshot_id);
            self.write_capture(&dest).await;
            Ok(SnapshotMetadata {
                id: snapshot_id,
                size_bytes: self.payload.len() as u64,
                created_at: chrono::Utc::now(),
                image_version: "test:1".into(),
                disk_manifest: None,
                memory_manifest: None,
                base_memory_manifest: None,
                migration_source: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: vec![],
                paused_at: None,
                peer_hints: Vec::new(),
            })
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.dir_for(id)
        }
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroy_calls.lock().push(id);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    fn finalize_payload() -> Vec<u8> {
        let mut bytes = vec![0u8; 64 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i % 200) + 1) as u8; // skip zero so chunks aren't elided
        }
        bytes
    }

    /// `PooledBackend` wired with a chunk store + checkpoint_dir + the
    /// `FakeCaptureBackend`, ready to exercise `snapshot_begin`. The
    /// `gate`'d blob storage lets a test freeze the spawned finalize job
    /// mid-upload; `None` runs it to completion unobstructed.
    async fn finalize_test_backend(
        gate: Option<Arc<tokio::sync::Notify>>,
    ) -> (
        Arc<PooledBackend>,
        ChunkStore,
        PathBuf,
        Arc<PlMutex<Vec<SandboxId>>>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let local = engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob"));
        let blob: Arc<dyn engram_core::traits::BlobStorage> = match gate {
            Some(gate) => Arc::new(GateFirstPut {
                inner: local,
                gate,
                armed: std::sync::atomic::AtomicBool::new(false),
            }),
            None => Arc::new(local),
        };
        let cs = ChunkStore::new(blob);
        let destroy_calls = Arc::new(PlMutex::new(Vec::new()));
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: destroy_calls.clone(),
        });
        let checkpoint_dir = tmp.path().join("checkpoints");
        let materialize_dir = tmp.path().join("materialized");
        let p = PooledBackend::new(inner)
            .with_chunk_store(cs.clone(), materialize_dir)
            .with_checkpoint_dir(checkpoint_dir.clone());
        let arc = Arc::new(p);
        arc.set_self_ref(&arc);
        std::mem::forget(tmp); // keep the staging dirs alive for the test
        (arc, cs, checkpoint_dir, destroy_calls)
    }

    /// Acceptance criterion #7: an eviction-scanner retry storm against
    /// a mid-finalize sandbox must NOT re-capture — `snapshot_begin`
    /// re-observes the pending `snapshot_id`. Gated so the first call's
    /// background job can't race to completion before the second call.
    #[tokio::test]
    async fn snapshot_begin_is_idempotent_under_a_pending_finalize() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (pooled, _cs, _ckpt_dir, _destroy_calls) =
            finalize_test_backend(Some(gate.clone())).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let first = pooled
            .snapshot_begin(sandbox_id)
            .await
            .expect("first begin");
        let second = pooled
            .snapshot_begin(sandbox_id)
            .await
            .expect("second begin (retry storm)");
        assert_eq!(
            first, second,
            "a pending finalize must be re-observed, not re-captured"
        );

        // Let the frozen job run to completion so it doesn't outlive the
        // test's temp dirs.
        gate.notify_one();
    }

    /// `snapshot_begin` persists the `EvictionFinalizeRecord` (durably,
    /// to `<checkpoint_dir>/finalize/`) BEFORE it returns — the core
    /// durability-boundary-moves-earlier claim. Gated so the assertion
    /// runs while the job is still frozen mid-leg, not racing its own
    /// (fast, local-disk) completion.
    #[tokio::test]
    async fn snapshot_begin_persists_the_finalize_record_before_returning() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (pooled, _cs, ckpt_dir, _destroy_calls) =
            finalize_test_backend(Some(gate.clone())).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let snapshot_id = pooled.snapshot_begin(sandbox_id).await.expect("begin");
        assert!(
            pooled.pending_finalizes.contains_key(&sandbox_id),
            "pending_finalizes must be set before snapshot_begin returns",
        );
        let record_path = ckpt_dir
            .join("finalize")
            .join(format!("{snapshot_id}.json"));
        let bytes = tokio::fs::read(&record_path)
            .await
            .expect("finalize record must be on disk before snapshot_begin returns");
        // R5: records are sealed in a content-hash envelope keyed on their id.
        let body = crate::durable_envelope::open(&bytes, &snapshot_id.to_string())
            .expect("finalize record envelope must open");
        let record: crate::eviction_finalize::EvictionFinalizeRecord =
            serde_json::from_slice(&body).expect("finalize record must parse");
        assert_eq!(record.sandbox_id, sandbox_id);
        assert_eq!(record.session_id, session_id);
        // The gate freezes the job inside the memory leg (the first
        // blob PUT), so by construction the disk leg — synchronous,
        // no blob I/O — is the furthest it can have progressed; not
        // asserting an exact stage here avoids racing the job's own
        // (legitimately fast, local-disk) advancement past `Captured`.
        assert!(
            matches!(
                record.stage,
                crate::eviction_finalize::FinalizeStage::Captured
                    | crate::eviction_finalize::FinalizeStage::DiskUploaded
            ),
            "unexpected stage {:?} while the job is gated in the memory leg",
            record.stage,
        );

        gate.notify_one();
    }

    /// The full happy path: `snapshot_begin` on an FC-shaped capture
    /// eventually (without the coordinator ever touching it again)
    /// produces a durable `CheckpointRecord { kind: EvictionFinal }`,
    /// deletes its `EvictionFinalizeRecord`, clears `pending_finalizes`,
    /// and best-effort destroys the sandbox.
    #[tokio::test]
    async fn snapshot_begin_completes_and_produces_an_eviction_final_checkpoint() {
        let (pooled, cs, ckpt_dir, destroy_calls) = finalize_test_backend(None).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let snapshot_id = pooled.snapshot_begin(sandbox_id).await.expect("begin");

        let records_dir = ckpt_dir.join("records");
        let record_path = records_dir.join(format!("{snapshot_id}.json"));
        wait_for("eviction-final checkpoint record", || record_path.exists()).await;

        let bytes = tokio::fs::read(&record_path).await.unwrap();
        // R5: records are sealed in a content-hash envelope keyed on their id.
        let body = crate::durable_envelope::open(&bytes, &snapshot_id.to_string()).unwrap();
        let record: CheckpointRecord = serde_json::from_slice(&body).unwrap();
        assert_eq!(record.snapshot_id, snapshot_id);
        assert_eq!(record.sandbox_id, sandbox_id);
        assert_eq!(record.session_id, session_id);
        assert_eq!(
            record.kind,
            engram_protocol::heartbeat::CheckpointKind::EvictionFinal
        );
        let memory_ref = record
            .memory_manifest
            .expect("a captured memory.bin must chunk to a manifest");
        let manifest = cs.get_manifest(memory_ref).await.unwrap();
        assert_eq!(manifest.total_bytes, finalize_payload().len() as u64);

        // The checkpoint record becomes VISIBLE at rename time, but the
        // job deletes the finalize record only after `persist` returns —
        // which now includes a parent-dir fsync (an F_FULLFSYNC on macOS)
        // AFTER the rename. Waiting only for the checkpoint file above
        // therefore races the deletion/clear steps by that fsync latency;
        // wait for them like the destroy below, rather than asserting a
        // point-in-time snapshot of a still-running job.
        let finalize_record_path = ckpt_dir
            .join("finalize")
            .join(format!("{snapshot_id}.json"));
        wait_for("in-flight finalize record deleted on completion", || {
            !finalize_record_path.exists()
        })
        .await;
        wait_for("pending_finalizes cleared on completion", || {
            !pooled.pending_finalizes.contains_key(&sandbox_id)
        })
        .await;
        wait_for("best-effort destroy", || {
            destroy_calls.lock().contains(&sandbox_id)
        })
        .await;
    }

    /// Issue #529 crash recovery: a record persisted by a PRIOR host-agent
    /// process (the sandbox is gone — never `create()`d in this test)
    /// is picked up by `resume_pending_finalizes` and driven to the same
    /// durable `CheckpointRecord { kind: EvictionFinal }` outcome.
    #[tokio::test]
    async fn resume_pending_finalizes_redrives_a_record_with_the_sandbox_absent() {
        let (pooled, cs, ckpt_dir, _destroy_calls) = finalize_test_backend(None).await;
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let snapshot_id = engram_core::SnapshotId::new();

        // Hand-craft the on-disk state a crashed `snapshot_begin` would
        // have left: the FC staging dir (memory.bin/state.bin/manifest.json)
        // plus the durable EvictionFinalizeRecord pointing at it —
        // entirely independent of any live sandbox or RAM state.
        let dest = tmp_dest_for(&ckpt_dir, snapshot_id);
        let backend = FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: dest.parent().unwrap().to_path_buf(),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        };
        backend.write_capture(&dest).await;

        let record = crate::eviction_finalize::EvictionFinalizeRecord {
            snapshot_id,
            session_id,
            sandbox_id,
            image_version: "test:1".into(),
            size_bytes: finalize_payload().len() as u64,
            paused_at: chrono::Utc::now(),
            captured_at: chrono::Utc::now(),
            dest,
            chain_prev_ref: None,
            disk_pending: None,
            aux_bundles: vec![],
            stage: crate::eviction_finalize::FinalizeStage::Captured,
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        };
        record
            .persist(&engram_host_core::TokioFs, &ckpt_dir.join("finalize"))
            .await
            .expect("persist finalize record");

        pooled.resume_pending_finalizes().await;

        let record_path = ckpt_dir.join("records").join(format!("{snapshot_id}.json"));
        wait_for("redriven eviction-final checkpoint record", || {
            record_path.exists()
        })
        .await;
        let bytes = tokio::fs::read(&record_path).await.unwrap();
        let body = crate::durable_envelope::open(&bytes, &snapshot_id.to_string()).unwrap();
        let checkpoint: CheckpointRecord = serde_json::from_slice(&body).unwrap();
        assert_eq!(checkpoint.sandbox_id, sandbox_id);
        assert_eq!(
            checkpoint.kind,
            engram_protocol::heartbeat::CheckpointKind::EvictionFinal
        );
        let memory_ref = checkpoint.memory_manifest.expect("memory manifest set");
        let manifest = cs.get_manifest(memory_ref).await.unwrap();
        assert_eq!(manifest.total_bytes, finalize_payload().len() as u64);
    }

    fn tmp_dest_for(checkpoint_dir: &Path, id: engram_core::SnapshotId) -> PathBuf {
        checkpoint_dir
            .parent()
            .unwrap()
            .join("fc-snaps-redrive")
            .join(id.to_string())
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A `BlobStorage` whose PUTs always fail — drives the finalize
    /// quarantine path.
    struct AlwaysFailPut;
    #[async_trait]
    impl engram_core::traits::BlobStorage for AlwaysFailPut {
        async fn put_streaming(
            &self,
            _key: &str,
            _body: engram_core::traits::ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            Err(engram_core::error::BlobError::Protocol(
                "injected put failure (test)".into(),
            ))
        }
        async fn get_streaming(
            &self,
            _key: &str,
        ) -> Result<engram_core::traits::ByteStream, engram_core::error::BlobError> {
            Err(engram_core::error::BlobError::Protocol(
                "injected get failure (test)".into(),
            ))
        }
        async fn head(
            &self,
            _key: &str,
        ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
            Err(engram_core::error::BlobError::Protocol(
                "injected head failure (test)".into(),
            ))
        }
        async fn delete(&self, _key: &str) -> Result<(), engram_core::error::BlobError> {
            Ok(())
        }
        async fn list_prefix(
            &self,
            _prefix: &str,
        ) -> Result<Vec<String>, engram_core::error::BlobError> {
            Ok(Vec::new())
        }
    }

    /// Acceptance criterion #5: a finalize that fails terminally is
    /// quarantined with a metric + WARN/ERROR — never silent — after
    /// `ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS` attempts. Set to 1 so the
    /// test quarantines on the very first failure (no backoff sleep).
    #[tokio::test]
    async fn finalize_quarantines_after_max_attempts() {
        // SAFETY (test-only): cargo-nextest runs each test in its own
        // process, so this process-global env var doesn't leak across
        // tests.
        std::env::set_var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS", "1");

        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(AlwaysFailPut);
        let cs = ChunkStore::new(blob);
        let destroy_calls = Arc::new(PlMutex::new(Vec::new()));
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: destroy_calls.clone(),
        });
        let checkpoint_dir = tmp.path().join("checkpoints");
        let p = PooledBackend::new(inner)
            .with_chunk_store(cs, tmp.path().join("materialized"))
            .with_checkpoint_dir(checkpoint_dir.clone());
        let pooled = Arc::new(p);
        pooled.set_self_ref(&pooled);

        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        pooled.session_bindings.insert(sandbox_id, session_id);

        let snapshot_id = pooled.snapshot_begin(sandbox_id).await.expect("begin");

        let quarantined_path = checkpoint_dir
            .join("finalize")
            .join("failed")
            .join(format!("{snapshot_id}.json"));
        wait_for("quarantined finalize record", || quarantined_path.exists()).await;

        let in_flight_path = checkpoint_dir
            .join("finalize")
            .join(format!("{snapshot_id}.json"));
        // `quarantine()` persists the failed/ copy FIRST, then deletes
        // the in-flight record — observing the former does not imply
        // the latter yet (flaked under full-suite load).
        wait_for("in-flight record cleared after quarantine", || {
            !in_flight_path.exists()
        })
        .await;
        assert!(
            !pooled.pending_finalizes.contains_key(&sandbox_id),
            "pending_finalizes must be cleared on quarantine"
        );
        // No CheckpointRecord — never falsely claim durability on the
        // path that gave up.
        let checkpoint_path = checkpoint_dir
            .join("records")
            .join(format!("{snapshot_id}.json"));
        assert!(
            !checkpoint_path.exists(),
            "a quarantined finalize must never produce a checkpoint record"
        );

        std::env::remove_var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS");
        std::mem::forget(tmp);
    }

    /// Finding 6: `snapshot_begin`'s own `supports_diff_checkpoints()` gate
    /// — the only thing keeping a VZ/Process host on the composed
    /// `snapshot()` pipeline instead of the host-durable eviction finalize
    /// path — must surface `InvalidSpec` (the coordinator's `idle_evictor`
    /// falls through to `snapshot()` on exactly that variant). `ProcessBackend`
    /// doesn't override the trait default (`false`), the same shape VZ is
    /// (module doc: "Gate: VZ/Process ... surface `InvalidSpec`").
    #[tokio::test]
    async fn snapshot_begin_returns_invalidspec_over_a_non_diff_checkpoint_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let inner = Arc::new(ProcessBackend::new(tmp.path().join("sandboxes")));
        assert!(
            !inner.supports_diff_checkpoints(),
            "test premise: ProcessBackend must not opt into diff checkpoints"
        );
        let pooled = PooledBackend::new(inner)
            .with_chunk_store(cs, tmp.path().join("materialize"))
            .with_checkpoint_dir(tmp.path().join("checkpoints"));

        let err = pooled
            .snapshot_begin(SandboxId::new())
            .await
            .expect_err("a non-diff-checkpoint backend must not enter the host-durable path");
        assert!(
            matches!(err, SandboxError::InvalidSpec(_)),
            "expected InvalidSpec (the idle_evictor's composed-path fallback signal), got {err:?}"
        );
    }

    /// Finding 6, second half: the same gate's `checkpoint_dir` half. A
    /// diff-checkpoint-capable backend (FC-shaped) with no `checkpoint_dir`
    /// wired (checkpointing disabled on this host) must also surface
    /// `InvalidSpec`, not attempt to persist a finalize record with nowhere
    /// durable to put it.
    #[tokio::test]
    async fn snapshot_begin_returns_invalidspec_without_a_wired_checkpoint_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        });
        // Deliberately no `.with_checkpoint_dir(..)`.
        let pooled = PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialize"));

        let err = pooled
            .snapshot_begin(SandboxId::new())
            .await
            .expect_err("no wired checkpoint_dir must not enter the host-durable path");
        assert!(
            matches!(err, SandboxError::InvalidSpec(_)),
            "expected InvalidSpec (the idle_evictor's composed-path fallback signal), got {err:?}"
        );
    }

    // ─── durable chain-head record (survivor rehydrate after a pod roll) ───

    fn chain_head_file(checkpoint_dir: &Path, id: SandboxId) -> PathBuf {
        crate::checkpoint::ChainHeadRecord::subdir(checkpoint_dir).join(format!("{id}.json"))
    }

    /// The write-ahead protocol end to end on the composed snapshot
    /// path: (1) a successful Full capture commits a chain-head record
    /// matching the published memory manifest; (2) the next capture
    /// takes the diff path (chain seeded) and — because the write-ahead
    /// invalidate runs BEFORE the FC create — a failed diff create
    /// leaves NO record (the torn-capture property from the 2026-07-13
    /// incident) and drops the chain; (3) the capture after that is a
    /// Full again and re-commits the record.
    #[tokio::test]
    async fn chain_head_record_follows_the_write_ahead_protocol() {
        let (pooled, _cs, ckpt_dir, _destroy) = finalize_test_backend(None).await;
        let id = SandboxId::new();

        // (1) Full capture → chain seeded → record committed.
        let meta = pooled.snapshot(id).await.expect("full capture");
        let mem_ref = meta.memory_manifest.expect("memory manifest chunked");
        let record = crate::checkpoint::ChainHeadRecord::load(
            &crate::checkpoint::ChainHeadRecord::subdir(&ckpt_dir),
            id,
        )
        .await
        .expect("chain-head record committed after the Full");
        assert_eq!(record.manifest_ref, mem_ref);
        assert_eq!(pooled.chain_head_for_test(id), Some(mem_ref));

        // (2) The chain routes the next capture to snapshot_diff, which
        // the fake backend doesn't support — a create failure AFTER the
        // write-ahead invalidate. The record must be gone and the chain
        // poisoned; nothing may have resurrected the record.
        pooled
            .snapshot(id)
            .await
            .expect_err("diff create must fail on the fake backend");
        assert!(
            !chain_head_file(&ckpt_dir, id).exists(),
            "failed diff must leave no chain-head record (write-ahead invalidate)"
        );
        assert_eq!(
            pooled.chain_head_for_test(id),
            None,
            "failed diff create must poison the in-RAM chain"
        );

        // (3) Chain-less again → Full succeeds → record re-committed.
        let meta = pooled.snapshot(id).await.expect("post-poison full capture");
        let mem_ref = meta.memory_manifest.expect("memory manifest chunked");
        let record = crate::checkpoint::ChainHeadRecord::load(
            &crate::checkpoint::ChainHeadRecord::subdir(&ckpt_dir),
            id,
        )
        .await
        .expect("record re-committed by the recovery Full");
        assert_eq!(record.manifest_ref, mem_ref);

        // destroy removes the record with the chain.
        use engram_core::traits::SandboxBackend as _;
        pooled.destroy(id).await.expect("destroy");
        assert!(!chain_head_file(&ckpt_dir, id).exists());
    }

    /// `FakeCaptureBackend` with a configurable `list()` — the survivor
    /// set `rehydrate_chain_heads` seeds from.
    #[derive(Clone)]
    struct SurvivorListBackend {
        inner: FakeCaptureBackend,
        survivors: Vec<SandboxId>,
    }
    #[async_trait]
    impl SandboxBackend for SurvivorListBackend {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn supports_diff_checkpoints(&self) -> bool {
            true
        }
        async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            self.inner.snapshot(id).await
        }
        fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
            self.inner.snapshot_path_for(id)
        }
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.inner.destroy(id).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(self.survivors.clone())
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    /// The pod-roll simulation at the unit level: generation A captures
    /// (committing the chain-head record) and dies; generation B — a
    /// fresh `PooledBackend` on the same checkpoint_dir + chunk store,
    /// whose backend reattached the survivor — rehydrates the chain from
    /// the record. Records for sandboxes that did NOT survive are GC'd,
    /// and a torn record seeds nothing.
    #[tokio::test]
    async fn rehydrate_chain_heads_seeds_survivors_and_gcs_strays() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);
        let checkpoint_dir = tmp.path().join("checkpoints");
        let fake = FakeCaptureBackend {
            payload: finalize_payload(),
            staging_root: tmp.path().join("fc-snaps"),
            destroy_calls: Arc::new(PlMutex::new(Vec::new())),
        };

        let survivor = SandboxId::new();
        let dead = SandboxId::new();
        let torn = SandboxId::new();

        // Generation A: capture the survivor AND the dead sandbox —
        // both get chain-head records.
        let gen_a = {
            let p = PooledBackend::new(Arc::new(fake.clone()) as Arc<dyn SandboxBackend>)
                .with_chunk_store(cs.clone(), tmp.path().join("materialize-a"))
                .with_checkpoint_dir(checkpoint_dir.clone());
            let arc = Arc::new(p);
            arc.set_self_ref(&arc);
            arc
        };
        let survivor_meta = gen_a.snapshot(survivor).await.expect("survivor capture");
        let survivor_ref = survivor_meta.memory_manifest.expect("chunked");
        gen_a.snapshot(dead).await.expect("dead-sandbox capture");
        // A torn (unparseable) record for a third "survivor".
        let chains_dir = crate::checkpoint::ChainHeadRecord::subdir(&checkpoint_dir);
        tokio::fs::write(chains_dir.join(format!("{torn}.json")), b"not json")
            .await
            .unwrap();
        drop(gen_a); // the roll

        // Generation B: only `survivor` (and the torn id) reattached.
        let gen_b = {
            let p = PooledBackend::new(Arc::new(SurvivorListBackend {
                inner: fake,
                survivors: vec![survivor, torn],
            }) as Arc<dyn SandboxBackend>)
            .with_chunk_store(cs, tmp.path().join("materialize-b"))
            .with_checkpoint_dir(checkpoint_dir.clone());
            let arc = Arc::new(p);
            arc.set_self_ref(&arc);
            arc
        };
        assert_eq!(gen_b.chain_head_for_test(survivor), None, "fresh map");
        gen_b.rehydrate_chain_heads().await;

        assert_eq!(
            gen_b.chain_head_for_test(survivor),
            Some(survivor_ref),
            "survivor's chain must be re-seeded from the durable record"
        );
        assert!(
            !chain_head_file(&checkpoint_dir, dead).exists(),
            "record for a non-surviving sandbox must be GC'd"
        );
        assert_eq!(
            gen_b.chain_head_for_test(torn),
            None,
            "a torn record must seed nothing (treated as absent)"
        );
    }

    // ---- balloon_inflate_for_seed matrix (adversarial-review fix) ----
    //
    // The invariant under test: after ANY outcome, either the balloon is
    // provably deflated / absent (Ok(false)), or the caller has been told
    // it owes a confirmed release (Ok(true)), or the capture fails (Err).
    // There is no path that proceeds with the balloon state unknown.

    mod balloon_matrix {
        use super::super::balloon_inflate_for_seed;
        use async_trait::async_trait;
        use engram_core::traits::sandbox::SandboxBackend;
        use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
        use engram_core::types::snapshot::SnapshotMetadata;
        use engram_core::{SandboxError, SandboxId};
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;

        struct BalloonMock {
            reclaim_result: Mutex<Option<Result<u64, SandboxError>>>,
            release_result: Mutex<Option<Result<(), SandboxError>>>,
            release_calls: AtomicUsize,
        }

        impl BalloonMock {
            fn new(reclaim: Result<u64, SandboxError>, release: Result<(), SandboxError>) -> Self {
                Self {
                    reclaim_result: Mutex::new(Some(reclaim)),
                    release_result: Mutex::new(Some(release)),
                    release_calls: AtomicUsize::new(0),
                }
            }
        }

        #[async_trait]
        impl SandboxBackend for BalloonMock {
            async fn balloon_reclaim(
                &self,
                _: SandboxId,
                _: u64,
                _: std::time::Duration,
            ) -> Result<u64, SandboxError> {
                self.reclaim_result
                    .lock()
                    .unwrap()
                    .take()
                    .expect("reclaim called once")
            }
            async fn balloon_release(&self, _: SandboxId) -> Result<(), SandboxError> {
                self.release_calls.fetch_add(1, Ordering::SeqCst);
                self.release_result
                    .lock()
                    .unwrap()
                    .take()
                    .expect("release called at most once")
            }

            // ---- unused required surface ----
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                unreachable!()
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                unreachable!()
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                unreachable!()
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                unreachable!()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                unreachable!()
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                unreachable!()
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                unreachable!()
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                unreachable!()
            }
        }

        fn vm_err(msg: &str) -> SandboxError {
            SandboxError::Vm(msg.to_string().into())
        }

        const DL: std::time::Duration = std::time::Duration::from_secs(1);

        /// A landed inflate — even a 0-MiB grant — obligates a release.
        #[tokio::test]
        async fn landed_inflate_owes_a_release() {
            for granted in [0u64, 512] {
                let mock = BalloonMock::new(Ok(granted), Ok(()));
                let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                    .await
                    .unwrap();
                assert!(
                    inflated,
                    "granted={granted}: caller must be told to release"
                );
                assert_eq!(
                    mock.release_calls.load(Ordering::SeqCst),
                    0,
                    "the helper itself must not release on the happy path \
                     (the caller releases after the dump)"
                );
            }
        }

        /// The TYPED no-device outcome is the only fail-open: dense
        /// seed, no release owed, no release attempted.
        #[tokio::test]
        async fn typed_no_device_is_fail_open() {
            let mock = BalloonMock::new(
                Err(SandboxError::InvalidSpec("no balloon device".into())),
                Ok(()),
            );
            let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                .await
                .unwrap();
            assert!(!inflated);
            assert_eq!(mock.release_calls.load(Ordering::SeqCst), 0);
        }

        /// An untyped reclaim failure (the inflate PATCH may have
        /// landed) is normalized via release-and-confirm; a confirmed
        /// release means a safe dense seed.
        #[tokio::test]
        async fn untyped_reclaim_failure_normalizes_via_release() {
            let mock = BalloonMock::new(Err(vm_err("stats poll: connection reset")), Ok(()));
            let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                .await
                .unwrap();
            assert!(!inflated, "normalized ⇒ dense seed, nothing owed");
            assert_eq!(
                mock.release_calls.load(Ordering::SeqCst),
                1,
                "the normalizing release MUST run on an untyped reclaim failure"
            );
        }

        /// Reclaim failed AND the normalizing release failed: the
        /// balloon state is unknown — the capture must fail, never
        /// proceed to a warm hook.
        #[tokio::test]
        async fn unnormalizable_balloon_state_fails_the_capture() {
            let mock = BalloonMock::new(
                Err(vm_err("stats poll: connection reset")),
                Err(SandboxError::Snapshot("deflate did not complete".into())),
            );
            let err = balloon_inflate_for_seed(&mock, SandboxId::new(), 1024, DL)
                .await
                .expect_err("unknown balloon state must fail the capture");
            assert!(
                err.to_string().contains("balloon state unknown"),
                "error must say why: {err}"
            );
            assert_eq!(mock.release_calls.load(Ordering::SeqCst), 1);
        }

        /// Target 0 (tiny guest ≤ the reserve): no balloon interaction
        /// at all.
        #[tokio::test]
        async fn zero_target_never_touches_the_balloon() {
            let mock = BalloonMock::new(Ok(0), Ok(()));
            let inflated = balloon_inflate_for_seed(&mock, SandboxId::new(), 0, DL)
                .await
                .unwrap();
            assert!(!inflated);
            assert_eq!(mock.release_calls.load(Ordering::SeqCst), 0);
            assert!(
                mock.reclaim_result.lock().unwrap().is_some(),
                "reclaim must not have been called"
            );
        }
    }
}
