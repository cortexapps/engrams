//! The co-simulated host world (ADR 0098 R-CoSim, rung 1).
//!
//! [`CosimHost`] models one host-agent process at the coordinator↔host
//! boundary. Unlike [`engram_dst_host::SimHost`] — which owns a fixed slot
//! id-space and drives the host's disk/spool/NBD oracles in isolation —
//! this host is keyed by the **coordinator-minted** sandbox/session ids
//! (the coordinator owns the id-space at this boundary), and its job is to
//! run the REAL host-agent lifecycle flows the coordinator's verbs trigger:
//!
//! * **create / restore** → build a real [`ChunkedDiskBackend`] over the
//!   shared [`ChunkStore`] on a fresh private base manifest;
//! * **eviction capture** ([`CosimHost::snapshot_begin`]) → the REAL
//!   `EvictionFinalizer` capture leg: drain the dirty tier through the real
//!   `persist_disk_pending_chunks`, persist a real `EvictionFinalizeRecord`,
//!   take the capture lock, pause the VM;
//! * **finalize** ([`CosimHost::finalize_tick`]) → the REAL production loop
//!   body `run_eviction_finalize_attempt` (upload + disk-manifest publish +
//!   terminal destroy);
//! * **durable exec** → the REAL Firecracker host reader and REAL agentd
//!   journal handler over per-sandbox duplex connections; checkpoint steps
//!   quietly sever the modeled vsock transport;
//! * **teardown-reconcile** → the REAL
//!   [`reconcile_once`](engram_host_agent::teardown_reconcile::reconcile_once)
//!   over [`CosimReconcileBackend`].
//!
//! **Why not reuse `SimHost` wholesale (documented divergence, per the ADR
//! plan's "follow unless the code proves it wrong"):** `SimHost::new` seeds
//! a fixed set of sandboxes with pre-minted literal ids (`0x5B00_0000+i`)
//! and assumes it owns the session/sandbox id-space. At the co-sim boundary
//! the coordinator mints those ids (entropy stream) and the host must track
//! whatever the coordinator creates — so the top-level slot model is wrong
//! here. We reuse the *real extracted flows* it is built on (the
//! `EvictionFinalizer` machinery, `ChunkedDiskBackend`, `reconcile_once`,
//! [`SimFs`]) — just keyed by the boundary's id-space.
//!
//! **Concurrency.** [`CosimHost`] lives behind an `Arc<tokio::sync::Mutex>`
//! (the coordinator's async `HostClient` verbs and the host's async steps
//! both mutate it). The teardown-reconcile trait has SYNC methods, which
//! cannot take an async lock — so the reconcile-visible state (live set,
//! local bindings, capture-in-flight flags) is mirrored into a
//! `parking_lot`-guarded [`HostView`] the sync methods read, kept in
//! lockstep by every mutating host op. This mirrors `engram-dst-host`'s
//! own split (its `SimReconcileBackend` is a separate model seeded in
//! lockstep with the disk world).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use engram_agentd::exec_journal::{AttachOrStart, ExecJournal};
use engram_agentd::{
    serve_connection_with_journal, CaCertInstaller, CaCertPaths, HarnessSupervisor,
};
use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::manifest::{
    ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
};
use engram_chunk_store::store::ChunkStore;
use engram_core::error::SandboxError;
use engram_core::traits::{Clock as _, Entropy as _};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{AuxBundleRef, ExecRequest, SandboxAuxBundles};
use engram_core::types::SnapshotId;
use engram_core::{HostId, SandboxId, SessionId};
use engram_dst_host::device_plane::{DevicePlane, ServeOutcome};
use engram_dst_host::SimFs;
use engram_host_agent::disk_daemon::backend::ChunkedDiskBackend;
use engram_host_agent::disk_daemon::{NbdSlot, NbdSlotAllocator};
use engram_host_agent::eviction_finalize::{
    persist_disk_pending_chunks, run_eviction_finalize_attempt, DiskPendingRecord,
    EvictionFinalizeRecord, EvictionFinalizer, EvictionSandbox, FinalizeAttempt,
};
use engram_host_agent::teardown_reconcile::ReconcileBackend;
use engram_host_core::{FinalizeStage, HostFs, TokioFs};
use engram_sandbox_firecracker::BoxExecIo;
use engram_sim::{SimClock, SimEntropy};
use parking_lot::Mutex;

/// Disk geometry — small on purpose (the oracle asserts a boundary
/// property, not throughput).
pub const CHUNK_SIZE: u64 = 4096;
pub const NUM_CHUNKS: u64 = 8;
/// Small redrive cap so a quarantine arm stays cheap (prod is 10).
pub const FINALIZE_MAX_ATTEMPTS: u32 = 3;

/// The NBD device universe for one co-simulated host — small (the small-world
/// bounding, #784 rung 2): enough `/dev/nbdN` slots for every concurrent
/// sandbox plus a couple of spares for the slot-accounting exercise.
pub const DEVICE_CAPACITY: u32 = 8;

/// The staged-bundle file extension for the co-simulated host (the
/// `SandboxBackend::bundle_file_ext` analog — `squashfs` in prod, a plain
/// tag here). MUST match what [`CosimHost::finalizer`] passes the real
/// `EvictionFinalizer`, or the finalize's bundle-publish leg misses the
/// staged files.
pub const BUNDLE_EXT: &str = "bin";

/// Content bytes stamped with `tag` (first 8 bytes little-endian, tiled).
pub(crate) fn synth_chunk(tag: u64) -> Vec<u8> {
    let stamp = tag.to_le_bytes();
    let mut out = vec![0u8; CHUNK_SIZE as usize];
    for (i, b) in out.iter_mut().enumerate() {
        *b = stamp[i % 8];
    }
    out
}

/// The private base manifest: `NUM_CHUNKS` positional entries all deduped
/// onto the single tag-0 base chunk.
fn base_manifest(base_hash: ChunkHash) -> Manifest {
    Manifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        kind: ManifestKind::Disk,
        chunk_size: ChunkSize::bytes(CHUNK_SIZE),
        total_bytes: NUM_CHUNKS * CHUNK_SIZE,
        chunks: (0..NUM_CHUNKS)
            .map(|i| ChunkRef {
                offset: i * CHUNK_SIZE,
                hash: base_hash,
            })
            .collect(),
        parent: None,
        working_set_trace: None,
        annotations: Default::default(),
    }
}

async fn build_backend(
    store: &Arc<ChunkStore>,
    cache_root: &std::path::Path,
    key: &SandboxId,
    manifest_ref: ManifestRef,
) -> ChunkedDiskBackend {
    let mut cfg = ChunkCacheConfig::new(cache_root.join(format!("sandbox-{key}")));
    cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cfg);
    // u64::MAX threshold: flushes are explicit (never auto-threshold).
    ChunkedDiskBackend::from_blob(manifest_ref, cache, store.clone(), u64::MAX)
        .await
        .expect("build backend from base manifest")
}

/// Build a fresh base-manifest-backed sandbox WITHOUT the host lock held
/// (the concurrency rule above). The bridge calls this with cloned handles,
/// then commits the result under a brief synchronous lock.
pub(crate) async fn build_base_sandbox(
    store: Arc<ChunkStore>,
    cache_root: std::path::PathBuf,
    entropy: Arc<SimEntropy>,
) -> (SandboxId, ManifestRef, Arc<ChunkedDiskBackend>) {
    let sandbox_id = SandboxId::from(entropy.uuid());
    let base_hash = store
        .put_chunk(&synth_chunk(0))
        .await
        .expect("seed base chunk");
    let base_ref = ManifestRef {
        manifest_id: entropy.uuid(),
        version: 1,
    };
    store
        .put_manifest(base_ref, &base_manifest(base_hash))
        .await
        .expect("seed base manifest");
    let backend = build_backend(&store, &cache_root, &sandbox_id, base_ref).await;
    (sandbox_id, base_ref, Arc::new(backend))
}

/// The reconcile-visible slice of host state (parking_lot-guarded so the
/// SYNC [`ReconcileBackend`] methods can read it without an async lock).
#[derive(Default)]
pub struct HostView {
    inner: Mutex<HostViewInner>,
}

#[derive(Default)]
struct HostViewInner {
    /// Resident VMs the host lists (`SandboxBackend::list`). A paused,
    /// mid-capture VM is STILL resident here — which is exactly why
    /// teardown-reconcile can reach it (issue #570).
    live: BTreeSet<SandboxId>,
    /// The in-RAM local binding table (`session_for_sandbox`).
    bindings: BTreeMap<SandboxId, SessionId>,
    /// Sandboxes with a capture lock held (`capture_in_flight`) — the
    /// eviction-finalize window.
    capturing: BTreeSet<SandboxId>,
    /// Oracle memory: every local destroy reconcile performed.
    destroyed: Vec<SandboxId>,
    /// Oracle memory: every binding repair reconcile performed.
    binding_repairs: Vec<(SandboxId, SessionId)>,
}

impl HostView {
    fn live(&self) -> Vec<SandboxId> {
        self.inner.lock().live.iter().copied().collect()
    }
    fn binding(&self, id: SandboxId) -> Option<SessionId> {
        self.inner.lock().bindings.get(&id).copied()
    }
    fn capturing(&self, id: SandboxId) -> bool {
        self.inner.lock().capturing.contains(&id)
    }
    /// Oracle/test read: local destroys reconcile performed.
    pub fn destroyed(&self) -> Vec<SandboxId> {
        self.inner.lock().destroyed.clone()
    }
    /// Oracle/test read: binding repairs reconcile performed.
    pub fn binding_repairs(&self) -> Vec<(SandboxId, SessionId)> {
        self.inner.lock().binding_repairs.clone()
    }
    /// Drop the in-RAM local binding for a sandbox — models the post-roll
    /// survivor whose `PooledBackend` binding table died with the process
    /// (ADR 0090 / 731df805). The VM stays live; reconcile must REPAIR the
    /// binding from the coordinator, never reap.
    pub fn drop_binding(&self, id: SandboxId) {
        self.inner.lock().bindings.remove(&id);
    }
}

/// Terminal-destroy recorder for the real finalizer's `EvictionSandbox`.
#[derive(Default)]
struct CosimDestroyer {
    destroyed: Mutex<Vec<SandboxId>>,
}

#[async_trait]
impl EvictionSandbox for CosimDestroyer {
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.destroyed.lock().push(id);
        Ok(())
    }
}

/// One sandbox on the host.
struct CosimSandbox {
    session_id: Option<SessionId>,
    /// ADR 0035 amendment: the aux bundle generations this sandbox has
    /// attached (the create-time swap-to-current — the host's stamp at
    /// commit time). What the heartbeat's `sandbox_bundles` reports and
    /// what a capture's `aux_bundles` pins.
    attached_bundles: Vec<AuxBundleRef>,
    /// The private base manifest the sandbox was created on — the ref a
    /// post-roll rehydrate rebuilds the RAM backend from (rung 2: the survivor
    /// re-serve leg reconstructs `ChunkedDiskBackend` off the durable pointer).
    base_ref: ManifestRef,
    published_ref: Option<ManifestRef>,
    /// The live RAM backend — `None` once paused for a finalize, destroyed, or
    /// (rung 2) dropped by a host-agent roll until the rehydrate re-serves it.
    backend: Option<Arc<ChunkedDiskBackend>>,
    /// The work cursor (an events-index stand-in): advanced by each guest
    /// write, captured at eviction time. The boundary oracle keys on it.
    cursor: i64,
    /// The guest journal + vsock-generation model. It survives connection
    /// severance exactly like the guest filesystem survives a checkpoint.
    exec: CosimExecPlane,
}

/// Per-sandbox guest exec state. Every connection runs the real agentd
/// handler. Only the vsock transport is modeled.
///
/// Production snapshot capture gives the host no EOF. A gate holds each
/// severed pipe open to model that silence. Only [`engram_sandbox_firecracker::EpochSeveredStream`]
/// can convert the silence to EOF. A natural guest close still gives the
/// host a real EOF through the gate.
struct CosimExecPlane {
    journal: Arc<ExecJournal>,
    supervisor: Arc<HarnessSupervisor>,
    cacerts: Arc<CaCertInstaller>,
    epoch: tokio::sync::watch::Sender<u64>,
    connection_tasks: Vec<tokio::task::AbortHandle>,
    severances: u64,
    connections_opened: u64,
}

impl CosimExecPlane {
    fn new(root: std::path::PathBuf) -> Self {
        Self {
            journal: Arc::new(ExecJournal::new(root.join("journal"))),
            supervisor: HarnessSupervisor::new(),
            cacerts: Arc::new(CaCertInstaller::new(CaCertPaths {
                bundle: root.join("cacerts/ca-bundle"),
                extra_cert: root.join("cacerts/engram.crt"),
            })),
            epoch: tokio::sync::watch::channel(0).0,
            connection_tasks: Vec::new(),
            severances: 0,
            connections_opened: 0,
        }
    }

    async fn authorize_start(&self, cmd: &ExecRequest) -> Result<(), SandboxError> {
        let exec_id = cmd.exec_id.as_deref().ok_or_else(|| {
            SandboxError::InvalidSpec(
                "cosim durable exec requires the coordinator-assigned exec_id".into(),
            )
        })?;
        let authorization = self
            .journal
            .attach_or_start(exec_id, &cmd.command)
            .await
            .map_err(SandboxError::Io)?;
        let entry = match authorization {
            AttachOrStart::Start(entry) | AttachOrStart::DegradedStart { entry, .. } => entry,
            AttachOrStart::Attach(_) => return Ok(()),
            AttachOrStart::Missing => {
                return Err(SandboxError::InvalidSpec(
                    "fresh cosim exec unexpectedly returned a missing journal".into(),
                ));
            }
        };

        // `serve_durable_start` execs `current_exe __exec-wrapper`, which is
        // the Rust test harness here rather than agentd. Pre-authorizing the
        // journal and running the real wrapper in-process makes the handler's
        // first request take its real Attach/live-tail path. Re-attachments
        // then use the exact same path against the same journal directory.
        let command = cmd.command.clone();
        let timeout = cmd.timeout;
        tokio::spawn(async move {
            if let Err(error) =
                engram_agentd::exec_journal::run_wrapper(entry, command, timeout).await
            {
                tracing::warn!(%error, "cosim durable exec wrapper failed");
            }
        });
        Ok(())
    }

    fn connect(&mut self) -> BoxExecIo {
        let (driver_end, mut gate_a) = tokio::io::duplex(64 * 1024);
        let (mut gate_b, guest_end) = tokio::io::duplex(64 * 1024);
        let guest_task = tokio::spawn(serve_connection_with_journal(
            guest_end,
            None,
            self.supervisor.clone(),
            self.cacerts.clone(),
            self.journal.clone(),
        ));
        self.connection_tasks.push(guest_task.abort_handle());
        // Dropping the join handle detaches the real handler. Drop keeps its
        // abort handle for cleanup.
        drop(guest_task);

        let mut rx = self.epoch.subscribe();
        let born = *rx.borrow();
        let gate_task = tokio::spawn(async move {
            let severed = tokio::select! {
                _ = tokio::io::copy_bidirectional(&mut gate_a, &mut gate_b) => false,
                changed = rx.wait_for(|epoch| *epoch > born) => {
                    // Drop the borrowed watch value before this task parks.
                    drop(changed);
                    true
                }
            };
            if severed {
                // A checkpoint severed this connection. Firecracker sends
                // no FIN or RST to the host. Hold both pipe ends open so
                // the driver stays silent. Only EpochSeveredStream can
                // convert this silence to EOF.
                std::future::pending::<()>().await;
            }
        });
        self.connection_tasks.push(gate_task.abort_handle());
        // A severed gate can stay parked. Drop keeps its abort handle.
        drop(gate_task);

        self.connections_opened += 1;
        Box::new(engram_sandbox_firecracker::EpochSeveredStream::new(
            driver_end,
            self.epoch.subscribe(),
        ))
    }

    fn sever(&mut self) {
        self.epoch.send_modify(|epoch| *epoch += 1);
        self.severances += 1;
    }

    fn counters(&self) -> (u64, u64) {
        (self.severances, self.connections_opened)
    }
}

impl Drop for CosimExecPlane {
    fn drop(&mut self) {
        for task in self.connection_tasks.drain(..) {
            task.abort();
        }
    }
}

/// The outcome of a [`CosimHost::finalize_tick`] the bridge maps to a
/// coordinator snapshot-row write (mirroring prod's heartbeat reconcile).
#[derive(Clone, Debug)]
pub enum FinalizeTickOutcome {
    /// No finalize in flight for this sandbox (already completed, or
    /// cancelled by a teardown-reconcile reap — the #570 loss).
    Idle,
    /// Still uploading; re-drive next tick.
    Retrying,
    /// The finalize was quarantined (redrive budget exhausted) — no
    /// durable snapshot lands.
    Quarantined { snapshot_id: SnapshotId },
    /// The finalize completed: a durable snapshot at `cursor` exists. The
    /// bridge records the recoverable snapshot row.
    Completed {
        snapshot_id: SnapshotId,
        session_id: SessionId,
        cursor: i64,
        /// Whether the finalize published a disk manifest (a poisoned,
        /// manifestless completion is the pre-#743 shape; always `true`
        /// here).
        disk_manifest: bool,
        /// ADR 0035 amendment: the generations this capture pinned (and the
        /// finalize published) — recorded onto the snapshot row so the pin
        /// set's snapshot leg holds after the sandbox is destroyed.
        aux_bundles: Vec<AuxBundleRef>,
    },
}

/// One co-simulated host-agent process.
pub struct CosimHost {
    pub host_id: HostId,
    pub clock: Arc<SimClock>,
    pub entropy: Arc<SimEntropy>,
    pub fs: SimFs,
    pub store: Arc<ChunkStore>,
    fs_seam: Arc<dyn HostFs>,
    pub view: Arc<HostView>,
    sandboxes: BTreeMap<SandboxId, CosimSandbox>,
    /// The `PooledBackend::pending_finalizes` analog — RAM, drives the
    /// capture-lock idempotency + the real finalizer's map.
    pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
    in_flight: BTreeMap<SandboxId, EvictionFinalizeRecord>,
    /// The cursor a given in-flight finalize will publish at.
    finalize_cursor: BTreeMap<SnapshotId, i64>,
    destroyer: Arc<CosimDestroyer>,
    next_tag: u64,
    /// Rung 2 (#784): the REAL NBD slot/generation/park/un-pause device-plane
    /// model, shared with `engram-dst-host` via
    /// [`engram_dst_host::device_plane`], keyed by the coordinator-minted
    /// [`SandboxId`]. Drives the roll → register-rehydrate → stale-sweep →
    /// un-pause family co-simulated at the boundary.
    pub device: DevicePlane,
    /// Oracle memory (survives): sandboxes the stale-sweep PARKed because a
    /// live guest still held the device (the `sweep-blocked-live-holder` set,
    /// #806) — never severed, must be re-served. Under Wave 7b this is the
    /// dead-owner live-holder set the barrier classified `ReconnectMe` OR
    /// `QuarantinedUnknown` (both left RECONNECTABLE).
    sweep_parked_live: BTreeSet<SandboxId>,
    /// Wave 7b (ADR 0098 §Phase 3, #784 layers 2–3): oracle memory — sandboxes
    /// the classification barrier put in
    /// [`SlotClass::QuarantinedUnknown`](engram_host_core::SlotClass::QuarantinedUnknown):
    /// a kernel-connected device with a live holder that NO tracked record
    /// accounted for (#769 gap A). CLASSIFIED + alertable, never skipped.
    quarantined_unknown: BTreeSet<SandboxId>,
    /// ADR 0035 amendment: the SHARED blob bucket (one `MemBlobStorage`
    /// backing the coordinator's blob tier, this host's `ChunkStore`, AND
    /// the bundle store) — the pre-amendment cosim gave coord and host two
    /// disjoint buckets, which made "host publishes → coord GC deletes →
    /// host needs it" structurally unrepresentable.
    blob: Arc<dyn engram_core::traits::BlobStorage>,
    /// The bake stamp: the current bundle generation set (`current.json`'s
    /// cosim analog). One `dyn_0` slot is enough — the incident class is
    /// about rotation, not slot count.
    stamp: Vec<AuxBundleRef>,
    /// ADR 0035 amendment D1 state: has the current stamp generation been
    /// published to blob storage? `false` models the startup-publish task
    /// not yet having succeeded (the heartbeat retries it, mirroring the
    /// 60 s retry timer).
    stamp_published: bool,
    /// Deterministic generation counter for stamp contents.
    bundle_gen: u64,
}

impl CosimHost {
    /// Build a host over a shared clock/entropy (the ONE clock the whole
    /// co-sim shares), the SHARED blob bucket (ADR 0035 amendment: the
    /// coordinator's blob tier and this host's chunk/bundle stores are one
    /// bucket, exactly like prod's single GCS bucket), and its own per-run
    /// [`SimFs`].
    pub fn new(
        host_id: HostId,
        clock: Arc<SimClock>,
        entropy: Arc<SimEntropy>,
        blob: Arc<dyn engram_core::traits::BlobStorage>,
    ) -> Self {
        let fs = SimFs::new().expect("sim tempdir");
        // The blob tier is the deterministic in-memory `MemBlobStorage` (ADR
        // 0098 determinism-audit item 7 / the #799 fix): no `tokio::fs`
        // blocking-pool I/O to auto-advance the paused clock, and sorted
        // `list_prefix` for free. The `ChunkedDiskBackend`'s own `ChunkCache`
        // still uses the `SimFs` tempdir (inherent to reusing the REAL
        // backend), but its I/O never feeds a decision — see the swarm's
        // I/O-audit verdict.
        let store = Arc::new(ChunkStore::new(blob.clone()));
        Self {
            host_id,
            clock,
            entropy,
            fs,
            store,
            fs_seam: Arc::new(TokioFs),
            view: Arc::new(HostView::default()),
            sandboxes: BTreeMap::new(),
            pending_finalizes: Arc::new(DashMap::new()),
            in_flight: BTreeMap::new(),
            finalize_cursor: BTreeMap::new(),
            destroyer: Arc::new(CosimDestroyer::default()),
            next_tag: 0,
            device: DevicePlane::new(DEVICE_CAPACITY),
            sweep_parked_live: BTreeSet::new(),
            quarantined_unknown: BTreeSet::new(),
            blob,
            stamp: Vec::new(),
            stamp_published: false,
            bundle_gen: 0,
        }
    }

    // ─────────── ADR 0035 amendment: the bundle lifecycle ───────────
    //
    // The REAL `BundleStore` (publish / materialize / sweep_unpinned) over
    // the SHARED blob bucket + this host's staged-bundle dir. The stamp is
    // the `current.json` analog; rotation models a node-assets roll staging
    // a new bake. The 2026-08-10 incident's whole mechanism — attach the
    // current generation at create, rotate the stamp, sweep the only copy,
    // fail the capture's publish — is expressible through these ops.

    /// The staged-bundle dir (what the real `EvictionFinalizer` gets as
    /// `bundle_dir`).
    pub fn bundle_dir(&self) -> std::path::PathBuf {
        self.fs.root().join("bundles")
    }

    fn bundle_store(&self) -> engram_host_agent::bundles::BundleStore {
        engram_host_agent::bundles::BundleStore::new(
            self.blob.clone(),
            self.bundle_dir(),
            BUNDLE_EXT,
        )
    }

    /// Stage a NEW stamp generation (the node-assets init on a bake roll):
    /// write deterministic content-addressed bytes into the staged dir and
    /// point the stamp's `dyn_0` slot at it. Does NOT publish — that is
    /// [`startup_publish`](Self::startup_publish) (D1), kept separate so the
    /// pre-D1 world (staged-but-never-durable) stays replayable.
    pub async fn stage_new_stamp_generation(&mut self) {
        use sha2::Digest as _;
        self.bundle_gen += 1;
        let body = synth_chunk(0xB00D_0000 + self.bundle_gen);
        let sha = hex::encode(sha2::Sha256::digest(&body));
        let dir = self.bundle_dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("bundle dir mkdir");
        tokio::fs::write(dir.join(format!("{sha}.{BUNDLE_EXT}")), &body)
            .await
            .expect("stage stamp generation");
        self.stamp = vec![AuxBundleRef {
            drive_id: "dyn_0".into(),
            sha256: sha,
        }];
        self.stamp_published = false;
    }

    /// ADR 0035 amendment D1: publish the stamp's generations to blob
    /// storage (the startup-publish task's one pass — the REAL idempotent
    /// HEAD-first `BundleStore::publish`).
    pub async fn startup_publish(&mut self) -> Result<(), String> {
        self.bundle_store()
            .publish(&self.stamp)
            .await
            .map_err(|e| format!("startup publish: {e}"))?;
        self.stamp_published = true;
        Ok(())
    }

    /// One heartbeat-ack's worth of the REAL bundle supervisor semantics:
    /// materialize pinned generations this host is missing, then sweep
    /// staged generations that are neither pinned nor in the stamp. The
    /// sweep runs even when materialize fails, exactly like the supervisor
    /// loop.
    pub async fn bundle_sweep(&mut self, live: &[AuxBundleRef]) -> Result<(), String> {
        let store = self.bundle_store();
        let materialize = store.materialize_if_missing(live).await;
        store.sweep_unpinned(live, &self.stamp).await;
        materialize.map_err(|e| format!("bundle materialize: {e}"))
    }

    /// The stamp (the heartbeat's `current_bundles`).
    pub fn current_bundles(&self) -> Vec<AuxBundleRef> {
        self.stamp.clone()
    }

    /// Has the current stamp generation reached blob storage (D1 state)?
    pub fn stamp_published(&self) -> bool {
        self.stamp_published
    }

    /// The heartbeat's `sandbox_bundles`: every sandbox this host still
    /// holds (paused/capturing included — the real `aux_bundles_all` reports
    /// every backend-map entry) with its attached generations.
    pub fn sandbox_bundles(&self) -> Vec<SandboxAuxBundles> {
        self.sandboxes
            .iter()
            .filter(|(_, s)| !s.attached_bundles.is_empty())
            .map(|(id, s)| SandboxAuxBundles {
                sandbox_id: *id,
                bundles: s.attached_bundles.clone(),
            })
            .collect()
    }

    /// Oracle read: every held sandbox's attachments together with whether
    /// each generation is still staged locally (checked synchronously under
    /// the lock; blob reachability is the caller's async half).
    pub fn attachment_staging(&self) -> Vec<(SandboxId, AuxBundleRef, bool)> {
        let store = self.bundle_store();
        self.sandboxes
            .iter()
            .flat_map(|(id, s)| {
                s.attached_bundles.iter().map(|r| {
                    let staged = store.staged_path(r).exists();
                    (*id, r.clone(), staged)
                })
            })
            .collect()
    }

    fn next_tag(&mut self) -> u64 {
        self.next_tag += 1;
        self.next_tag
    }

    /// A clone handle to the reconcile-visible state (for building a
    /// [`CosimReconcileBackend`]).
    pub fn view(&self) -> Arc<HostView> {
        self.view.clone()
    }

    /// The terminal-destroy log (VMs the finalize + reconcile tore down).
    pub fn terminal_destroys(&self) -> Vec<SandboxId> {
        self.destroyer.destroyed.lock().clone()
    }

    // ── Unlocked-build accessors (ADR 0098 R-CoSim concurrency rule) ──
    //
    // The coordinator's boot pipeline runs `restore_base_for_session` inside
    // a `tokio::join!` (overlapped with the env/egress leg) AND wraps the
    // whole CreateBoot verb in a `tokio::time::timeout`. Holding the shared
    // `tokio::sync::Mutex<CosimHost>` guard ACROSS the async base-manifest
    // build under that machinery livelocks the fair mutex (a queued waiter +
    // the held guard never converge). So the bridge builds the backend with
    // cloned handles WITHOUT the guard held, then takes the lock only for the
    // synchronous insert. These accessors + [`commit_created_sandbox`] are
    // that split.

    pub fn store_handle(&self) -> Arc<ChunkStore> {
        self.store.clone()
    }
    pub fn entropy_handle(&self) -> Arc<SimEntropy> {
        self.entropy.clone()
    }
    pub fn cache_root(&self) -> std::path::PathBuf {
        self.fs.cache_dir().to_path_buf()
    }
    /// The current-generation NBD slot pool handle (cloned) — the bridge claims
    /// a new sandbox's device on THIS pool OUTSIDE the host lock, then commits
    /// the slot under a brief lock (the same concurrency split the backend
    /// build uses; see the module-level note).
    pub fn device_pool(&self) -> Arc<NbdSlotAllocator> {
        self.device.nbd_pool.clone()
    }
    /// A free `/dev/nbdN` path for a new sandbox (see
    /// [`DevicePlane::next_free_device`]).
    pub fn next_free_device(&self) -> std::path::PathBuf {
        self.device
            .next_free_device()
            .expect("cosim device universe not exhausted (DEVICE_CAPACITY)")
    }

    /// Insert a pre-built sandbox (the sync half of the boot restore leg),
    /// together with its pre-claimed NBD device slot.
    pub fn commit_created_sandbox(
        &mut self,
        id: SandboxId,
        base_ref: ManifestRef,
        backend: Arc<ChunkedDiskBackend>,
        device: std::path::PathBuf,
        lease: NbdSlot,
    ) {
        let exec = CosimExecPlane::new(self.fs.root().join("exec").join(id.to_string()));
        self.sandboxes.insert(
            id,
            CosimSandbox {
                session_id: None,
                // ADR 0035 amendment: the create-time swap-to-current — the
                // sandbox attaches whatever generation the stamp holds NOW
                // (`fc.swap_aux_bundles`' cosim analog). A later stamp
                // rotation must not strand these refs.
                attached_bundles: self.stamp.clone(),
                base_ref,
                published_ref: None,
                backend: Some(backend),
                cursor: 0,
                exec,
            },
        );
        self.device.insert_served_slot(id, device, lease);
        self.view.inner.lock().live.insert(id);
    }

    /// Bind a sandbox to a session (host-local binding table half of the
    /// coordinator's `bind_session`).
    pub fn bind(&mut self, session_id: SessionId, sandbox_id: SandboxId) {
        if let Some(s) = self.sandboxes.get_mut(&sandbox_id) {
            s.session_id = Some(session_id);
        }
        self.view
            .inner
            .lock()
            .bindings
            .insert(sandbox_id, session_id);
    }

    /// Drop the local binding for a session (host-local half of
    /// `unbind_session`). NB: the D5 idle-eviction fast path does NOT call
    /// this — it clears only the coordinator's PG `sandbox_id`, leaving the
    /// host's local binding intact (which is why reconcile hits the *Bound*
    /// arm, not Unbound, in the #570 race).
    pub fn unbind_session(&mut self, session_id: SessionId) {
        let mut view = self.view.inner.lock();
        view.bindings.retain(|_, s| *s != session_id);
        drop(view);
        for s in self.sandboxes.values_mut() {
            if s.session_id == Some(session_id) {
                s.session_id = None;
            }
        }
    }

    /// Coordinator-driven destroy of a sandbox (a `HostClient::destroy`).
    pub fn destroy(&mut self, sandbox_id: SandboxId) {
        self.remove_sandbox(sandbox_id);
    }

    /// Is a sandbox present (for probes)?
    pub fn contains(&self, sandbox_id: SandboxId) -> bool {
        self.sandboxes.contains_key(&sandbox_id)
    }

    /// The live sandbox ids (a `HostClient::list`).
    pub fn list(&self) -> Vec<SandboxId> {
        self.view.live()
    }

    /// The sandbox's current work cursor (for the periodic-checkpoint +
    /// oracle bridging).
    pub fn cursor(&self, sandbox_id: SandboxId) -> Option<i64> {
        self.sandboxes.get(&sandbox_id).map(|s| s.cursor)
    }

    /// Authorize at most one wrapper spawn and open the first real agentd
    /// connection.
    pub async fn start_exec_transport(
        &mut self,
        sandbox_id: SandboxId,
        cmd: &ExecRequest,
    ) -> Result<BoxExecIo, SandboxError> {
        let sandbox = self
            .sandboxes
            .get_mut(&sandbox_id)
            .ok_or(SandboxError::NotFound)?;
        sandbox.exec.authorize_start(cmd).await?;
        Ok(sandbox.exec.connect())
    }

    /// Open a fresh real agentd handler on the sandbox's surviving journal.
    pub fn redial_exec_transport(
        &mut self,
        sandbox_id: SandboxId,
    ) -> Result<BoxExecIo, SandboxError> {
        self.sandboxes
            .get_mut(&sandbox_id)
            .map(|sandbox| sandbox.exec.connect())
            .ok_or(SandboxError::NotFound)
    }

    /// Model snapshot `TRANSPORT_RESET`. The gate keeps the host side silent.
    /// The epoch wrapper converts that silence to EOF.
    pub fn sever_exec_transports(&mut self, sandbox_id: SandboxId) {
        if let Some(sandbox) = self.sandboxes.get_mut(&sandbox_id) {
            sandbox.exec.sever();
        }
    }

    /// Oracle read for directed exec scenarios:
    /// `(checkpoint severances, real agentd connections opened)`.
    pub fn exec_transport_counters(&self, sandbox_id: SandboxId) -> Option<(u64, u64)> {
        self.sandboxes
            .get(&sandbox_id)
            .map(|sandbox| sandbox.exec.counters())
    }

    /// Sandboxes with an eviction finalize in flight (drive `finalize_tick`
    /// on these).
    pub fn pending_finalize_sandboxes(&self) -> Vec<SandboxId> {
        self.in_flight.keys().copied().collect()
    }

    /// Is a capture in flight for this sandbox (the capture lock)?
    pub fn capture_in_flight(&self, sandbox_id: SandboxId) -> bool {
        self.pending_finalizes.contains_key(&sandbox_id)
    }

    // ─────────── Rung 2: the NBD device-plane family (#784) ───────────
    //
    // All decisions run over the REAL host-core verdicts (`sweep_verdict` incl.
    // the #806 holder table, `resume_data_plane_served`,
    // `is_local_survivor_candidate`) and the REAL `NbdSlotAllocator`, via the
    // shared `engram_dst_host::device_plane::DevicePlane`. Only the world
    // bookkeeping (generation / served_by / parked / guest liveness) is sim.

    /// A host-agent process ROLL: the successor comes up as a fresh generation.
    /// The RAM disk backends die (rebuilt by the rehydrate); the FC VMs stay
    /// resident survivors (`view.live` unchanged); the device plane rolls
    /// (`served_by` clears, `kernel_owner` persists at the now-dead gen). A
    /// sandbox mid-capture already has `backend == None` (paused for finalize),
    /// so this leaves it untouched.
    pub fn roll(&mut self) {
        self.device.roll();
        for s in self.sandboxes.values_mut() {
            s.backend = None; // RAM died with the process
        }
    }

    /// The register-time rehydrate sequence (ADR 0098 P7 / #739 / #784):
    /// coord-list pass → local ChainHeadRecord pass → stale-binding sweep. The
    /// `rehydrate_list` is the REAL coordinator listing
    /// (`register_rehydrate_list_core`), passed in by the harness; the local
    /// pass + sweep run over the pure predicates. `local_pass_enabled=false`
    /// replays the pre-#739 ungated variant (proving the un-pause gate is the
    /// last line).
    pub async fn register_rehydrate(
        &mut self,
        rehydrate_list: &[SandboxId],
        local_pass_enabled: bool,
    ) -> Result<(), String> {
        // 1. Coord-list pass: re-serve every LISTED resident survivor. The coord
        //    list is authoritative for what it CONTAINS (each listed survivor
        //    carries the coordinator's disk-manifest ref), so it is NOT gated on
        //    the local record — a gap-A survivor is one the list OMITS (e.g. a
        //    HostLost row is not reserves-host-memory), which the REAL
        //    `register_rehydrate_list_core` already excludes. Only the #739 local
        //    pass below reads the (loseable) ChainHeadRecord.
        for &id in rehydrate_list {
            if self.device.is_resident_survivor(id) {
                self.serve_and_rebuild(id).await?;
            }
        }
        // 2. Local ChainHeadRecord pass (#739): re-serve any live survivor the
        //    coord list missed (live ∧ unserved ∧ session-bound) — but only when
        //    a record actually exists to read.
        if local_pass_enabled {
            let ids: Vec<SandboxId> = self.device.slots.keys().copied().collect();
            for id in ids {
                if !self.device.record_present(id) {
                    continue;
                }
                let has_session = self.sandboxes.get(&id).and_then(|s| s.session_id).is_some();
                if self.device.is_local_survivor_candidate(id, has_session) {
                    self.serve_and_rebuild(id).await?;
                }
            }
        }
        // 3. The Wave 7b classification barrier: reconcile the kernel inventory
        //    against the records, reap ONLY TerminalSafeToReap. A dead-owner
        //    device a live guest still holds is ReconnectMe (record) or
        //    QuarantinedUnknown (no record) — NEITHER reaped (never severed).
        self.barrier_sweep();
        Ok(())
    }

    /// Wave 7b (#784 layers 2–3): classify the kernel-derived inventory against
    /// the records, then reap ONLY the `TerminalSafeToReap` subset via the REAL
    /// [`DevicePlane::reap_terminal`] (which accepts only a
    /// [`ReapList`](engram_host_core::ReapList) — the ordering contract). Records
    /// the parked-live (ReconnectMe ∪ QuarantinedUnknown) + the quarantined
    /// (record-invisible) sets for the oracles.
    fn barrier_sweep(&mut self) {
        let classification = self.device.classify_startup();
        self.sweep_parked_live
            .extend(classification.reconnect.iter().copied());
        self.sweep_parked_live
            .extend(classification.quarantined.iter().copied());
        self.quarantined_unknown
            .extend(classification.quarantined.iter().copied());
        self.device.reap_terminal(classification.reap);
    }

    /// Serve `id`'s device on the current generation (RECONFIGURE) and rebuild
    /// its RAM backend if the roll dropped it. A capture-in-flight sandbox is
    /// skipped (its VM is paused for the finalize).
    async fn serve_and_rebuild(&mut self, id: SandboxId) -> Result<(), String> {
        if self.capture_in_flight(id) {
            return Ok(());
        }
        match self.device.serve(id).await? {
            ServeOutcome::NewlyServed => self.ensure_backend(id).await,
            ServeOutcome::AlreadyServed => Ok(()),
        }
    }

    /// Rebuild `id`'s RAM backend from its durable pointer (last publish, else
    /// base) if it is currently `None` — the post-roll survivor re-serve leg.
    async fn ensure_backend(&mut self, id: SandboxId) -> Result<(), String> {
        let needs = self.sandboxes.get(&id).is_some_and(|s| s.backend.is_none())
            && !self.capture_in_flight(id);
        if !needs {
            return Ok(());
        }
        let manifest_ref = {
            let s = self.sandboxes.get(&id).expect("sandbox present");
            s.published_ref.unwrap_or(s.base_ref)
        };
        let backend = build_backend(&self.store, self.fs.cache_dir(), &id, manifest_ref).await;
        if let Some(s) = self.sandboxes.get_mut(&id) {
            s.backend = Some(Arc::new(backend));
        }
        Ok(())
    }

    /// Rung-2 PARK sandbox `id` (FC paused, VM resident, device still served) —
    /// the 731df805 pre-condition.
    pub fn park(&mut self, id: SandboxId) {
        self.device.park(id);
    }

    /// Un-pause sandbox `id` over the REAL un-pause data-plane gate. Returns
    /// `true` if un-paused, `false` if the gate fired (unserved plane).
    pub fn unpause(&mut self, id: SandboxId) -> bool {
        self.device.unpause(id)
    }

    /// Model the FC guest genuinely dying (#806): it no longer holds its device
    /// node open, so a dead-owner sweep may legally DISCONNECT.
    pub fn kill_guest(&mut self, id: SandboxId) {
        self.device.kill_guest(id);
    }

    /// Wave 7b (#784 layer 2): lose `id`'s tracked records (#769 gap A) — a
    /// resident guest holds a device no record accounts for.
    pub fn lose_record(&mut self, id: SandboxId) {
        self.device.lose_record(id);
    }

    /// The operator/runbook reconcile: restore `id`'s record so the next register
    /// re-serves the reconnectable device with zero loss.
    pub fn regain_record(&mut self, id: SandboxId) {
        self.device.regain_record(id);
    }

    /// Quiescence heal (Wave 7b): restore EVERY device's record — the record-loss
    /// fault is healed globally at quiescence exactly like every other fault
    /// (re-served survivors, drained finalizes), so the drain's rehydrate can
    /// re-serve every survivor and convergence is required against a healed world.
    pub fn regain_all_records(&mut self) {
        let ids: Vec<SandboxId> = self.device.slots.keys().copied().collect();
        for id in ids {
            self.device.regain_record(id);
        }
    }

    /// Run the stale-binding sweep independently, GATED by the Wave 7b
    /// classification barrier (classify → reap only TerminalSafeToReap).
    pub fn stale_sweep_tick(&mut self) {
        self.barrier_sweep();
    }

    /// `try_claim` a spare device on the real allocator (slot-accounting).
    pub async fn slot_claim(&mut self) {
        // Spare device: the first free path ABOVE the sandbox devices.
        if let Some(device) = self.device.next_free_device() {
            self.device.slot_claim(device).await;
        }
    }

    /// Release the oldest held spare lease (slot-accounting).
    pub fn slot_populate_tick(&mut self) {
        self.device.slot_populate_tick();
    }

    // ── Device-plane oracle/read accessors ──
    pub fn is_parked(&self, id: SandboxId) -> bool {
        self.device.is_parked(id)
    }
    pub fn served_by_current(&self, id: SandboxId) -> bool {
        self.device.served_by_current(id)
    }
    /// Does the host hold a LIVE RAM backend for this sandbox — i.e. is it
    /// ACTIVELY serving the guest (not paused for a capture, not post-roll
    /// pre-rehydrate)? The split-brain oracle keys on this: a paused/capturing
    /// device is mid-lifecycle-transition, not actively double-serving.
    pub fn has_live_backend(&self, id: SandboxId) -> bool {
        self.sandboxes.get(&id).is_some_and(|s| s.backend.is_some())
    }
    pub fn guest_holds_device(&self, id: SandboxId) -> bool {
        self.device.guest_holds_device(id)
    }
    pub fn kernel_owner(&self, id: SandboxId) -> Option<u32> {
        self.device.kernel_owner(id)
    }
    pub fn device_slot_ids(&self) -> Vec<SandboxId> {
        self.device.slots.keys().copied().collect()
    }
    /// The slot-accounting identity terms: `(free, warm, held, capacity)`.
    pub async fn slot_accounting(&self) -> (usize, usize, usize, usize) {
        let free = self.device.nbd_pool.free_count().await;
        let warm = self.device.nbd_pool.warm_count().await;
        let held = self.device.leases_held();
        (free, warm, held, self.device.nbd_capacity as usize)
    }
    /// Oracle read: sandboxes the sweep PARKed because a live guest held the
    /// device (#806) — never severed.
    pub fn sweep_parked_live(&self) -> Vec<SandboxId> {
        self.sweep_parked_live.iter().copied().collect()
    }
    /// Oracle read (Wave 7b): sandboxes the classification barrier put in
    /// `QuarantinedUnknown` — a record-invisible live survivor (#769 gap A).
    pub fn quarantined_unknown(&self) -> Vec<SandboxId> {
        self.quarantined_unknown.iter().copied().collect()
    }
    /// The session a sandbox is bound to, as the HOST knows it — what the
    /// real host-agent stamps into a `QuarantinedSurvivor` advertise.
    pub fn session_of(&self, id: SandboxId) -> Option<SessionId> {
        self.sandboxes.get(&id).and_then(|s| s.session_id)
    }

    /// A unit of guest work: write one real content-tagged chunk (advancing
    /// the backend's dirty tier) and bump the work cursor.
    pub async fn guest_write(&mut self, sandbox_id: SandboxId) -> Result<(), String> {
        let tag = self.next_tag();
        let Some(s) = self.sandboxes.get_mut(&sandbox_id) else {
            return Ok(());
        };
        let Some(backend) = s.backend.clone() else {
            return Ok(()); // paused/destroyed
        };
        let chunk_idx = (tag % NUM_CHUNKS) * CHUNK_SIZE;
        backend
            .write(chunk_idx, &synth_chunk(tag))
            .await
            .map_err(|e| format!("guest_write {sandbox_id}: {e}"))?;
        s.cursor += 1;
        Ok(())
    }

    /// A flush: upload the dirty tier + advance the durable pointer (the
    /// periodic-checkpoint disk leg). Real `ChunkedDiskBackend::flush`.
    pub async fn flush(&mut self, sandbox_id: SandboxId) -> Result<(), String> {
        let Some(s) = self.sandboxes.get_mut(&sandbox_id) else {
            return Ok(());
        };
        let Some(backend) = s.backend.clone() else {
            return Ok(());
        };
        backend
            .flush()
            .await
            .map_err(|e| format!("flush {sandbox_id}: {e}"))?;
        s.published_ref = Some(backend.manifest_ref().await);
        Ok(())
    }

    /// The eviction capture leg (`PooledBackend::snapshot_begin`): drain the
    /// dirty tier through the REAL `persist_disk_pending_chunks`, persist a
    /// REAL `EvictionFinalizeRecord`, take the capture lock, pause the VM.
    /// Idempotent: a pending finalize re-observes the same snapshot id.
    pub async fn snapshot_begin(&mut self, sandbox_id: SandboxId) -> Result<SnapshotId, String> {
        if let Some(existing) = self.pending_finalizes.get(&sandbox_id) {
            return Ok(*existing);
        }
        let Some(s) = self.sandboxes.get(&sandbox_id) else {
            return Err(format!("snapshot_begin: unknown sandbox {sandbox_id}"));
        };
        let session_id = s
            .session_id
            .ok_or_else(|| format!("snapshot_begin: sandbox {sandbox_id} not bound"))?;
        let cursor = s.cursor;
        let aux_bundles = s.attached_bundles.clone();
        let Some(backend) = s.backend.clone() else {
            return Err(format!(
                "snapshot_begin: sandbox {sandbox_id} has no backend"
            ));
        };

        // The eviction/manual capture path reaches the same Firecracker
        // `prepare_save` hook as a periodic checkpoint. Every live guest
        // connection ends, but the journal survives on the guest disk.
        self.sever_exec_transports(sandbox_id);

        let snapshot_id = SnapshotId::from(self.entropy.uuid());
        let dest = self.fs.root().join("staging").join(snapshot_id.to_string());
        tokio::fs::create_dir_all(&dest)
            .await
            .map_err(|e| format!("snapshot_begin staging dir: {e}"))?;
        tokio::fs::write(dest.join("state.bin"), b"cosim fc state")
            .await
            .map_err(|e| format!("snapshot_begin state.bin: {e}"))?;

        // The capture disk drain: the un-uploaded tier moves from RAM to the
        // node-durable staging files via the REAL writer.
        let (base_manifest_ref, chunks) = backend.export_unflushed().await;
        let disk_chunks: Vec<(usize, ChunkHash, Bytes)> = chunks
            .iter()
            .map(|(i, b)| (*i, ChunkHash::of(b), Bytes::from(b.clone())))
            .collect();
        persist_disk_pending_chunks(&dest, &disk_chunks)
            .await
            .map_err(|e| format!("persist_disk_pending_chunks: {e}"))?;

        let now = self.clock.now_utc();
        let record = EvictionFinalizeRecord {
            snapshot_id,
            session_id,
            sandbox_id,
            image_version: "cosim".to_string(),
            size_bytes: 0,
            paused_at: now,
            captured_at: now,
            dest,
            chain_prev_ref: None,
            disk_pending: Some(DiskPendingRecord {
                base_manifest: base_manifest_ref,
                chunk_size: CHUNK_SIZE,
                total_bytes: NUM_CHUNKS * CHUNK_SIZE,
                chunks: disk_chunks.iter().map(|(i, h, _)| (*i, *h)).collect(),
            }),
            // ADR 0035 amendment: the capture pins what the sandbox actually
            // has attached, so the REAL finalize's bundle-publish leg
            // (`run_upload_blobs` → `BundleStore::publish`) runs — the exact
            // leg the 2026-08-10 incident failed on.
            aux_bundles,
            stage: FinalizeStage::Captured,
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        };
        let finalizer = self.finalizer();
        record
            .persist(self.fs_seam.as_ref(), &finalizer.finalize_dir())
            .await
            .map_err(|e| format!("snapshot_begin record persist: {e}"))?;

        self.pending_finalizes.insert(sandbox_id, snapshot_id);
        self.in_flight.insert(sandbox_id, record);
        self.finalize_cursor.insert(snapshot_id, cursor);
        // Take the capture lock (view mirror) + pause the VM.
        self.view.inner.lock().capturing.insert(sandbox_id);
        if let Some(s) = self.sandboxes.get_mut(&sandbox_id) {
            s.backend = None;
        }
        Ok(snapshot_id)
    }

    /// One finalize redrive attempt — the REAL `run_eviction_finalize_attempt`
    /// production loop body. On completion the terminal destroys the VM and
    /// the durable disk manifest becomes the survivor pointer.
    pub async fn finalize_tick(&mut self, sandbox_id: SandboxId) -> FinalizeTickOutcome {
        let Some(mut record) = self.in_flight.remove(&sandbox_id) else {
            return FinalizeTickOutcome::Idle;
        };
        let snapshot_id = record.snapshot_id;
        let session_id = record.session_id;
        let cursor = self.finalize_cursor.get(&snapshot_id).copied().unwrap_or(0);
        let finalizer = self.finalizer();
        match run_eviction_finalize_attempt(&finalizer, &mut record).await {
            FinalizeAttempt::Completed => {
                let disk_manifest = record.disk_manifest.is_some();
                if let Some(published) = record.disk_manifest {
                    if let Some(s) = self.sandboxes.get_mut(&sandbox_id) {
                        s.published_ref = Some(published);
                    }
                }
                // Capture lock released; the terminal leg destroyed the VM.
                self.pending_finalizes.remove(&sandbox_id);
                self.finalize_cursor.remove(&snapshot_id);
                self.remove_sandbox(sandbox_id);
                FinalizeTickOutcome::Completed {
                    snapshot_id,
                    session_id,
                    cursor,
                    disk_manifest,
                    aux_bundles: record.aux_bundles.clone(),
                }
            }
            FinalizeAttempt::Quarantined => {
                self.pending_finalizes.remove(&sandbox_id);
                self.finalize_cursor.remove(&snapshot_id);
                self.view.inner.lock().capturing.remove(&sandbox_id);
                FinalizeTickOutcome::Quarantined { snapshot_id }
            }
            FinalizeAttempt::RetryAfter(_backoff) => {
                self.in_flight.insert(sandbox_id, record);
                FinalizeTickOutcome::Retrying
            }
        }
    }

    /// Test hook (#784 rung 2, the capture-lock release pin): delete the
    /// in-flight finalize's node-durable staging dir so every redrive attempt
    /// fails (the ENOENT class) and the REAL `run_eviction_finalize_attempt`
    /// QUARANTINES after the redrive budget — proving the quarantine arm
    /// releases the capture lock (`pending_finalizes` entry cleared), so
    /// #783's teardown-reconcile capture-in-flight exemption can never become a
    /// permanent reap-shield.
    pub async fn sabotage_finalize_staging(&self, sandbox_id: SandboxId) {
        if let Some(record) = self.in_flight.get(&sandbox_id) {
            let _ = tokio::fs::remove_dir_all(&record.dest).await;
        }
    }

    /// Reconcile's local reap: cancel any in-flight finalize (the #570
    /// loss), drop the VM, record the destroy in the oracle log.
    fn reconcile_destroy(&mut self, sandbox_id: SandboxId) {
        // Cancelling a mid-upload finalize is the corruption: its inputs
        // are dropped and no snapshot row will ever land.
        self.in_flight.remove(&sandbox_id);
        if let Some((_, snap)) = self.pending_finalizes.remove(&sandbox_id) {
            self.finalize_cursor.remove(&snap);
        }
        let mut view = self.view.inner.lock();
        view.destroyed.push(sandbox_id);
        drop(view);
        self.remove_sandbox(sandbox_id);
    }

    fn remove_sandbox(&mut self, sandbox_id: SandboxId) {
        self.sandboxes.remove(&sandbox_id);
        // Drop the device slot too (its lease Drop releases the `/dev/nbdN`
        // back to the pool). A terminal finalize / coordinator destroy tears
        // the whole plane entry down.
        self.device.remove_slot(sandbox_id);
        self.sweep_parked_live.remove(&sandbox_id);
        self.quarantined_unknown.remove(&sandbox_id);
        let mut view = self.view.inner.lock();
        view.live.remove(&sandbox_id);
        view.bindings.remove(&sandbox_id);
        view.capturing.remove(&sandbox_id);
    }

    fn finalizer(&self) -> EvictionFinalizer {
        EvictionFinalizer::new(
            Some((*self.store).clone()),
            None,
            self.bundle_dir(),
            BUNDLE_EXT,
            self.fs.root().to_path_buf(),
            self.pending_finalizes.clone(),
            self.destroyer.clone(),
            self.fs_seam.clone(),
            FINALIZE_MAX_ATTEMPTS,
        )
    }
}

/// A shared, lock-guarded [`CosimHost`] both bridge directions mutate.
pub type SharedHost = Arc<tokio::sync::Mutex<CosimHost>>;

/// The [`ReconcileBackend`] over the co-sim host. The SYNC trait methods
/// read the parking_lot-guarded [`HostView`]; the async `destroy` locks the
/// host to cancel the finalize + drop the VM.
///
/// `honor_capture_signal` is the adversarial knob (the `coord_includes_parked`
/// pattern): `true` reports the real capture lock (the #570 fix), `false`
/// suppresses it — behaviorally identical to the PRE-fix reconcile that never
/// consulted the signal, so the same real `reconcile_once` reproduces the bug.
pub struct CosimReconcileBackend {
    host: SharedHost,
    view: Arc<HostView>,
    honor_capture_signal: bool,
}

impl CosimReconcileBackend {
    pub fn new(host: SharedHost, view: Arc<HostView>, honor_capture_signal: bool) -> Self {
        Self {
            host,
            view,
            honor_capture_signal,
        }
    }
}

#[async_trait]
impl ReconcileBackend for CosimReconcileBackend {
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.view.live())
    }
    fn migration_role_present(&self, _id: SandboxId) -> bool {
        false
    }
    fn is_live_capture(&self, _id: SandboxId) -> bool {
        // A base-snapshot capture JOB (ADR 0084) — not the same as a D5
        // eviction finalize. None in the co-sim boundary scenarios.
        false
    }
    fn capture_in_flight(&self, id: SandboxId) -> bool {
        self.honor_capture_signal && self.view.capturing(id)
    }
    fn session_for_sandbox(&self, id: SandboxId) -> Option<SessionId> {
        self.view.binding(id)
    }
    fn record_session_binding(&self, id: SandboxId, session: SessionId) {
        let mut view = self.view.inner.lock();
        view.bindings.insert(id, session);
        view.binding_repairs.push((id, session));
    }
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.host.lock().await.reconcile_destroy(id);
        Ok(())
    }
}
