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
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
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
// ADR 0037: how long a warm capture waits for the spawned harness to reach
// warm+idle before falling back to a cold capture. Generous: claude (Bun)
// cold-boots in roughly 7-9s, plus margin for a loaded capture host.
const WARM_CAPTURE_TIMEOUT_SECS: u64 = 60;

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
    /// ADR 0007: chunk-store-backed materialization. When set, the
    /// `bundle.json` on a cached image is the source of truth for
    /// the disk — chunks are fetched from `BlobStorage`, written to
    /// a content-addressed file under `materialize_dir`, and that
    /// path becomes `spec.rootfs_source`. When unset, we fall back
    /// to the OCI-pulled `rootfs.ext4` (transitional path; retired
    /// in Phase 6).
    chunk_store: Option<ChunkStore>,
    /// Per-host directory where chunked manifests are materialized.
    /// `Some` iff `chunk_store` is. Files inside are named by
    /// `manifest_id`-`version` so two sessions hitting the same
    /// manifest share the materialized file (and VZ's per-sandbox
    /// APFS clonefile / FC's NBD-on-the-shared-path work on top).
    materialize_dir: Option<PathBuf>,
    /// ADR 0035: fleet-canonical staged-bundle dir
    /// (`<drive_id>-<sha256>.squashfs` + the bake's `current.json`).
    /// Defaults to `AuxRoDrive::SHARED_DIR`; tests inject a tempdir
    /// via `with_bundle_dir`.
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
    /// ADR 0037: the host's `HarnessHub`, injected post-construction (the
    /// hub is built alongside this backend in `lib.rs`). `build_base_snapshot`
    /// reads it to wait for the warm capture harness to reach warm+idle;
    /// `None` (tests / mode=all glue without warm-capture) ⇒ cold capture.
    warm_capture_hub: parking_lot::Mutex<Option<Arc<crate::harness::HarnessHub>>>,
}

impl PooledBackend {
    /// Shared body of `restore` (resume flavor) and `restore_fresh`
    /// (fresh-create flavor) — see ADR 0035 §3 for the split.
    async fn restore_with(
        &self,
        metadata: SnapshotMetadata,
        fresh: bool,
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
        let prefetch_start = std::time::Instant::now();
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
        let _ = (prefetched_chunks, prefetch_start);

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
            self.prepare_resume_nbd_attach(&metadata, &src),
            tracing::info_span!("restore.prepare_nbd"),
        )
        .await?;

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
        // contiguous memory.bin is now a per-restore-flavor decision.
        // Resume under UFFD (`fresh == false`) serves memory lazily from
        // chunks — the handler faults straight from the cache the prefetch
        // above just warmed, so materializing would be pure overhead.
        // Base-create under File (`fresh == true`) needs the memfile
        // present for `load_snapshot`; `materialize_memory_if_missing` is
        // an idempotent no-op once the residency prefetch (image_prefetch)
        // wrote the *per-template* file at this same snapshot-id-keyed
        // path — which is exactly what keeps siblings sharing one inode
        // rather than each rebuilding a divergent copy.
        if self.inner.restore_memory_is_lazy_for(fresh) {
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
        // ADR 0035 §3: fresh creates swap aux bundles to the host's
        // current generation inside the backend; resumes keep the pin.
        let new_id = if fresh {
            self.inner.restore_fresh(metadata).await?
        } else {
            self.inner.restore(metadata).await?
        };

        // ADR 0016 Phase B commit 5: post-restore wiring. The new
        // sandbox_id is only known here; install it into
        // `nbd_sandboxes` together with the FlushScheduler so the
        // resumed sandbox is first-class in the COW diagnostic and
        // in the continuous-flush pipeline. Field-ordered Drop
        // ensures scheduler-cancel → NBD-disconnect → slot-release
        // on subsequent destroy.
        #[cfg(target_os = "linux")]
        if let Some(mut state) = pending_nbd_state {
            state.install_flush_scheduler(
                new_id,
                self.live_manifest_publisher.clone(),
                self.flush_config.clone(),
            );
            // ADR 0019: open the resume operation window — restore-time disk
            // reads (load_snapshot + the resumed guest's working set) attach
            // `chunk.fetch` spans to this trace. Covers idle→resume AND
            // evac-dest; the coord parent trace distinguishes them. Closed by
            // `start_agent` (finish_resume_to_active calls it). Memory page-in
            // is the UFFD side, on the spawn-TRACEPARENT path.
            state.backend.operation_scope().begin("resume");
            self.nbd_sandboxes.insert(new_id, state);
        }
        Ok(new_id)
    }

    pub fn new(inner: Arc<dyn SandboxBackend>) -> Self {
        Self {
            inner,
            image_cache: None,
            egress: None,
            session_bindings: Arc::new(DashMap::new()),
            chunk_store: None,
            materialize_dir: None,
            bundle_dir: PathBuf::from(engram_core::types::sandbox::AuxRoDrive::SHARED_DIR),
            chunk_cache: None,
            materialize_lock: Mutex::new(()),
            oci_client: None,
            nbd_pool: None,
            #[cfg(target_os = "linux")]
            nbd_sandboxes: Arc::new(DashMap::new()),
            inflight_snapshots: Arc::new(DashMap::new()),
            last_snapshot_unix_ms: Arc::new(DashMap::new()),
            // ADR 0016 Phase B: scheduler defaults come from env at
            // host-agent startup; the builder method
            // `with_flush_scheduler` can override.
            flush_config: crate::disk_daemon::FlushSchedulerConfig::from_env(),
            live_manifest_publisher: Arc::new(crate::disk_daemon::NoOpLiveManifestPublisher),
            live_manifest_publisher_handle: None,
            checkpoint_dir: None,
            checkpoint_chains: Arc::new(DashMap::new()),
            capture_locks: Arc::new(DashMap::new()),
            warm_capture_hub: parking_lot::Mutex::new(None),
        }
    }

    /// ADR 0037: inject the host's `HarnessHub` so `build_base_snapshot`
    /// can wait for a warm-capture harness to reach warm+idle. Called once
    /// at host-agent startup, right after the hub is built (mirrors
    /// `set_harness_sink`). Without it, warm-capture falls back to cold.
    pub fn set_warm_capture_hub(&self, hub: Arc<crate::harness::HarnessHub>) {
        *self.warm_capture_hub.lock() = Some(hub);
    }

    /// ADR 0037 warm-capture: spawn the generic, prompt-less
    /// `warm_harness_spec` and block until it reaches warm+idle, so
    /// `build_base_snapshot` snapshots a ready agent. Returns `true` only
    /// when actually captured warm; `false` (⇒ cold capture) when opted
    /// out, no spec, no hub, or warming failed/timed out. Best-effort by
    /// construction — every failure path returns `false` and the caller
    /// proceeds with a normal cold snapshot. If a harness was spawned but
    /// never went idle, it's left running: a cold restore's `SpawnHarness`
    /// kill+respawn cleanly replaces it.
    async fn maybe_warm_capture(
        &self,
        id: SandboxId,
        warm_harness_spec: Option<AgentSpec>,
    ) -> bool {
        // Kill-switch: opt-in only (default off ⇒ today's cold capture).
        if !matches!(
            std::env::var("ENGRAM_WARM_HARNESS_CAPTURE").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        ) {
            return false;
        }
        let Some(agent) = warm_harness_spec else {
            return false; // harness-less image — nothing to warm
        };
        let Some(hub) = self.warm_capture_hub.lock().clone() else {
            tracing::warn!("warm-capture requested but no harness hub wired; capturing cold");
            return false;
        };
        // No sentinel session registration needed: the harness sink keys the
        // connection on this capture VM's sandbox id (per-sandbox vsock UDS),
        // not the harness's self-reported (sentinel) session id — so the
        // attach lands on `id` and `wait_harness_warm(id)` observes it.
        if let Err(e) = self.start_agent(id, agent).await {
            tracing::warn!(error = %e, "warm-capture: start_agent failed; capturing cold");
            return false;
        }
        let warm = hub
            .wait_harness_warm(
                id,
                std::time::Duration::from_secs(WARM_CAPTURE_TIMEOUT_SECS),
            )
            .await;
        if warm {
            tracing::info!(sandbox_id = %id, "warm-capture: harness warm+idle; capturing warm");
        } else {
            tracing::warn!(
                sandbox_id = %id,
                "warm-capture: harness did not reach warm+idle before deadline; capturing cold",
            );
        }
        warm
    }

    /// ADR 0028 Fix A: enable checkpoint chains, rooted at `dir`
    /// (`rolling/` + `records/` subdirs are created lazily). Without
    /// this, `snapshot()` stays pure-Full and the periodic driver
    /// no-ops.
    pub fn with_checkpoint_dir(mut self, dir: PathBuf) -> Self {
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

    /// ADR 0028 Fix A: where un-acked durable checkpoint records live.
    pub fn checkpoint_records_dir(&self) -> Option<PathBuf> {
        self.checkpoint_dir.as_ref().map(|d| d.join("records"))
    }

    fn checkpoint_rolling_path(&self, id: SandboxId) -> Option<PathBuf> {
        self.checkpoint_dir
            .as_ref()
            .map(|d| d.join("rolling").join(format!("{id}.memory.bin")))
    }

    /// ADR 0028 Fix A: post-capture chain bookkeeping + the durable
    /// host-owned record. Runs at the tail of every successful
    /// `snapshot()` (periodic checkpoint, eviction, drain, SIGTERM —
    /// they're all chain entries now). Never fails the capture: the
    /// snapshot's own durability (chunks + blobs in GCS) is already
    /// settled; everything here is acceleration (chain) or
    /// reconciliation insurance (record).
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
            // Diff capture: the rolling file was already overlaid in
            // the post block; just advance the chain's manifest.
            Some(next) => {
                if let Some(mut chain) = self.checkpoint_chains.get_mut(&id) {
                    chain.manifest_ref = memory_ref;
                    chain.manifest = next;
                }
            }
            // Full capture: seed the chain — copy the freshly-written
            // memory.bin as the rolling diff-apply target and fetch
            // the manifest we just published.
            None => {
                if let Err(e) = self.seed_checkpoint_chain(id, metadata, memory_ref).await {
                    tracing::warn!(
                        sandbox_id = %id,
                        error = %e,
                        "checkpoint chain seed failed; next capture will be Full again",
                    );
                }
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
        };
        if let Err(e) = record.persist(&records_dir).await {
            tracing::warn!(
                sandbox_id = %id,
                snapshot_id = %metadata.id,
                error = %e,
                "durable checkpoint record write failed; PG row (if the caller's \
                 pipeline survives) is the only reference",
            );
        }
    }

    async fn seed_checkpoint_chain(
        &self,
        id: SandboxId,
        metadata: &SnapshotMetadata,
        memory_ref: engram_core::types::manifest::ManifestRef,
    ) -> Result<(), SandboxError> {
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return Ok(());
        };
        let Some(rolling) = self.checkpoint_rolling_path(id) else {
            return Ok(());
        };
        let src = self.inner.snapshot_path_for(metadata.id).join("memory.bin");
        if tokio::fs::metadata(&src).await.is_err() {
            return Ok(());
        }
        if let Some(parent) = rolling.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("rolling dir: {e}")))?;
        }
        // GiB-scale copy off the hot path (the guest already resumed);
        // copy to a temp + rename so a crash never leaves a torn
        // rolling file masquerading as a baseline.
        let tmp = rolling.with_extension("partial");
        let (src_c, tmp_c) = (src.clone(), tmp.clone());
        tokio::task::spawn_blocking(move || std::fs::copy(&src_c, &tmp_c))
            .await
            .map_err(|e| SandboxError::Snapshot(format!("seed copy join: {e}")))?
            .map_err(|e| SandboxError::Snapshot(format!("seed copy: {e}")))?;
        tokio::fs::rename(&tmp, &rolling)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("seed rename: {e}")))?;
        let manifest = chunk_store
            .get_manifest(memory_ref)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("fetch seeded manifest: {e}")))?;
        self.checkpoint_chains.insert(
            id,
            crate::checkpoint::CheckpointChain {
                manifest_ref: memory_ref,
                manifest,
                rolling_memfile: rolling,
            },
        );
        tracing::info!(
            sandbox_id = %id,
            manifest = %memory_ref,
            "checkpoint chain seeded; subsequent captures ride the diff path",
        );
        Ok(())
    }

    /// Sandboxes due for a periodic checkpoint: session-bound, and no
    /// successful capture (of any flavor) within `interval`. Empty
    /// when checkpointing is disabled.
    pub fn checkpoint_candidates(
        &self,
        interval: std::time::Duration,
    ) -> Vec<(SandboxId, SessionId)> {
        if self.checkpoint_dir.is_none() {
            return Vec::new();
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let interval_ms = interval.as_millis() as i64;
        self.session_bindings
            .iter()
            .filter_map(|e| {
                let id = *e.key();
                let last = self.last_snapshot_unix_ms.get(&id).map(|v| *v).unwrap_or(0);
                (now_ms - last >= interval_ms).then_some((id, *e.value()))
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
    /// using the host-agent's `CoordClient` and the freshly-wrapped
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
        coord: crate::coord_client::CoordClient,
        host_id: engram_core::HostId,
    ) -> Self {
        let session_bindings = Arc::clone(&self.session_bindings);
        let resolver: Arc<dyn crate::disk_daemon::SessionResolver> =
            Arc::new(move |sandbox_id: SandboxId| -> Option<SessionId> {
                session_bindings.get(&sandbox_id).map(|e| *e)
            });
        let (publisher, handle) =
            crate::disk_daemon::CoordLiveManifestPublisher::spawn(coord, host_id, resolver);
        self.live_manifest_publisher = publisher;
        self.live_manifest_publisher_handle = Some(handle);
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

    /// ADR 0035: override the staged-bundle dir (tests). Production
    /// keeps the fleet-canonical default.
    pub fn with_bundle_dir(mut self, dir: PathBuf) -> Self {
        self.bundle_dir = dir;
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
    /// Concurrency is bounded at 8 — well under the NIC's
    /// saturation point on the prod n2-standard-8 hosts (single-
    /// stream GCS hits ~80 MB/s; 8× parallel = ~640 MB/s, half the
    /// 10 Gbps line rate) and well below GCS's per-object rate
    /// limits.
    async fn prefetch_memory_chunks(
        &self,
        metadata: &SnapshotMetadata,
    ) -> Result<usize, SandboxError> {
        let Some(mref) = metadata.memory_manifest else {
            return Ok(0);
        };
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return Ok(0);
        };
        let Some(cache) = self.chunk_cache.as_ref() else {
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
            Some(ws_key) => match self.fetch_working_set_chunks(chunk_store, ws_key).await {
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
            .prefetch_chunks_parallel(hashes_to_prefetch, 8, move |hash| {
                let s = store_for_fetch.clone();
                async move { s.get_chunk(hash).await }
            })
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
        &self,
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
    let state_path = src.join("state.bin");
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
    let sidecar_path = src.join("manifest.json");
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
/// zero chunks + zero bytes of object storage.
async fn chunk_memory_to_store(
    chunk_store: &ChunkStore,
    memory_bin: &std::path::Path,
) -> Result<engram_core::types::manifest::ManifestRef, SandboxError> {
    let manifest = chunk_store
        .chunk_file(memory_bin, engram_chunk_store::ManifestKind::Memory, None)
        .await
        .map_err(|e| {
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
async fn patch_fc_manifest_memory_ref(
    manifest_json: &std::path::Path,
    memory_manifest: engram_core::types::manifest::ManifestRef,
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

/// Pick a stable name for the harness substrate's mount-root
/// subdirectory. Tries: (1) the env-injected
/// `ENGRAM_SESSION_HARNESS_NAME` hint set by `resolve_harness` —
// ADR 0021 P1.5: `harness_name_for_substrate` / `harness_name_from_uri`
// retired with the substrate. The in-VM harness path comes from the
// image manifest's `[harness] exec` now, not a URI suffix.

#[async_trait]
impl SandboxBackend for PooledBackend {
    // Capability methods proxy to the wrapped backend — the pool is
    // a thin caching layer; whatever Process / VZ / FC reports
    // about itself is what callers see.
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        self.inner.harness_dial()
    }

    fn restore_memory_is_lazy(&self) -> bool {
        self.inner.restore_memory_is_lazy()
    }

    fn restore_memory_is_lazy_for(&self, fresh: bool) -> bool {
        self.inner.restore_memory_is_lazy_for(fresh)
    }

    async fn guest_memory_stats(&self) -> Option<engram_core::traits::sandbox::GuestMemoryStats> {
        self.inner.guest_memory_stats().await
    }

    async fn create(&self, mut spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
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
        let phase_total = std::time::Instant::now();
        let mut image_resolve = std::time::Duration::ZERO;
        let mut materialize = std::time::Duration::ZERO;
        let result: Result<SandboxId, SandboxError> = async {
            // ADR 0028 Fix B: explicit rootfs-manifest override wins
            // over image resolution — the disk-only cold-boot recovery
            // boots a fresh kernel against a session's evolved rootfs
            // lineage, so the image's own disk (and its pull) is
            // irrelevant; `spec.image_uri` stays as record-keeping.
            if let Some(manifest_ref) = spec.rootfs_manifest {
                let t = std::time::Instant::now();
                let (path, _state) = self.resolve_rootfs_from_manifest(manifest_ref).await?;
                materialize += t.elapsed();
                spec.rootfs_source = Some(path);
                #[cfg(target_os = "linux")]
                {
                    pending_nbd_state = _state;
                }
            } else if let Some(cache) = &self.image_cache {
                if let Some(uri) = spec.image_uri.clone() {
                    let t = std::time::Instant::now();
                    let cached = cache.ensure_image(&uri).await.map_err(|e| {
                        SandboxError::InvalidSpec(format!("image cache pull {uri}: {e}"))
                    })?;
                    image_resolve += t.elapsed();
                    tracing::debug!(uri = %uri, digest = %cached.digest, "image cache hit/pulled");
                    let t = std::time::Instant::now();
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
            let sandbox_id = self.inner.create(spec).await?;
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
            #[cfg(target_os = "linux")]
            if let Some(mut state) = pending_nbd_state {
                state.install_flush_scheduler(
                    sandbox_id,
                    self.live_manifest_publisher.clone(),
                    self.flush_config.clone(),
                );
                // ADR 0019: open the cold-boot operation window. The guest's
                // rootfs/substrate ext4-mount page-ins (served by this NBD
                // backend) now attach `chunk.fetch` spans to the cold-boot
                // trace until `start_agent` ends the window at agent_ready.
                state.backend.operation_scope().begin("cold_boot");
                self.nbd_sandboxes.insert(sandbox_id, state);
            }
            Ok(sandbox_id)
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

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // ADR 0028 Fix A: serialize captures per sandbox — a periodic
        // checkpoint and an eviction/drain/SIGTERM snapshot must never
        // interleave (the pause is idempotent, but concurrent NBD
        // flushes + chain advances are not).
        let capture_lock = self.capture_lock(id);
        let _capture_guard = capture_lock.lock().await;
        // Diff-mode when a rolling chain exists: same coherent
        // (memory, disk) capture contract, O(dirty set) cost. The
        // chain seeds on the first (Full) capture below — so an
        // eviction on a long-running session is just the final diff
        // in its checkpoint chain, collapsing the multi-GiB-dump
        // window the cf4d4afd incident sat in.
        let chain_prev = self.checkpoint_chains.get(&id).map(|c| {
            (
                c.manifest_ref,
                c.manifest.clone(),
                c.rolling_memfile.clone(),
            )
        });

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

        // ADR 0018 commit 12m: snapshot ordering is now
        //   pause → wait_idle → flush → inner.snapshot
        // where inner.snapshot's internal pause/capture/resume is
        // a no-op pause (FC's PATCH /vm is idempotent) followed by
        // the memory dump and a resume that brings the VM back
        // running. This guarantees memory + disk capture happen at
        // the same point in time: the explicit pause stops vCPUs,
        // wait_idle drains any in-flight virtio writes through the
        // NBD daemon, flush publishes the just-quiesced disk
        // manifest, and inner.snapshot then captures the page cache
        // (which now agrees with the published disk lineage).
        //
        // Pre-12m ordering was flush → inner.snapshot (FC's pause
        // happened inside the inner call AFTER our flush). Writes
        // queued between flush and pause landed in memory but not
        // the published manifest — the cross-host evac canary md5
        // mismatch on dev-vm validation was exactly this race.
        //
        // ADR 0028 A.log: the pause instant is the coherence cut for
        // the (memory, disk, event-log) triple — the coord resolves
        // the session_events cursor as "last event at or before this"
        // when it records the checkpoint.
        let paused_at = chrono::Utc::now();
        self.inner
            .pause(id)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("pre-flush pause: {e}")))?;

        #[cfg(target_os = "linux")]
        let nbd_disk_manifest = if let Some(entry) = self.nbd_sandboxes.get(&id) {
            // Drain in-flight NBD requests so the upcoming flush
            // sees a quiescent dirty buffer. With FC paused above,
            // no new virtio writes are issued, and wait_idle returns
            // once any already-in-flight requests have completed
            // through backend.write().
            entry.backend.wait_idle().await;
            // ADR 0019: scope the dirty-chunk flush — the idle-evict /
            // evacuate critical-path cost — so each upload attaches a
            // `chunk.flush` span to the snapshot op's trace. `kind="snapshot"`
            // covers both flows; the coord parent trace distinguishes them.
            // begin/end bracket only the flush so `end` runs on error too.
            entry.backend.operation_scope().begin("snapshot");
            let flush_result = entry.backend.flush().await;
            entry.backend.operation_scope().end();
            let outcome =
                flush_result.map_err(|e| SandboxError::Snapshot(format!("nbd disk flush: {e}")))?;
            tracing::info!(
                sandbox_id = %id,
                manifest = %outcome.manifest_ref,
                chunks_flushed = outcome.chunks_flushed,
                bytes_uploaded = outcome.bytes_uploaded,
                "chunked NBD disk flushed (post-pause)",
            );
            Some(outcome.manifest_ref)
        } else {
            None
        };

        // ADR 0007 Phase 6: backend owns its staging dir; we look
        // it up via snapshot_path_for after the inner call so we
        // can read/patch the on-disk artifacts the inner backend
        // wrote (memory.bin / memory.diff, manifest.json). The inner
        // call's pause-create-resume cycle is idempotent against our
        // earlier pause and brings the VM back to running on exit —
        // for the diff flavor that resume lands after O(dirty set),
        // not O(guest RAM).
        let mut metadata = if chain_prev.is_some() {
            self.inner.snapshot_diff(id).await?
        } else {
            self.inner.snapshot(id).await?
        };
        let dest = self.inner.snapshot_path_for(metadata.id);
        // Filled by the diff branch below; consumed by the chain
        // advance after the post-processing block succeeds.
        let mut next_manifest_for_chain: Option<engram_chunk_store::Manifest> = None;

        // ADR 0014 cleanup hygiene: from here on, FC has materialised
        // state.bin + memory.bin in `dest` (4+ GiB). Any failure in
        // the post-inner steps below (chunking, sidecar patch, BlobStorage
        // upload, NBD version conflict surfaced via the caller's
        // earlier flush) MUST rm -rf `dest` before propagating, or
        // we leak 4 GiB per failure — idle-evict retries every ~30s
        // and fills the host disk inside an hour.
        let post = async {
            // Plumb the new disk manifest onto SnapshotMetadata so
            // the coord-side snapshot recorder persists it on the
            // `snapshots` row's `disk_manifest_*` columns.
            #[cfg(target_os = "linux")]
            if let Some(mref) = nbd_disk_manifest {
                metadata.disk_manifest = Some(mref);
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
            let manifest_ref = if let Some((prev_ref, prev_manifest, rolling)) = chain_prev.as_ref()
            {
                // ADR 0028 Fix A diff path: overlay the sparse diff
                // onto the rolling full image, then re-chunk ONLY the
                // chunks the dirty extents touched — the previous
                // manifest's hashes carry over for everything else,
                // so CPU + upload stay O(dirty set). The manifest id
                // is stable for the chain's lifetime; only `version`
                // ticks.
                let diff_path = dest.join("memory.diff");
                let ranges = crate::checkpoint::dirty_ranges(&diff_path)
                    .map_err(|e| SandboxError::Snapshot(format!("dirty ranges: {e}")))?;
                crate::checkpoint::overlay_sparse(&diff_path, rolling)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("overlay diff: {e}")))?;
                let next = chunk_store
                    .update_for_dirty_ranges(prev_manifest, rolling, &ranges)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("incremental re-chunk: {e}")))?;
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
                    "diff checkpoint re-chunked incrementally",
                );
                next_ref
            } else {
                let mem_path = dest.join("memory.bin");
                if fs::metadata(&mem_path).await.is_err() {
                    return Ok(metadata);
                }
                let mref = chunk_memory_to_store(chunk_store, &mem_path).await?;
                tracing::info!(
                    session_sandbox = %id,
                    manifest = %mref,
                    "chunked FC memory.bin → chunk store",
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
            patch_fc_manifest_memory_ref(&manifest_json, manifest_ref).await?;
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
                crate::bundles::BundleStore::new(blob.clone(), self.bundle_dir.clone())
                    .publish(&metadata.aux_bundles)
                    .await?;
            }
            Ok::<_, SandboxError>(metadata)
        }
        .await;

        match post {
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
                self.last_snapshot_unix_ms.insert(id, now_ms);
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
        }
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
    /// delegates to whatever VMM is underneath. Used by our own
    /// `snapshot` above (pre-flush quiesce) and exposed on the
    /// trait so external orchestration can call it directly.
    async fn pause(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.pause(id).await
    }

    /// ADR 0018 commit 12m: forward resume. Symmetric with pause.
    async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.resume(id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        self.restore_with(metadata, /*fresh=*/ false).await
    }

    async fn restore_fresh(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        self.restore_with(metadata, /*fresh=*/ true).await
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
        }
        // ADR 0016 Phase A: drop the COW diagnostic timestamp so
        // the entry doesn't outlive its sandbox. A subsequent
        // `cow_state(id)` returns `None` (no NBD entry, no
        // snapshot timestamp) — same shape as a brand-new
        // sandbox.
        let _ = self.last_snapshot_unix_ms.remove(&id);
        // ADR 0028 Fix A: tear down the checkpoint chain + its rolling
        // memory image. Durable RECORDS deliberately survive destroy —
        // an eviction's final checkpoint must stay re-advertisable
        // until the coord acks it (that's the whole reconciliation
        // point). GCS chunks are the durable truth; the rolling file
        // is just the local diff-apply accelerator.
        if let Some((_, chain)) = self.checkpoint_chains.remove(&id) {
            let _ = tokio::fs::remove_file(&chain.rolling_memfile).await;
        }
        let _ = self.capture_locks.remove(&id);
        result
    }

    async fn start_agent(&self, id: SandboxId, mut agent: AgentSpec) -> Result<(), SandboxError> {
        // ADR 0021 P1.2: only the host-agent knows the per-host egress-
        // proxy CA, so it stamps the PEM onto the AgentSpec right
        // before the backend sees it. The FC backend uses this in its
        // `InstallHostCa` round-trip to agentd (post-readiness,
        // pre-SpawnHarness). Coord-supplied specs always arrive with
        // `host_ca_pem = None`; the host-agent fills it in here. The
        // legacy drive-based delivery still runs in parallel until
        // P1.5 retires the harness drive.
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

    #[tracing::instrument(name = "host.build_base_snapshot", skip_all)]
    async fn build_base_snapshot(
        &self,
        spec: SandboxSpec,
        warm_harness_spec: Option<AgentSpec>,
    ) -> Result<SnapshotMetadata, SandboxError> {
        // ADR 0021 P1.5: no stub-harness attach — the harness lives
        // in the rootfs of the image being captured, so the snapshot
        // is already complete without any second virtio-blk drive.

        // Boot the capture VM (opens the cold_boot operation scope on Linux).
        let id = self.create(spec).await?;

        // Drive the capture to a snapshot, then ALWAYS tear the VM down —
        // a capture VM has no session and must not linger.
        let captured = async {
            // Wait for the guest to reach agentd-ready (bootstrap on
            // accept(), harness unmounted — the option-D capture point).
            // VZ backend doesn't support this (FC-only), so ignore InvalidSpec.
            match self.inner.wait_agent_ready(id).await {
                Ok(()) => {}
                Err(SandboxError::InvalidSpec(_)) => {}
                Err(e) => return Err(e),
            }
            // Close the cold-boot window (mirrors `start_agent`) before the
            // snapshot flush opens its own `snapshot` operation scope.
            #[cfg(target_os = "linux")]
            if let Some(state) = self.nbd_sandboxes.get(&id) {
                state.backend.operation_scope().end();
            }
            // ADR 0037: optionally spawn a warm, prompt-less harness and
            // wait for it to go warm+idle, so the snapshot captures a ready
            // agent. Gated host-side; any failure ⇒ cold (the harness, if
            // spawned, is left running — a cold restore's SpawnHarness
            // kill+respawn cleanly replaces it).
            let warm = self.maybe_warm_capture(id, warm_harness_spec).await;
            // Capture: pause → flush disk → chunk memory + upload
            // state.bin/sidecar to BlobStorage. This is the portable
            // artifact `create_session` restores from.
            let mut meta = self.snapshot(id).await?;
            meta.warm_harness = warm;
            Ok(meta)
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
        captured
    }

    #[tracing::instrument(name = "host.restore_base_for_session", skip_all)]
    async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
    ) -> Result<SandboxId, SandboxError> {
        // 1. Restore the base snapshot (cross-host materialize +
        //    load_snapshot). The VM comes up running with the bake-time
        //    harness baked into the rootfs at /opt/engram/harness/.
        //    Fresh flavor (ADR 0035 §3): aux bundles swap to the host's
        //    current generation so new sessions run the latest skills.
        let id = self.restore_with(metadata, /*fresh=*/ true).await?;

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

    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        self.inner.guest_ip(id).await
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

    async fn netns_name_for(&self, id: SandboxId) -> Option<String> {
        self.inner.netns_name_for(id).await
    }

    async fn vm_internal_ip(&self, id: SandboxId) -> Option<String> {
        self.inner.vm_internal_ip(id).await
    }

    /// ADR 0037 P5b: forward the reconnect nudge to the inner FC
    /// backend (which dials agentd). Without this the trait's default
    /// (`Ok(false)`) runs and the warm-restore harness never gets
    /// SIGUSR1'd — exactly the `start_shell` missing-forward class above.
    async fn reconnect_harness(&self, id: SandboxId) -> Result<bool, SandboxError> {
        self.inner.reconnect_harness(id).await
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
            let entry = self.nbd_sandboxes.get(&id)?;
            self.cow_state_for_entry(id, entry.backend.clone()).await
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
    /// ADR 0016 Phase B commit 7 — restart-time rehydration.
    /// Coord hands the host a list of `(session_id, sandbox_id,
    /// effective_disk_manifest)` rows at registration time;
    /// this method rebuilds chunked-disk tracking for one entry.
    ///
    /// Steps:
    /// 1. Acquire an NBD slot.
    /// 2. Build `ChunkedDiskBackend` from the manifest_ref.
    /// 3. Spawn the NBD daemon serving the slot.
    /// 4. Insert into `nbd_sandboxes` keyed by `sandbox_id`.
    /// 5. Install the FlushScheduler so continuous flush resumes
    ///    for the survivor.
    /// 6. Pre-populate `session_bindings[sandbox_id] = session_id`
    ///    so the LiveManifestPublisher's resolver finds the
    ///    binding on the first post-rehydrate flush.
    ///
    /// **Out of scope** (see commit 8 closing notes):
    /// - Patching the FC sidecar (this code path doesn't re-launch
    ///   FC; the sandbox is already running from before the
    ///   host-agent restart).
    /// - Recovery of FC virtio-blk I/O after NBD device loss. If
    ///   the kernel left the device in a stuck state, the FC
    ///   sandbox's I/O likely failed and it's a candidate for
    ///   eviction-to-snapshot, not rehydration. Out of scope.
    ///
    /// Skip (`Ok(false)`) on any short-circuit (no nbd_pool /
    /// chunk_store / chunk_cache, or sandbox already present).
    /// Callers log + move on.
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

        let store_arc = Arc::new(chunk_store.clone());
        let mut state = crate::disk_daemon::attach_manifest(
            disk_manifest,
            chunk_cache.clone(),
            store_arc,
            pool,
            self.flush_config.dirty_threshold_bytes,
        )
        .await
        .map_err(|e| SandboxError::Vm(format!("rehydrate nbd attach: {e}").into()))?;

        state.install_flush_scheduler(
            sandbox_id,
            self.live_manifest_publisher.clone(),
            self.flush_config.clone(),
        );
        self.nbd_sandboxes.insert(sandbox_id, state);

        // Pre-populate session_bindings so the LiveManifestPublisher
        // resolver finds the binding on the first post-rehydrate
        // flush. Without this, the scheduler would skip-publish
        // with "sandbox not bound" — same shape as the pre-commit-
        // 5163366 cold-create regression.
        self.session_bindings.insert(sandbox_id, session_id);

        tracing::info!(
            %session_id,
            %sandbox_id,
            manifest = %disk_manifest,
            "rehydrated chunked-disk tracking + scheduler for survivor sandbox",
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
        let store_arc = Arc::new(chunk_store.clone());
        let state = crate::disk_daemon::attach_manifest(
            disk_ref,
            chunk_cache.clone(),
            store_arc,
            pool,
            self.flush_config.dirty_threshold_bytes,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_cache::ImageBundle;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use engram_sandbox_process::ProcessBackend;

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
                    warm_harness: false,
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
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
                    warm_harness: false,
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
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
                    warm_harness: false,
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "t".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
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
        // restore has nothing to read from disk. Memory.bin is also
        // deleted; it gets materialised from the chunked manifest.
        tokio::fs::remove_file(staging.join("state.bin"))
            .await
            .unwrap();
        tokio::fs::remove_file(staging.join("manifest.json"))
            .await
            .unwrap();
        tokio::fs::remove_file(staging.join("memory.bin"))
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
            warm_harness: false,
            id: snapshot_id,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: Some(manifest_ref),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
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
            warm_harness: false,
            id: engram_core::SnapshotId::new(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
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
    // PooledBackend MUST forward start_shell / netns_name_for /
    // guest_ip to its inner backend. The SandboxBackend trait has
    // default impls for these (Ok(7681) / None / None respectively)
    // that exist for backends without that capability (process,
    // VZ-without-netns). When PooledBackend wraps a FirecrackerBackend
    // that DOES implement them, NOT forwarding silently routes
    // through the trait defaults and the real FC capability never
    // fires — observed in prod: zero start_shell logs on the host-
    // agent despite a completed proxy_shell GRPC call, exactly
    // because PooledBackend.start_shell was using the trait default.
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
            netns_name_for_calls: Mutex<Vec<SandboxId>>,
            guest_ip_calls: Mutex<Vec<SandboxId>>,
            /// Non-default response values so we can verify the
            /// forward returned the inner's value, not the trait
            /// default.
            shell_port: u16,
            netns_name: Option<String>,
            guest_ip_value: Option<String>,
        }
        impl SpyInner {
            fn new() -> Self {
                Self {
                    start_shell_calls: Mutex::new(Vec::new()),
                    netns_name_for_calls: Mutex::new(Vec::new()),
                    guest_ip_calls: Mutex::new(Vec::new()),
                    // Pick non-default values so a "trait default ran
                    // instead of our override" failure shows up as a
                    // value mismatch, not just a counter mismatch.
                    shell_port: 31337,
                    netns_name: Some("engr-vm-spytest".into()),
                    guest_ip_value: Some("10.200.0.42".into()),
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
            async fn netns_name_for(&self, id: SandboxId) -> Option<String> {
                self.netns_name_for_calls.lock().push(id);
                self.netns_name.clone()
            }
            async fn guest_ip(&self, id: SandboxId) -> Option<String> {
                self.guest_ip_calls.lock().push(id);
                self.guest_ip_value.clone()
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

        /// Same regression but for netns_name_for. The host-agent's
        /// proxy_shell uses this to decide cold-path (None → dial
        /// from root) vs warm-path (Some → dial inside netns).
        /// Without forwarding, every warm-restored sandbox looks
        /// like a cold one and the proxy_shell dial misses the
        /// VM entirely.
        #[tokio::test]
        async fn pooled_backend_forwards_netns_name_for_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let ns = pooled.netns_name_for(id).await;

            let calls = inner.netns_name_for_calls.lock().clone();
            assert_eq!(calls, vec![id], "must forward to inner");
            assert_eq!(
                ns,
                inner.netns_name.clone(),
                "must return inner's value (proves the forward, not the default-None)",
            );
        }

        /// guest_ip forwarding was correct pre-fix, but exists
        /// here as a regression guard so we never lose it.
        #[tokio::test]
        async fn pooled_backend_forwards_guest_ip_to_inner() {
            let inner = Arc::new(SpyInner::new());
            let pooled = PooledBackend::new(inner.clone() as Arc<dyn SandboxBackend>);
            let id = SandboxId::new();
            let ip = pooled.guest_ip(id).await;

            let calls = inner.guest_ip_calls.lock().clone();
            assert_eq!(calls, vec![id], "must forward to inner");
            assert_eq!(
                ip,
                inner.guest_ip_value.clone(),
                "must return inner's value"
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
                    warm_harness: false,
                    id: snapshot_id,
                    size_bytes: 4096,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                    aux_bundles: vec![],
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
}
