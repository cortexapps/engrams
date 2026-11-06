//! The co-simulated host world (ADR 0098 R-CoSim, rung 1).
//!
//! [`CosimHost`] models one host-agent process at the coordinator↔host
//! boundary. It is keyed by the **coordinator-minted** sandbox/session ids
//! (the coordinator owns the id-space at this boundary), and its job is to
//! run the REAL host-agent lifecycle flows the coordinator's verbs trigger:
//!
//! * **create / restore** → provision the sandbox's root volume through the
//!   real [`SessionVolumes`] (a [`FileDisk`] under the run's tempdir) from a
//!   fresh private base manifest in the shared [`ChunkStore`];
//! * **eviction capture** ([`CosimHost::snapshot_begin`]) → the REAL
//!   capture leg: take the root checkpoint under the pause, persist a real
//!   `EvictionFinalizeRecord`, take the capture lock;
//! * **finalize** ([`CosimHost::finalize_tick`]) → the REAL production loop
//!   body `run_eviction_finalize_attempt` (publish the pending checkpoint +
//!   terminal destroy);
//! * **restart** ([`CosimHost::startup_reconcile`]) → the REAL startup
//!   volume reconcile over the live set and the pending finalizes;
//! * **durable exec** → the REAL Firecracker host reader and REAL agentd
//!   journal handler over per-sandbox duplex connections; checkpoint steps
//!   quietly sever the modeled vsock transport;
//! * **teardown-reconcile** → the REAL
//!   [`reconcile_once`](engram_host_agent::teardown_reconcile::reconcile_once)
//!   over [`CosimReconcileBackend`].
//!
//! **Concurrency.** [`CosimHost`] lives behind an `Arc<tokio::sync::Mutex>`
//! (the coordinator's async `HostClient` verbs and the host's async steps
//! both mutate it). The teardown-reconcile trait has SYNC methods, which
//! cannot take an async lock — so the reconcile-visible state (live set,
//! local bindings, capture-in-flight flags) is mirrored into a
//! `parking_lot`-guarded [`HostView`] the sync methods read, kept in
//! lockstep by every mutating host op.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_agentd::exec_journal::{AttachOrStart, ExecJournal};
use engram_agentd::{
    serve_connection_with_journal, CaCertInstaller, CaCertPaths, HarnessSupervisor,
};
use engram_chunk_store::manifest::{
    ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
};
use engram_chunk_store::store::ChunkStore;
use engram_core::error::SandboxError;
use engram_core::traits::{Clock as _, Entropy as _};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{AuxBundleRef, ExecRequest, SandboxAuxBundles};
use engram_core::types::snapshot::DiskRole;
use engram_core::types::SnapshotId;
use engram_core::{HostId, SandboxId, SessionId};
use engram_host_agent::eviction_finalize::{
    reconcile_volumes, run_eviction_finalize_attempt, EvictionFinalizeRecord, EvictionFinalizer,
    EvictionSandbox, FinalizeAttempt, PendingDisk,
};
use engram_host_agent::teardown_reconcile::ReconcileBackend;
use engram_host_core::session_volumes::{Report, Source};
use engram_host_core::{ChunkIndex, FileDisk, FinalizeStage, HostFs, SessionVolumes, TokioFs};
use engram_sandbox_firecracker::BoxExecIo;
use engram_sim::{SimClock, SimEntropy};
use parking_lot::Mutex;
use tempfile::TempDir;

/// Disk geometry — small on purpose (the oracle asserts a boundary
/// property, not throughput).
pub const CHUNK_SIZE: u64 = 4096;
pub const NUM_CHUNKS: u64 = 8;
/// Small redrive cap so a quarantine arm stays cheap (prod is 10).
pub const FINALIZE_MAX_ATTEMPTS: u32 = 3;

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

/// Provision a fresh sandbox's root volume WITHOUT the host lock held (the
/// concurrency rule above): seed a private base manifest in the shared store
/// and fork the volume from it. The bridge calls this with cloned handles,
/// then commits the result under a brief synchronous lock.
pub(crate) async fn build_base_sandbox(
    store: ChunkStore,
    volumes: Arc<SessionVolumes>,
    entropy: Arc<SimEntropy>,
) -> SandboxId {
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
    volumes
        .provision(
            sandbox_id,
            Source::Fork {
                manifest: base_ref,
                base: None,
            },
            None,
        )
        .await
        .expect("provision the root volume");
    sandbox_id
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
    /// (ADR 0090). The VM stays live; reconcile must REPAIR the
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
                chromium_policy: root.join("cacerts/chromium-policy.json"),
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
    /// The run's tempdir: staged bundles, capture staging dirs, exec
    /// journals and the session volumes live under it. It survives a
    /// modeled process restart, exactly like the node's disk.
    root: TempDir,
    pub store: ChunkStore,
    /// The REAL session-volume owner over a [`FileDisk`] in the tempdir.
    pub volumes: Arc<SessionVolumes>,
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
    /// tempdir.
    pub fn new(
        host_id: HostId,
        clock: Arc<SimClock>,
        entropy: Arc<SimEntropy>,
        blob: Arc<dyn engram_core::traits::BlobStorage>,
    ) -> Self {
        let root = tempfile::tempdir().expect("sim tempdir");
        // The blob tier is the deterministic in-memory `MemBlobStorage` (ADR
        // 0098 determinism-audit item 7 / the #799 fix): no `tokio::fs`
        // blocking-pool I/O to auto-advance the paused clock, and sorted
        // `list_prefix` for free. The session volumes are files in the
        // tempdir (inherent to reusing the REAL volume owner), but their
        // I/O never feeds a decision — see the swarm's I/O-audit verdict.
        let store = ChunkStore::new(blob.clone());
        let disk = FileDisk::new(root.path().join("volumes"), CHUNK_SIZE).expect("file disk");
        let volumes = Arc::new(SessionVolumes::new(Arc::new(disk), store.clone()));
        Self {
            host_id,
            clock,
            entropy,
            root,
            store,
            volumes,
            fs_seam: Arc::new(TokioFs),
            view: Arc::new(HostView::default()),
            sandboxes: BTreeMap::new(),
            pending_finalizes: Arc::new(DashMap::new()),
            in_flight: BTreeMap::new(),
            finalize_cursor: BTreeMap::new(),
            destroyer: Arc::new(CosimDestroyer::default()),
            next_tag: 0,
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
    // a new bake. These ops can express the sweep-before-publish failure:
    // attach the current generation at create, rotate the stamp, sweep the
    // only copy, fail the capture's publish.

    /// The staged-bundle dir (what the real `EvictionFinalizer` gets as
    /// `bundle_dir`).
    pub fn bundle_dir(&self) -> std::path::PathBuf {
        self.root.path().join("bundles")
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

    pub fn store_handle(&self) -> ChunkStore {
        self.store.clone()
    }
    pub fn entropy_handle(&self) -> Arc<SimEntropy> {
        self.entropy.clone()
    }
    pub fn volumes_handle(&self) -> Arc<SessionVolumes> {
        self.volumes.clone()
    }

    /// Insert a provisioned sandbox (the sync half of the boot restore leg).
    pub fn commit_created_sandbox(&mut self, id: SandboxId) {
        let exec = CosimExecPlane::new(self.root.path().join("exec").join(id.to_string()));
        self.sandboxes.insert(
            id,
            CosimSandbox {
                session_id: None,
                // ADR 0035 amendment: the create-time swap-to-current — the
                // sandbox attaches whatever generation the stamp holds NOW
                // (`fc.swap_aux_bundles`' cosim analog). A later stamp
                // rotation must not strand these refs.
                attached_bundles: self.stamp.clone(),
                cursor: 0,
                exec,
            },
        );
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
    pub async fn destroy(&mut self, sandbox_id: SandboxId) {
        self.remove_sandbox(sandbox_id).await;
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

    // ─────────── Process restart ───────────

    /// The successor process's startup: the REAL volume reconcile over the
    /// resident sandboxes and the durable finalize records. RAM state the
    /// real host-agent reloads from node-durable records (bindings,
    /// finalize records) is modeled as surviving; the `DropHostBinding`
    /// fault models the binding loss separately.
    pub async fn startup_reconcile(&mut self) -> Result<Report, String> {
        let finalizer = self.finalizer();
        reconcile_volumes(&self.volumes, Some(&finalizer), self.view.live())
            .await
            .map_err(|e| format!("startup volume reconcile: {e}"))
    }

    /// The registration ack: the coordinator lists every sandbox it still
    /// binds on this host, and the host records each binding (the REAL
    /// `record_session_binding` the register response drives).
    pub fn register_rehydrate(&mut self, listed: &[(SandboxId, SessionId)]) {
        for &(sandbox_id, session_id) in listed {
            if self.sandboxes.contains_key(&sandbox_id) {
                self.bind(session_id, sandbox_id);
            }
        }
    }

    /// Every sandbox the host holds, paused or not.
    pub fn held_sandboxes(&self) -> Vec<SandboxId> {
        self.sandboxes.keys().copied().collect()
    }

    /// The sandboxes whose guest runs: held and not paused for a capture.
    pub fn active_sandboxes(&self) -> Vec<SandboxId> {
        self.sandboxes
            .keys()
            .copied()
            .filter(|id| !self.capture_in_flight(*id))
            .collect()
    }

    /// The volume invariant every held sandbox must satisfy: its live root
    /// volume and its latest root checkpoint both exist. A reconcile that
    /// deleted either severed a resident guest or lost its lineage.
    pub async fn volumes_intact(&self) -> Result<(), String> {
        for id in self.sandboxes.keys() {
            self.volumes.live(*id, DiskRole::Root).await.map_err(|e| {
                format!("volumes-intact: sandbox {id} has no live root volume: {e}")
            })?;
            self.volumes
                .checkpoint_of(*id, DiskRole::Root)
                .await
                .map_err(|e| format!("volumes-intact: sandbox {id} has no root checkpoint: {e}"))?;
        }
        Ok(())
    }

    /// A unit of guest work: write one real content-tagged chunk (advancing
    /// the backend's dirty tier) and bump the work cursor.
    pub async fn guest_write(&mut self, sandbox_id: SandboxId) -> Result<(), String> {
        let tag = self.next_tag();
        if self.capture_in_flight(sandbox_id) {
            return Ok(()); // paused
        }
        let Some(s) = self.sandboxes.get_mut(&sandbox_id) else {
            return Ok(());
        };
        let volume = self
            .volumes
            .live(sandbox_id, DiskRole::Root)
            .await
            .map_err(|e| format!("guest_write {sandbox_id}: open root: {e}"))?;
        volume
            .write_chunk(ChunkIndex(tag % NUM_CHUNKS), synth_chunk(tag))
            .await
            .map_err(|e| format!("guest_write {sandbox_id}: {e}"))?;
        s.cursor += 1;
        Ok(())
    }

    /// The periodic-checkpoint disk leg: take and publish the root
    /// checkpoint (the REAL `SessionVolumes::checkpoint`).
    pub async fn flush(&mut self, sandbox_id: SandboxId) -> Result<(), String> {
        if !self.sandboxes.contains_key(&sandbox_id) || self.capture_in_flight(sandbox_id) {
            return Ok(());
        }
        self.volumes
            .checkpoint(sandbox_id, DiskRole::Root)
            .await
            .map(|_| ())
            .map_err(|e| format!("flush {sandbox_id}: {e}"))
    }

    /// The eviction capture leg (`PooledBackend::snapshot_begin`): take the
    /// root checkpoint under the pause, persist a REAL
    /// `EvictionFinalizeRecord`, take the capture lock. Idempotent: a
    /// pending finalize re-observes the same snapshot id.
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

        // The eviction/manual capture path reaches the same Firecracker
        // `prepare_save` hook as a periodic checkpoint. Every live guest
        // connection ends, but the journal survives on the guest disk.
        self.sever_exec_transports(sandbox_id);

        let snapshot_id = SnapshotId::from(self.entropy.uuid());
        let dest = self
            .root
            .path()
            .join("staging")
            .join(snapshot_id.to_string());
        tokio::fs::create_dir_all(&dest)
            .await
            .map_err(|e| format!("snapshot_begin staging dir: {e}"))?;
        tokio::fs::write(dest.join("state.bin"), b"cosim fc state")
            .await
            .map_err(|e| format!("snapshot_begin state.bin: {e}"))?;

        // The checkpoint taken under the pause: the transient volume the
        // finalize publishes by name.
        let root = self
            .volumes
            .take(sandbox_id, DiskRole::Root)
            .await
            .map_err(|e| format!("snapshot_begin take root checkpoint: {e}"))?;
        let disk_pending = vec![PendingDisk {
            role: DiskRole::Root,
            previous: root.previous(),
        }];
        drop(root);

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
            disk_pending,
            // ADR 0035 amendment: the capture pins what the sandbox actually
            // has attached, so the REAL finalize's bundle-publish leg
            // (`run_upload_blobs` → `BundleStore::publish`) runs (the leg a
            // sweep-before-publish race breaks).
            aux_bundles,
            stage: FinalizeStage::Captured,
            attempts: 0,
            swap_manifest: None,
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
        // Take the capture lock (view mirror); the VM is paused for it.
        self.view.inner.lock().capturing.insert(sandbox_id);
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
                // Capture lock released; the terminal leg destroyed the VM.
                self.pending_finalizes.remove(&sandbox_id);
                self.finalize_cursor.remove(&snapshot_id);
                self.remove_sandbox(sandbox_id).await;
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

    /// Test hook (the capture-lock release pin): delete the in-flight
    /// finalize's staging dir and release the sandbox's volumes. The
    /// release keeps the checkpoint the pending one was taken over, so the
    /// disk leg finds its pending checkpoint gone and never published, and
    /// every redrive fails. The REAL `run_eviction_finalize_attempt` then
    /// QUARANTINES after the redrive budget, which proves the quarantine
    /// arm releases the capture lock (`pending_finalizes` entry cleared), so
    /// #783's teardown-reconcile capture-in-flight exemption can never
    /// become a permanent reap-shield.
    pub async fn sabotage_finalize_inputs(&self, sandbox_id: SandboxId) {
        if let Some(record) = self.in_flight.get(&sandbox_id) {
            let _ = tokio::fs::remove_dir_all(&record.dest).await;
        }
        let _ = self.volumes.release(sandbox_id).await;
    }

    /// Reconcile's local reap: cancel any in-flight finalize (the #570
    /// loss), drop the VM, record the destroy in the oracle log.
    async fn reconcile_destroy(&mut self, sandbox_id: SandboxId) {
        // Cancelling a mid-upload finalize is the corruption: its inputs
        // are dropped and no snapshot row will ever land.
        self.in_flight.remove(&sandbox_id);
        if let Some((_, snap)) = self.pending_finalizes.remove(&sandbox_id) {
            self.finalize_cursor.remove(&snap);
        }
        self.view.inner.lock().destroyed.push(sandbox_id);
        self.remove_sandbox(sandbox_id).await;
    }

    /// Drop the VM and release its volumes (the `PooledBackend::destroy`
    /// order: the sandbox leaves the live set, then the disk).
    async fn remove_sandbox(&mut self, sandbox_id: SandboxId) {
        if self.sandboxes.remove(&sandbox_id).is_none() {
            return;
        }
        {
            let mut view = self.view.inner.lock();
            view.live.remove(&sandbox_id);
            view.bindings.remove(&sandbox_id);
            view.capturing.remove(&sandbox_id);
        }
        if let Err(e) = self.volumes.release(sandbox_id).await {
            tracing::warn!(%sandbox_id, error = %e, "cosim: release volumes");
        }
    }

    fn finalizer(&self) -> EvictionFinalizer {
        EvictionFinalizer::new(
            Some(self.volumes.clone()),
            Some(self.store.clone()),
            None,
            self.bundle_dir(),
            BUNDLE_EXT,
            self.root.path().to_path_buf(),
            Arc::new(tokio::sync::Notify::new()),
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
        self.host.lock().await.reconcile_destroy(id).await;
        Ok(())
    }
}
