//! ADR 0016 Phase B — continuous disk-flush scheduler.
//!
//! One scheduler per chunked-disk-backed sandbox. The loop awakens
//! on either a periodic tick or a threshold-crossing `Notify` from
//! `ChunkedDiskBackend::ensure_dirty` (Phase B commit 1), calls
//! `backend.flush()`, and — if any chunks were flushed — hands the
//! freshly-published `ManifestRef` to a [`LiveManifestPublisher`].
//!
//! The publisher trait owns the sandbox→session mapping and the
//! coord-side POST. The scheduler itself doesn't know about session
//! ids; this decoupling is what lets the warm pool (when it
//! returns) share the same scheduler API without runtime-mutable
//! state on the scheduler. A warm-pool sandbox spawns its scheduler
//! at create time; the publisher consults its own sandbox→session
//! index and skips the POST while the session binding is absent.
//! Cold-create + resume + restart-rehydration follow the same shape
//! — the scheduler is dumb, the publisher is the one that knows
//! whether to publish.
//!
//! Lifetime: the returned [`FlushSchedulerHandle`] aborts the task
//! on `Drop`. Field order in `NbdSandboxState` puts the handle
//! first so the abort fires BEFORE the NBD daemon disconnects (the
//! task holds an `Arc<ChunkedDiskBackend>`, so a mid-flight
//! `flush()` survives the handle abort and runs to completion; the
//! Arc keeps the backend alive across `nbd_sandboxes.remove`. The
//! resulting publish lands harmlessly because coord's
//! `sessions.sandbox_id == publish.sandbox_id` guard drops stale
//! publishes from destroyed bindings — see commit 3.)
//!
//! ## Interaction with the idle evictor (ADR 0016 + A.1.5/A.1.6)
//!
//! The scheduler runs in parallel with the eviction pipeline. Both
//! call `ChunkedDiskBackend::flush()` on the same backend (and the
//! snapshot path runs the split `flush_local` + `flush_upload`).
//! Issue #199: the backend's `flush_pipeline` mutex serializes the
//! WHOLE drain→upload→publish→rebase of each flush, so two flushes
//! can never rebase a chunk out of order. (The earlier claim that
//! the `dirty` mutex sufficed was wrong — it only serialized the
//! drain; a slow upload from an earlier drain could publish/rebase
//! AFTER a later drain already did, overwriting the newer chunk with
//! the older hash and dropping acked writes.) The flush that drains
//! first publishes first; whichever drains last sees the empty
//! buffer (`chunks_flushed = 0`, skips the publish) or carries the
//! superseding bytes. `manifest_ref` retry on `VersionConflict`
//! (ADR 0014 #7) makes back-to-back flushes idempotent.
//!
//! The subtle race is **post-snapshot writes that escape via the
//! scheduler**. Inside the evict pipeline, `host.snapshot()`
//! does pause→state.bin→resume (FC is RUNNING after that point),
//! then the pipeline proceeds to `record_snapshot` → unbind →
//! `transition_session(Idle)` → `destroy()`. Between FC's post-
//! snapshot resume and `destroy()`, the guest can issue writes
//! that the scheduler then flushes and publishes as a `v_n+1` on
//! `sessions.live_disk_manifest_*`. On resume, the
//! `effective_resume_disk_manifest` resolver (commit 6) would pick
//! `v_n+1` over the snapshot's `v_n`, restoring a disk that is
//! AHEAD of the memory snapshot — inconsistent state.
//!
//! Mitigation lives in commit 3: `transition_session(Idle)` clears
//! `live_disk_manifest_*` in the same transaction it flips
//! `status = Idle`, so any racing publish that landed between
//! `host.snapshot()` and `assign_session_sandbox(None)` is wiped.
//! The `sessions.sandbox_id == publish.sandbox_id` guard ALSO
//! drops post-step-4 publishes. Two defences in series; the Idle-
//! clear is the load-bearing one for the pre-step-4 window.
//!
//! The scheduler does NOT participate in `inflight_snapshots` (ADR
//! 0014 #1/#2) or A.1.5a's host-side eviction-inflight gate —
//! those serialize eviction-driven snapshots, not background
//! flushes. The scheduler's flushes are short (drain → upload →
//! manifest tick) and don't pause FC, so there's no contention
//! with the eviction pipeline beyond the flush-pipeline-mutex
//! serialization above (issue #199).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::types::manifest::ManifestRef;
use engram_core::SandboxId;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::backend::{ChunkedDiskBackend, DiskBackendError};

/// Phase B production defaults. Env vars override per [`Self::from_env`].
#[derive(Clone, Debug)]
pub struct FlushSchedulerConfig {
    /// Periodic flush cadence. Default: 30 s
    /// (`ENGRAM_FLUSH_INTERVAL_SECS`).
    pub interval: Duration,
    /// Dirty-bytes threshold that pokes the backend's `Notify` and
    /// pre-empts the periodic tick. Default: 256 MiB
    /// (`ENGRAM_FLUSH_DIRTY_THRESHOLD_MIB`). The scheduler doesn't
    /// read this field at runtime — the backend does — but it's
    /// carried here because the same config governs both knobs and
    /// the caller threads it into `ChunkedDiskBackend::from_blob`.
    pub dirty_threshold_bytes: u64,
    /// Kill switch. `false` → callers SHOULD NOT call
    /// [`FlushScheduler::spawn`]. `true` (default) → scheduler
    /// runs. Operator override: `ENGRAM_CONTINUOUS_FLUSH_DISABLED=1`.
    pub enabled: bool,
}

impl Default for FlushSchedulerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            dirty_threshold_bytes: super::backend::DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        }
    }
}

impl FlushSchedulerConfig {
    /// Resolve env-var overrides. Bad/absent values fall back to
    /// [`Default`]. The host-agent's main constructs one of these
    /// once at startup and clones into every sandbox-create path.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Some(secs) = std::env::var("ENGRAM_FLUSH_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            cfg.interval = Duration::from_secs(secs);
        }
        if let Some(mib) = std::env::var("ENGRAM_FLUSH_DIRTY_THRESHOLD_MIB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            cfg.dirty_threshold_bytes = mib.saturating_mul(1024 * 1024);
        }
        if std::env::var("ENGRAM_CONTINUOUS_FLUSH_DISABLED")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            cfg.enabled = false;
        }
        cfg
    }
}

/// Decouples the scheduler from the host→coord wire shape. The
/// no-op impl ([`NoOpLiveManifestPublisher`]) lets commit 2 land
/// without coord plumbing; the real impl lands in commit 4 and
/// closes over the `HttpCoordClient`'s coalescing mpsc.
///
/// **`sandbox_id`, not `session_id`**: the publisher is responsible
/// for resolving the session binding (via the host-agent's
/// `session_bindings: DashMap<SandboxId, SessionId>` from ADR 0017
/// or any equivalent index) and short-circuiting the POST when the
/// sandbox is unbound (e.g. warm-pool, post-`destroy` race). This
/// shape lets warm-pool sandboxes share the same scheduler without
/// re-spawning the scheduler on assignment.
#[async_trait]
pub trait LiveManifestPublisher: Send + Sync {
    async fn publish(&self, sandbox_id: SandboxId, manifest_ref: ManifestRef);
}

/// Default impl until commit 4 wires the real publisher. Useful
/// for tests that exercise scheduler lifecycle without coord.
pub struct NoOpLiveManifestPublisher;

#[async_trait]
impl LiveManifestPublisher for NoOpLiveManifestPublisher {
    async fn publish(&self, _sandbox_id: SandboxId, _manifest_ref: ManifestRef) {}
}

/// ADR 0116 C4: escalation sink for a persistently failed data plane.
/// The scheduler calls this once per failure EPISODE — after
/// [`FLUSH_ESCALATION_THRESHOLD`] consecutive device-class flush
/// failures — not once per failed flush. The production impl
/// (PooledBackend) records the sandbox as a quarantined survivor so
/// the coordinator's quarantine-evict ladder owns the remediation.
/// The 2026-08-12 incident is the motivation: a host-side flush
/// failed every 30 s for 7+ hours with only a warn log, and a rung-2
/// park then froze a device with nothing durable captured.
#[async_trait]
pub trait DataPlaneHealth: Send + Sync {
    async fn data_plane_failed(&self, sandbox_id: SandboxId);
}

/// Default impl for tests and callers without quarantine plumbing.
pub struct NoOpDataPlaneHealth;

#[async_trait]
impl DataPlaneHealth for NoOpDataPlaneHealth {
    async fn data_plane_failed(&self, _sandbox_id: SandboxId) {}
}

/// ADR 0116 C4: consecutive device-class flush failures that fire the
/// escalation. At the default 30 s cadence this is ~2 minutes of a
/// continuously failed data plane — long enough to skip a transient
/// error burst, short enough that the quarantine ladder engages well
/// before the wedge can silently age.
const FLUSH_ESCALATION_THRESHOLD: u32 = 4;

/// ADR 0116 C4 — pure escalation state, split from the scheduler loop
/// per the `spawn()`/`run_once()` discipline (ADR 0098) so the
/// fire-exactly-once ladder is testable without the timer.
///
/// `fired` latches after one escalation so a permanently dead device
/// does not re-fire on every subsequent wake; a successful flush
/// resets the count AND re-arms the latch, so a heal→relapse cycle
/// escalates again.
#[derive(Debug, Default)]
struct FlushHealthTracker {
    consecutive: u32,
    fired: bool,
}

impl FlushHealthTracker {
    /// Feed one flush outcome. Returns `true` exactly once per
    /// failure episode: when the consecutive device-class failure
    /// count reaches [`FLUSH_ESCALATION_THRESHOLD`] and this episode
    /// has not fired yet. `device_class_failure: false` (a successful
    /// flush, or a non-device-class error) resets the ladder.
    fn on_flush_result(&mut self, device_class_failure: bool) -> bool {
        if !device_class_failure {
            self.consecutive = 0;
            self.fired = false;
            return false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= FLUSH_ESCALATION_THRESHOLD && !self.fired {
            self.fired = true;
            return true;
        }
        false
    }
}

/// ADR 0116 C4 (#1238 review, HIGH): does this flush error mean the
/// LOCAL durable-write plane cannot land bytes — as opposed to the
/// blob tier? Classified by WHICH TIER failed, not by hand-picked
/// variants:
///
/// - `Chunk(_)` is the blob tier — GCS throttling/outages heal on
///   their own and must NOT quarantine the sandbox. The ONLY
///   non-escalating class.
/// - Everything else on the flush path means nothing durable is
///   landing locally: `DeviceSync` (host page-cache sync of
///   `/dev/nbdN` failed), `ShortChunk` (a chunk cannot be served
///   in-bounds), `DirtyFile` (the dirty tier's own I/O — a read-only
///   remount or EIO fails every 30 s tick identically, forever),
///   `InvariantViolation` (including the poisoned tier after a freeze
///   double-fault, which persists until process restart), and the
///   shape/range arms that a flush should never produce (four
///   identical consecutive ones = the same wedge). The exhaustive
///   match makes a NEW variant a compile error, forcing the tier
///   decision instead of silently defaulting to the warn-loop this
///   module exists to eliminate.
fn is_device_class(e: &DiskBackendError) -> bool {
    match e {
        DiskBackendError::Chunk(_) => false,
        DiskBackendError::DeviceSync { .. }
        | DiskBackendError::ShortChunk { .. }
        | DiskBackendError::DirtyFile { .. }
        | DiskBackendError::InvariantViolation(_)
        | DiskBackendError::WrongKind(_)
        | DiskBackendError::InvalidManifest(_)
        | DiskBackendError::OutOfRange { .. }
        | DiskBackendError::AdoptShape { .. } => true,
    }
}

/// Owns the scheduler task. `Drop` aborts the task; the spawned
/// task is structured so abort-after-flush-completion is safe (it
/// holds an `Arc<ChunkedDiskBackend>` and `tokio::spawn::abort()`
/// pre-empts at the next await point, not mid-syscall).
pub struct FlushSchedulerHandle {
    task: JoinHandle<()>,
}

impl Drop for FlushSchedulerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Entrypoint. One sandbox = one call.
pub struct FlushScheduler;

impl FlushScheduler {
    /// Spawn the scheduler loop. The caller is responsible for
    /// honouring `config.enabled` (this method does NOT consult
    /// it) so the kill-switch decision lives at the policy site,
    /// not buried in spawn. The returned handle's `Drop` aborts the
    /// task; place it ordered-first in any owning struct so abort
    /// runs before NBD teardown.
    ///
    /// `health` is the ADR 0116 C4 escalation sink: fed once per
    /// persistent device-class failure episode (see
    /// [`FlushHealthTracker`]), never per failed flush.
    pub fn spawn(
        sandbox_id: SandboxId,
        backend: Arc<ChunkedDiskBackend>,
        publisher: Arc<dyn LiveManifestPublisher>,
        health: Arc<dyn DataPlaneHealth>,
        config: FlushSchedulerConfig,
    ) -> FlushSchedulerHandle {
        let threshold_notify = backend.threshold_notify();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(config.interval);
            // `tokio::time::interval` fires immediately on first
            // tick. `Delay` skips backlogged ticks rather than
            // firing them in a burst — preserves our cadence after
            // a flush takes longer than `interval`.
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // Consume the immediate first tick so we don't issue a
            // flush at t=0 (nothing dirty yet at spawn).
            interval.tick().await;
            let mut tracker = FlushHealthTracker::default();
            loop {
                tokio::select! {
                    _ = interval.tick() => {},
                    _ = threshold_notify.notified() => {},
                }
                let outcome = match backend.flush().await {
                    Ok(o) => {
                        // ADR 0116 C4: a successful flush ends the
                        // failure episode and re-arms the escalation —
                        // BEFORE the `chunks_flushed == 0` short-circuit
                        // below, so an empty-but-successful flush also
                        // heals the ladder.
                        tracker.on_flush_result(false);
                        o
                    }
                    Err(e) => {
                        // ADR 0116 C4: device-class failures escalate
                        // after FLUSH_ESCALATION_THRESHOLD consecutive
                        // misses. The 2026-08-12 incident's silent leg
                        // was exactly this warn, every 30 s, for 7+
                        // hours, with a rung-2 park then landing on the
                        // wedged device.
                        if tracker.on_flush_result(is_device_class(&e)) {
                            ::metrics::counter!(crate::metrics::NBD_FLUSH_ESCALATIONS_TOTAL)
                                .increment(1);
                            tracing::error!(
                                %sandbox_id,
                                error = %e,
                                consecutive_failures = FLUSH_ESCALATION_THRESHOLD,
                                "flush_scheduler: persistent device-class flush failure; \
                                 escalating to DataPlaneHealth (quarantine ladder)",
                            );
                            health.data_plane_failed(sandbox_id).await;
                        } else {
                            tracing::warn!(
                                %sandbox_id,
                                error = %e,
                                "flush_scheduler: backend.flush() failed; will retry on next wake",
                            );
                        }
                        continue;
                    }
                };
                if outcome.chunks_flushed == 0 {
                    // Nothing dirty; no manifest published. Skip
                    // the callback — coord shouldn't see a publish
                    // it can't act on. The flush still stamped
                    // `last_flush_unix_ms`; the diagnostic surface
                    // reads that.
                    continue;
                }
                tracing::debug!(
                    %sandbox_id,
                    manifest_id = %outcome.manifest_ref.manifest_id,
                    manifest_version = outcome.manifest_ref.version,
                    chunks_flushed = outcome.chunks_flushed,
                    bytes_uploaded = outcome.bytes_uploaded,
                    "flush_scheduler: flushed; publishing live manifest",
                );
                publisher.publish(sandbox_id, outcome.manifest_ref).await;
            }
        });
        FlushSchedulerHandle { task }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_daemon::backend::{ChunkedDiskBackend, DEFAULT_DIRTY_THRESHOLD_BYTES};
    use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
    use engram_chunk_store::manifest::{
        ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
    };
    use engram_chunk_store::store::ChunkStore;
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Mutex;
    use std::time::Duration;

    fn synth_manifest(total_bytes: u64, chunk_size: u64) -> Manifest {
        Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes,
            chunks: Vec::<ChunkRef>::new(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }

    type PublishLog = Arc<Mutex<Vec<(SandboxId, ManifestRef)>>>;

    /// Recording publisher: every `publish` call appends to a vec
    /// under a Mutex. Tests inspect the vec to assert publish
    /// cadence + payload.
    struct RecordingPublisher {
        events: PublishLog,
    }

    impl RecordingPublisher {
        fn new() -> (Self, PublishLog) {
            let events: PublishLog = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                },
                events,
            )
        }
    }

    #[async_trait]
    impl LiveManifestPublisher for RecordingPublisher {
        async fn publish(&self, sandbox_id: SandboxId, manifest_ref: ManifestRef) {
            self.events.lock().unwrap().push((sandbox_id, manifest_ref));
        }
    }

    async fn build_backend(threshold_bytes: u64) -> Arc<ChunkedDiskBackend> {
        let dir = tempfile::tempdir().unwrap();
        // Leak the dir guard for the test's life — the temp files
        // live until process exit; cheap.
        let path = dir.keep();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(path.clone()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = chunk_size * 8;
        let manifest = synth_manifest(total, chunk_size);
        let manifest_ref = engram_core::types::manifest::ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(path.join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, threshold_bytes)
                .unwrap();
        Arc::new(backend)
    }

    /// Threshold crossing wakes the scheduler before the periodic
    /// tick. A short test interval (1s) and a small threshold (8
    /// KiB; 2 chunks) prove the crossing path: two writes cross
    /// the threshold, the scheduler wakes early, flushes, and the
    /// publisher records the new manifest_ref.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn threshold_crossing_wakes_scheduler_before_tick() {
        let chunk_size = 4096u64;
        let threshold_bytes = chunk_size * 2;
        let backend = build_backend(threshold_bytes).await;
        let (publisher, events) = RecordingPublisher::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            // Long enough that a tick-driven flush wouldn't fire
            // within the test's lifetime — any flush we observe
            // came from the threshold path.
            interval: Duration::from_secs(60),
            dirty_threshold_bytes: threshold_bytes,
            enabled: true,
        };
        let _handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(NoOpDataPlaneHealth),
            config,
        );

        // Cross the threshold.
        backend.write(0, &[0xcc; 8]).await.unwrap();
        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();

        // Wait for the scheduler to observe the notify, flush, and
        // call the publisher. 500 ms is generous on a fast loop.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !events.lock().unwrap().is_empty() {
                break;
            }
        }
        let events = events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "publisher should have been called exactly once; saw {events:?}",
        );
        assert_eq!(events[0].0, sandbox_id, "publish carries the right sandbox");
    }

    /// Periodic tick flushes even without a threshold crossing. A
    /// 60 ms interval + 5 KiB threshold (a single chunk doesn't
    /// cross) proves the tick path independently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn periodic_tick_flushes_below_threshold() {
        let backend = build_backend(DEFAULT_DIRTY_THRESHOLD_BYTES).await;
        let (publisher, events) = RecordingPublisher::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            interval: Duration::from_millis(60),
            dirty_threshold_bytes: DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        };
        let _handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(NoOpDataPlaneHealth),
            config,
        );

        // One write — well below the 256 MiB default threshold —
        // so the only path to a publish is the periodic tick.
        backend.write(0, &[0xee; 8]).await.unwrap();

        // Wait up to ~500 ms for the tick to fire and publish.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !events.lock().unwrap().is_empty() {
                break;
            }
        }
        let events = events.lock().unwrap();
        assert!(
            !events.is_empty(),
            "periodic tick should have triggered at least one publish below threshold",
        );
    }

    /// The scheduler's periodic publish is a capture like any other:
    /// its flush must sync the served device before it freezes. The
    /// only write in this test sits in the modeled HOST page cache
    /// (the stub delivers it into the backend exclusively when the
    /// device is synced), so an observed publish PROVES the tick
    /// synced — an unsynced tick finds an empty dirty tier and never
    /// publishes. A survivor reattaches from these periodic manifests
    /// after an ungraceful host-agent death, so a torn periodic
    /// publish is the same corruption class as a torn snapshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn periodic_flush_syncs_the_host_device_before_freezing() {
        use engram_host_core::DeviceSync;
        use std::path::{Path, PathBuf};

        struct DeliverOnSync {
            backend: Mutex<Option<Arc<ChunkedDiskBackend>>>,
            pending: Mutex<Option<(u64, Vec<u8>)>>,
        }

        #[async_trait]
        impl DeviceSync for DeliverOnSync {
            async fn sync_device(&self, _path: &Path) -> std::io::Result<()> {
                let backend = self.backend.lock().unwrap().clone();
                let pending = self.pending.lock().unwrap().take();
                if let (Some(backend), Some((offset, data))) = (backend, pending) {
                    backend
                        .write(offset, &data)
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(())
            }
        }

        let backend = build_backend(DEFAULT_DIRTY_THRESHOLD_BYTES).await;
        let sync = Arc::new(DeliverOnSync {
            backend: Mutex::new(Some(backend.clone())),
            pending: Mutex::new(Some((0, vec![0xab; 8]))),
        });
        backend.register_host_device(PathBuf::from("/dev/nbd-test"), sync);

        let (publisher, events) = RecordingPublisher::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            interval: Duration::from_millis(60),
            dirty_threshold_bytes: DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        };
        let _handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(NoOpDataPlaneHealth),
            config,
        );

        // No direct `write()` in this test: the publish can only carry
        // the byte the tick's own sync delivered.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !events.lock().unwrap().is_empty() {
                break;
            }
        }
        assert!(
            !events.lock().unwrap().is_empty(),
            "the tick must sync the device and publish the delivered write",
        );
    }

    /// Empty flushes (no dirty chunks) don't publish. The scheduler
    /// short-circuits before the publisher when `chunks_flushed ==
    /// 0` — important for the per-session coalescing on the
    /// publisher side (commit 4): we don't want to enqueue a
    /// publish that has no manifest tick.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_flush_does_not_call_publisher() {
        let backend = build_backend(DEFAULT_DIRTY_THRESHOLD_BYTES).await;
        let (publisher, events) = RecordingPublisher::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            interval: Duration::from_millis(40),
            dirty_threshold_bytes: DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        };
        let _handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(NoOpDataPlaneHealth),
            config,
        );

        // Let several ticks fire with no writes.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            events.lock().unwrap().is_empty(),
            "publisher must not be called for chunks_flushed=0 flushes",
        );
    }

    /// Drop on the handle cancels the spawned task. After drop,
    /// the scheduler stops flushing — observable by writing after
    /// drop and waiting longer than `interval`, expecting no
    /// publish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_handle_cancels_scheduler() {
        let backend = build_backend(DEFAULT_DIRTY_THRESHOLD_BYTES).await;
        let (publisher, events) = RecordingPublisher::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            interval: Duration::from_millis(40),
            dirty_threshold_bytes: DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        };
        let handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(NoOpDataPlaneHealth),
            config,
        );
        drop(handle);

        // Cancellation has propagated by the next tick boundary.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Write something that WOULD have caused a publish if the
        // scheduler were still alive.
        backend.write(0, &[0xff; 8]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            events.lock().unwrap().is_empty(),
            "drop must cancel the spawned task; saw {} publishes post-drop",
            events.lock().unwrap().len(),
        );
    }

    /// ADR 0116 C4: the pure escalation ladder — no fire below the
    /// threshold, one fire at it, a latched episode never re-fires,
    /// and a success re-arms so a relapse fires again.
    #[test]
    fn flush_health_tracker_fires_once_at_threshold_and_rearms_on_success() {
        let mut t = FlushHealthTracker::default();
        for i in 0..3 {
            assert!(!t.on_flush_result(true), "failure {i} must not fire");
        }
        assert!(t.on_flush_result(true), "the 4th consecutive failure fires");
        assert!(
            !t.on_flush_result(true),
            "a latched episode must not re-fire on the 5th",
        );
        assert!(!t.on_flush_result(false), "a success never fires");
        for i in 0..3 {
            assert!(
                !t.on_flush_result(true),
                "relapse failure {i} must not fire early"
            );
        }
        assert!(
            t.on_flush_result(true),
            "after a success re-armed the ladder, the relapse fires again",
        );
    }

    /// ADR 0116 C4: only device-class errors escalate. The blob tier
    /// (`Chunk`) heals on its own (GCS throttling/outage) and must
    /// not quarantine the sandbox.
    #[test]
    fn device_class_classifier_separates_device_from_blob_tier() {
        use std::path::PathBuf;
        assert!(is_device_class(&DiskBackendError::DeviceSync {
            device: PathBuf::from("/dev/nbd9"),
            source: std::io::Error::other("sync failed"),
        }));
        assert!(is_device_class(&DiskBackendError::ShortChunk {
            chunk_idx: 3,
            expected: 4096,
            actual: 12,
        }));
        assert!(!is_device_class(&DiskBackendError::Chunk(
            engram_chunk_store::error::ChunkStoreError::HashMismatch {
                expected: "aa".into(),
                actual: "bb".into(),
            }
        )));
        // #1238 review (HIGH): local dirty-tier durability failures are
        // device-class — a read-only remount / EIO fails every tick
        // identically forever, and the poisoned tier
        // (InvariantViolation) persists until process restart. Treating
        // them as non-escalating reproduced the silent warn-loop AND
        // reset the counter, masking interleaved device failures.
        assert!(is_device_class(&DiskBackendError::DirtyFile {
            operation: "freeze rename",
            path: std::path::PathBuf::from("/var/lib/engram/dirty/9"),
            source: std::io::Error::other("read-only file system"),
        }));
        assert!(is_device_class(&DiskBackendError::InvariantViolation(
            "dirty tier poisoned by freeze double-fault".into()
        )));
        // Shape/range arms a flush should never produce: four identical
        // consecutive ones are the same wedge — escalate.
        assert!(is_device_class(&DiskBackendError::WrongKind(
            "memory".into()
        )));
        assert!(is_device_class(&DiskBackendError::OutOfRange {
            offset: 8192,
            length: 4096,
            total: 4096,
        }));
    }

    /// #1238 review (HIGH), the masking half: a non-blob failure must
    /// never RESET the consecutive count under an interleaved
    /// device-class run. With the tier-based classifier every local
    /// failure accumulates, so 2×DeviceSync + 2×DirtyFile fires at 4.
    #[test]
    fn mixed_local_failures_accumulate_to_escalation() {
        let mut t = FlushHealthTracker::default();
        assert!(!t.on_flush_result(true)); // DeviceSync
        assert!(!t.on_flush_result(true)); // DirtyFile (device-class now)
        assert!(!t.on_flush_result(true)); // DirtyFile
        assert!(
            t.on_flush_result(true),
            "the 4th consecutive local failure fires"
        );
    }

    type EscalationLog = Arc<Mutex<Vec<SandboxId>>>;

    /// ADR 0116 C4: recording `DataPlaneHealth` sink — every report
    /// appends the sandbox id.
    struct RecordingHealth {
        events: EscalationLog,
    }

    impl RecordingHealth {
        fn new() -> (Self, EscalationLog) {
            let events: EscalationLog = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                },
                events,
            )
        }
    }

    #[async_trait]
    impl DataPlaneHealth for RecordingHealth {
        async fn data_plane_failed(&self, sandbox_id: SandboxId) {
            self.events.lock().unwrap().push(sandbox_id);
        }
    }

    /// `DeviceSync` stub that always fails — a wedged device whose
    /// host page cache cannot sync (the backend.rs precedent).
    struct FailingDeviceSync;

    #[async_trait]
    impl engram_host_core::DeviceSync for FailingDeviceSync {
        async fn sync_device(&self, _path: &std::path::Path) -> std::io::Result<()> {
            Err(std::io::Error::other("injected sync failure"))
        }
    }

    /// `DeviceSync` stub that always succeeds — a healed device.
    struct HealthySync;

    #[async_trait]
    impl engram_host_core::DeviceSync for HealthySync {
        async fn sync_device(&self, _path: &std::path::Path) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// ADR 0116 C4: a persistently failing device sync escalates into
    /// the `DataPlaneHealth` sink exactly once — the 4th consecutive
    /// failed wake fires, later failed wakes stay latched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failing_device_sync_escalates_exactly_once() {
        let backend = build_backend(DEFAULT_DIRTY_THRESHOLD_BYTES).await;
        backend.write(0, &[0xcc; 8]).await.unwrap();
        backend.register_host_device(
            std::path::PathBuf::from("/dev/nbd-test"),
            Arc::new(FailingDeviceSync),
        );
        let (publisher, _publishes) = RecordingPublisher::new();
        let (health, escalations) = RecordingHealth::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            interval: Duration::from_millis(20),
            dirty_threshold_bytes: DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        };
        let _handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(health),
            config,
        );

        // Four failed wakes (~80 ms at the 20 ms interval) fire the
        // escalation; poll generously.
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !escalations.lock().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(
            escalations.lock().unwrap().as_slice(),
            &[sandbox_id],
            "the threshold crossing must report exactly once, with the sandbox id",
        );

        // ~15 more failed wakes: the latch holds, no second report.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            escalations.lock().unwrap().len(),
            1,
            "a latched failure episode must not re-fire",
        );
    }

    /// ADR 0116 C4: healing the device re-arms the ladder — a later
    /// relapse escalates AGAIN, so a heal→relapse cycle cannot hide
    /// behind the first episode's latch. Heals via re-registering a
    /// healthy sync (the backend.rs heal precedent).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn healed_device_relapse_escalates_again() {
        let backend = build_backend(DEFAULT_DIRTY_THRESHOLD_BYTES).await;
        backend.write(0, &[0xcc; 8]).await.unwrap();
        backend.register_host_device(
            std::path::PathBuf::from("/dev/nbd-test"),
            Arc::new(FailingDeviceSync),
        );
        let (publisher, publishes) = RecordingPublisher::new();
        let (health, escalations) = RecordingHealth::new();
        let sandbox_id = SandboxId::new();
        let config = FlushSchedulerConfig {
            interval: Duration::from_millis(20),
            dirty_threshold_bytes: DEFAULT_DIRTY_THRESHOLD_BYTES,
            enabled: true,
        };
        let _handle = FlushScheduler::spawn(
            sandbox_id,
            backend.clone(),
            Arc::new(publisher),
            Arc::new(health),
            config,
        );

        // First failure episode.
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !escalations.lock().unwrap().is_empty() {
                break;
            }
        }
        assert_eq!(escalations.lock().unwrap().len(), 1, "first episode fires");

        // Heal the device. The next successful flush publishes the
        // held-back dirty write — the observable proof a success fed
        // the tracker and re-armed the latch.
        backend.register_host_device(
            std::path::PathBuf::from("/dev/nbd-test"),
            Arc::new(HealthySync),
        );
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if !publishes.lock().unwrap().is_empty() {
                break;
            }
        }
        assert!(
            !publishes.lock().unwrap().is_empty(),
            "the healed device must flush and publish the held-back write",
        );

        // Relapse: the device fails again; a second episode fires.
        backend.register_host_device(
            std::path::PathBuf::from("/dev/nbd-test"),
            Arc::new(FailingDeviceSync),
        );
        for _ in 0..400 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if escalations.lock().unwrap().len() >= 2 {
                break;
            }
        }
        assert_eq!(
            escalations.lock().unwrap().len(),
            2,
            "a relapse after a heal must escalate again",
        );
    }
}
