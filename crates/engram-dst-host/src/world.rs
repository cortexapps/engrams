//! The simulated host world and the acked-write ledger (ADR 0098 Phase 2).
//!
//! [`SimHost`] models one host-agent process. A process death splits its state
//! in two parts.
//!
//! * Process memory contains each live `ChunkedDiskBackend`. Process death
//!   drops these values.
//! * Disk contains the stable dirty files, records, manifests, chunks, and
//!   cache data. All of this state survives process death.
//! * Oracle memory contains the [`AckedWriteLedger`]. It records each write
//!   that the guest saw as complete.
//!
//! Guest writes use synthetic chunks with content tags. A read decodes the
//! tag and identifies the exact write. Tag zero is the base content.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::manifest::{
    ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
};
use engram_chunk_store::store::ChunkStore;
use engram_core::traits::{BlobStorage, Clock as _, Entropy as _};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::SnapshotId;
use engram_core::{HostId, SandboxId, SessionId};
#[cfg(target_os = "linux")]
use engram_host_agent::disk_daemon::backend::{read_ref_sidecar, resolve_recover_attach_ref};
use engram_host_agent::disk_daemon::backend::{
    ChunkedDiskBackend, DirtyFileOpenMode, FlushSeamPoint,
};
use engram_host_agent::disk_daemon::{NbdSlot, NbdSlotAllocator};
use engram_host_agent::eviction_finalize::{
    persist_disk_pending_chunks, run_eviction_finalize_attempt, DiskPendingRecord,
    EvictionFinalizeRecord, EvictionFinalizer, EvictionSandbox, FinalizeAttempt,
};
use engram_host_agent::migration::{self, MigrationExport, MigrationRegistry, TtlVerdict};
use engram_host_core::{
    FinalizeStage, HostEffects, HostFs, LiveManifestPublishOutcome, LiveManifestPublishRequest,
};
use engram_sim::{SimClock, SimEntropy};
use engram_storage_local::LocalBlobStorage;

use crate::coord_stub::SimCoordClient;
use crate::effects::{sim_effects, SeamLog};
use crate::fs_crash::CrashFs;
use crate::reconcile::SimReconcileBackend;
use crate::simfs::SimFs;

/// Disk geometry every sim sandbox uses. Small on purpose — the oracle
/// asserts a correctness property (acked writes survive), not throughput, so
/// the least data that demonstrates it keeps the swarm fast (AGENTS.md: size
/// a test to the property).
pub const CHUNK_SIZE: u64 = 4096;
/// Chunks per sandbox disk → a 32 KiB disk; `chunk_idx` ranges `0..NUM_CHUNKS`.
pub const NUM_CHUNKS: u64 = 8;

/// Build the deterministic bytes for a chunk stamped with `content_tag`:
/// `tag.to_le_bytes()` tiled across the whole chunk. The first 8 bytes are
/// `tag.to_le_bytes()`, so [`decode_tag`] recovers it from any prefix.
pub fn synth_chunk(content_tag: u64) -> Vec<u8> {
    let stamp = content_tag.to_le_bytes();
    let mut out = vec![0u8; CHUNK_SIZE as usize];
    for (i, b) in out.iter_mut().enumerate() {
        *b = stamp[i % 8];
    }
    out
}

/// Recover the `content_tag` a chunk's bytes were stamped with (its first 8
/// bytes, little-endian). A never-written chunk reads back base content
/// (tag 0).
pub fn decode_tag(bytes: &[u8]) -> u64 {
    let mut le = [0u8; 8];
    let n = bytes.len().min(8);
    le[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(le)
}

/// One appended acked-write fact: the oracle's per-write memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    /// Sandbox slot index (stable across a run).
    pub sandbox: usize,
    pub chunk_idx: u64,
    /// The synthetic write's content tag.
    pub content_tag: u64,
    /// The manifest lineage at the time of the ack.
    pub lineage_at_ack: ManifestRef,
}

/// The append-only acked-write ledger is the oracle's durable memory. It
/// survives [`CrashProcess`](crate::Step::CrashProcess). The latest tag for a
/// chunk is the exact value that recovery must return. The published floor is
/// used only by the quiescence check.
#[derive(Default)]
pub struct AckedWriteLedger {
    log: Vec<LedgerEntry>,
    /// The highest published tag for each chunk.
    published_floor: BTreeMap<(usize, u64), u64>,
}

impl AckedWriteLedger {
    /// Append a fact when a `write_chunk` returns Ok (the guest is acked).
    pub fn record(&mut self, entry: LedgerEntry) {
        self.log.push(entry);
    }

    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Raise `(sandbox, chunk_idx)`'s published floor to `content_tag` — the tag
    /// a flush was OBSERVED to publish in the real durable manifest. Monotonic
    /// (takes the max): the published tier never loses a chunk, so the floor
    /// never decreases. A base (tag-0) chunk in a published manifest leaves an
    /// existing higher floor untouched.
    pub fn mark_published(&mut self, sandbox: usize, chunk_idx: u64, content_tag: u64) {
        let slot = self
            .published_floor
            .entry((sandbox, chunk_idx))
            .or_insert(0);
        *slot = (*slot).max(content_tag);
    }

    /// Get the highest published tag for a chunk.
    pub fn handed_off_tag(&self, sandbox: usize, chunk_idx: u64) -> Option<u64> {
        self.published_floor.get(&(sandbox, chunk_idx)).copied()
    }

    /// The recoverable set: the LATEST acked tag per `(sandbox, chunk_idx)`
    /// (an overwrite supersedes the earlier tag — only the last write is the
    /// value the guest expects to read back). BTreeMap so iteration order is
    /// deterministic.
    pub fn latest_by_chunk(&self) -> BTreeMap<(usize, u64), LedgerEntry> {
        let mut out: BTreeMap<(usize, u64), LedgerEntry> = BTreeMap::new();
        for e in &self.log {
            out.insert((e.sandbox, e.chunk_idx), e.clone());
        }
        out
    }
}

/// One sandbox slot. The dirty file and coordinator pointer survive process
/// death. The backend value does not survive.
pub struct SandboxSlot {
    pub sandbox_id: SandboxId,
    pub session_id: SessionId,
    /// The private base manifest the sandbox was created on (in the store).
    pub base_ref: ManifestRef,
    /// The last manifest ref that the coordinator accepted.
    pub published_ref: Option<ManifestRef>,
    /// The live backend. This is `None` after process death.
    pub backend: Option<Arc<ChunkedDiskBackend>>,

    // ── Flow B: the NBD slot/reattach device-serving model (ADR 0098 P7) ──
    /// The `/dev/nbdN` this sandbox's rootfs is served over.
    pub nbd_device: std::path::PathBuf,
    /// The current generation's real slot lease from [`SimHost::nbd_pool`].
    /// `Some` while this generation serves the device; forgotten (not
    /// released) on a roll, exactly like `abandon_for_shutdown`.
    pub lease: Option<NbdSlot>,
    /// The host-agent generation whose serve socket the kernel currently
    /// serves this device with (`RECONFIGURE`d). `None` = unserved (post-roll,
    /// pre-rehydrate, or after a stale-sweep DISCONNECT). "served by THIS
    /// generation" (the un-pause gate) is `served_by == Some(host.generation)`.
    pub served_by: Option<u32>,
    /// The generation the kernel records as the device's configuring owner
    /// (`/sys/block/nbdN/pid` stand-in). SURVIVES a roll — only a DISCONNECT
    /// clears it — so the stale-binding sweep probes it against liveness.
    pub kernel_owner: Option<u32>,
    /// Rung-2 parked (evicting-shaped: FC paused, VM resident). The 731df805
    /// class was a parked survivor whose device the sweep disconnected.
    pub parked: bool,
    /// ADR 0098 G2: the slot's last completed finalize published NO disk
    /// manifest — a poisoned lineage (only reachable via the pre-#743
    /// silent-skip capture the G2 seeds reproduce). A resume of this
    /// snapshot has no manifest to attach; the sidecar still names the
    /// capture-time literal device.
    pub poisoned_snapshot: bool,
    /// ADR 0098 P8 (Flow E): a migration export is open — the guest is
    /// FROZEN (the source is a page server); writes/flushes/captures are
    /// excluded exactly as the export's held capture lock excludes them
    /// in prod. Cleared by commit/abort/destroy or a process death (the
    /// registry is RAM).
    pub migrating: bool,
    /// R6 (ADR 0098 §Phase 3, #784 layer 1 / #769 gap A): does the FC guest
    /// process still hold this sandbox's `/dev/nbdN` node open? The guest is a
    /// SEPARATE process from the host-agent, so it SURVIVES a host-agent roll
    /// (the whole survivor premise) and keeps reading its rootfs across the
    /// gap. This is the world-side twin of the prod `device_has_live_holder`
    /// proc-scan: a live holder ⇒ [`DeviceHolder::LiveHolder`], driving the
    /// stale-binding sweep to PARK (never DISCONNECT) a dead-owner device the
    /// rehydrate passes missed. `false` models a genuinely-gone guest (FC
    /// crashed/destroyed) ⇒ `NoHolder` ⇒ a DISCONNECT is legal.
    pub guest_holds_device: bool,
    /// Wave 7b (ADR 0098 §Phase 3, #784 layers 2–3): does a TRACKED RECORD (the
    /// coordinator rehydrate list OR the durable `ChainHeadRecord`) reference
    /// this device? The Layer-2 reconcile: the startup classification barrier
    /// matches records AGAINST the kernel-derived inventory, so a device with NO
    /// record — a survivor whose records were lost upstream (#769 gap A) — is
    /// QUARANTINED, never silently skipped. `false` models that record loss.
    /// Mirrors prod, where the record set is built from `rootfs_device` over the
    /// resident-and-recorded sandboxes: a device is "recorded" while a live
    /// guest still holds it (`record_present && guest_holds_device` — a
    /// genuinely-gone guest's FC is not resident, so `rootfs_device` returns
    /// `None` and the device is reap-eligible on proof of death) — OR while
    /// this generation quarantine-parked it (`quarantine_parked` below).
    pub record_present: bool,
    /// The allocator's quarantine-park (PR #828): THIS generation's rehydrate
    /// failed for the device and `slot.quarantine()` registered it in the
    /// allocator's parked set — a device-keyed record source that keeps the
    /// survivor TRACKED even after its FC-derived record vacates (the
    /// 2026-07-21 false `rehydrate-unknown-device` alarm). Distinct from the
    /// rung-2 `parked` flag. Per-PROCESS state: cleared on roll — the
    /// successor's fresh allocator has no memory of the predecessor's parks.
    pub quarantine_parked: bool,
}

impl SandboxSlot {
    /// The ref a successor rebuilds from: the last publish, else the base.
    pub fn rebuild_ref(&self) -> ManifestRef {
        self.published_ref.unwrap_or(self.base_ref)
    }
}

/// [`SimHost::snapshot_begin`]'s outcome — the G2 capture-leg verdicts
/// made observable for the seeds (the swarm treats every variant as a
/// benign step).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// A finalize began.
    Began(SnapshotId),
    /// Idempotent re-observation of the pending finalize (the
    /// capture-lock lockout's observable contract).
    AlreadyPending(SnapshotId),
    /// The G2 refusal: an untracked RESIDENT survivor — capturing would
    /// record a manifestless snapshot and drop its acked writes.
    RefusedUntracked,
    /// Nothing capturable at this slot (no VM at all).
    NotCapturable,
}

/// [`SimHost::resume_finalized`]'s outcome — the G2 resume-leg verdicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// The chunked attach path took; the sandbox rebuilt from its
    /// snapshot's disk manifest.
    Attached,
    /// The G2 refusal: a poisoned (manifestless) snapshot on a
    /// data-plane host — no boot onto the stale literal device.
    RefusedStaleLiteral,
    /// PRE-#743 only (seed-driven): FC booted onto the capture-time
    /// literal device's current content.
    BootedStaleLiteral,
}

/// The sim's [`EvictionSandbox`] — records every terminal-destroy the REAL
/// `run_terminal` issues (the coordinator-side teardown is out of the host
/// sim's scope; the record is the observable).
#[derive(Default)]
pub struct SimEvictionSandbox {
    destroyed: parking_lot::Mutex<Vec<SandboxId>>,
}

impl SimEvictionSandbox {
    pub fn destroyed(&self) -> Vec<SandboxId> {
        self.destroyed.lock().clone()
    }

    /// Consume the destroy record for `id`: a finalized-snapshot resume
    /// re-created the sandbox, so it is a live VM again — not a terminal
    /// corpse the recovery legs must keep refusing to rebuild.
    pub fn forget(&self, id: SandboxId) {
        self.destroyed.lock().retain(|d| *d != id);
    }
}

#[async_trait::async_trait]
impl EvictionSandbox for SimEvictionSandbox {
    async fn destroy(&self, id: SandboxId) -> Result<(), engram_core::SandboxError> {
        self.destroyed.lock().push(id);
        Ok(())
    }
}

/// The sim's finalize redrive cap. Small so the quarantine arm + the
/// convergence drain stay cheap; the prod default (10) only stretches the
/// same ladder.
pub const SIM_FINALIZE_MAX_ATTEMPTS: u32 = 3;

/// One simulated host-agent process over a per-run disk.
pub struct SimHost {
    pub host_id: HostId,
    pub clock: Arc<SimClock>,
    pub entropy: Arc<SimEntropy>,
    pub fs: SimFs,
    pub store: Arc<ChunkStore>,
    pub coord: Arc<SimCoordClient>,
    pub effects: HostEffects,
    pub seam_log: Arc<SeamLog>,
    pub sandboxes: Vec<SandboxSlot>,
    /// Flow C (ADR 0098 P3): the teardown-reconcile world the REAL
    /// `reconcile_once` drives. Seeded in lockstep with `sandboxes` (same
    /// ids, same coord ownership) but a SEPARATE concern — reconcile reaps
    /// bindings, the disk model serves chunks; a reconcile destroy does not
    /// touch a `SandboxSlot`.
    pub reconcile: Arc<SimReconcileBackend>,
    pub ledger: AckedWriteLedger,
    /// Monotonic content-tag source. Advances only on writes, so the id a
    /// given step mints is a pure function of the step sequence (seed).
    next_tag: u64,

    // ── Flow B: the NBD slot allocator + generation (ADR 0098 P7) ──
    /// The current process generation (a pid stand-in). Bumped on every roll
    /// ([`crash_process`](SimHost::crash_process)); the successor's rehydrate
    /// re-serves under the new generation while the kernel devices retain the
    /// old (now dead) owner until re-served or swept.
    pub generation: u32,
    /// The REAL portable slot allocator for THIS generation. A roll replaces
    /// it (a fresh host-agent process builds a fresh pool; the kernel device
    /// bindings survive in `SandboxSlot::kernel_owner`). Devices
    /// `0..num_sandboxes` are the sandbox rootfs devices; `num_sandboxes..cap`
    /// are spares the populator warms + `SlotClaim`/`SlotPopulateTick` exercise.
    pub nbd_pool: Arc<NbdSlotAllocator>,
    /// The pool's device universe size (constant across generations).
    pub nbd_capacity: u32,
    /// Spare-device leases held for the slot-accounting exercise (dropped on a
    /// roll). Separate from per-sandbox `lease`s so the accounting oracle sees
    /// every held slot.
    spare_leases: Vec<NbdSlot>,

    // ── Flow D: eviction finalize (ADR 0098 P5) ──
    /// The `snapshot_begin` idempotency map — RAM (the real
    /// `PooledBackend::pending_finalizes` DashMap): dies on crash, rebuilt by
    /// the restart resume leg from the durable records. Shared with every
    /// [`EvictionFinalizer`] this host builds.
    pub pending_finalizes: Arc<dashmap::DashMap<SandboxId, SnapshotId>>,
    /// The in-flight finalize job state per slot idx — RAM (the spawned
    /// job's `record` local). Dies on crash; the durable record on disk is
    /// what the restart resume leg re-drives from.
    in_flight: BTreeMap<usize, EvictionFinalizeRecord>,
    /// The terminal-destroy recorder (the sim [`EvictionSandbox`]).
    pub destroyer: Arc<SimEvictionSandbox>,
    /// Oracle memory (survives crashes): every finalize ever started, for the
    /// convergence oracle (#8) — each must reach completed-or-quarantined by
    /// quiescence.
    pub finalize_started: std::collections::BTreeSet<SnapshotId>,
    /// Oracle memory (survives crashes): the highest stage ever observed
    /// per snapshot — the FinalizeStage monotonicity watermark (#6).
    /// A Mutex so the read-only oracle pass can update it.
    pub finalize_stage_seen: parking_lot::Mutex<BTreeMap<SnapshotId, FinalizeStage>>,

    // ── Flow E: migration (ADR 0098 P8) ──
    /// The REAL per-host export registry — RAM (dies on crash, like the
    /// prod DashMap). The TTL sweep drives the REAL `expired()` over the
    /// injected paused clock and the REAL `ttl_verdict` over the
    /// (adversarial, scriptable) coordinator's ownership answer.
    pub migrations: Arc<MigrationRegistry>,
    /// Oracle #7's structural catch: any abort-unpause applied to an
    /// export whose `state_served` was set is recorded here — the #216
    /// split-brain the decision table forbids. Oracle memory (survives).
    pub split_brain_unpauses: Vec<SandboxId>,

    // ── Wave 7b: the startup classification barrier (ADR 0098 §Phase 3, #784) ──
    /// Oracle memory (survives crashes): every sandbox the barrier ever
    /// classified [`SlotClass::QuarantinedUnknown`](engram_host_core::SlotClass::QuarantinedUnknown)
    /// — a kernel-connected device with a live holder that no tracked record
    /// accounted for (#769 gap A). The `record-invisible-survivor-classified`
    /// oracle asserts every such survivor was CLASSIFIED here, never silently
    /// skipped-or-severed.
    pub quarantined_unknown: std::collections::BTreeSet<SandboxId>,
}

impl SimHost {
    /// Build a host with `num_sandboxes` sandboxes, each on its own private
    /// base manifest (so flushes version a private lineage — no shared-base
    /// fork bookkeeping). Async because seeding the store + building backends
    /// awaits real (deterministic, content-addressed) I/O.
    pub async fn new(seed: u64, num_sandboxes: usize) -> Self {
        let clock = SimClock::new();
        let entropy = Arc::new(SimEntropy::seeded(seed));
        let fs = SimFs::new().expect("sim tempdir");
        let blob: Arc<dyn BlobStorage> =
            Arc::new(LocalBlobStorage::new(fs.chunks_dir().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let coord = Arc::new(SimCoordClient::new());
        let reconcile = Arc::new(SimReconcileBackend::new());
        let (effects, seam_log) = sim_effects(clock.clone(), entropy.clone(), coord.clone());
        // Deterministic, readable host id.
        let host_id = HostId::from(uuid::Uuid::from_u128(0x0A57_0000));

        // Flow B: the generation-1 slot allocator. `num_sandboxes` rootfs
        // devices + 2 spares (for the warm/validation-window/`SlotClaim`
        // exercise); `warm_target = 1` so the populator warms exactly one
        // spare when time advances. Each sandbox claims its own `/dev/nbdN`.
        let nbd_capacity = num_sandboxes as u32 + 2;
        let generation = 1u32;
        let nbd_pool = NbdSlotAllocator::with_capacity(nbd_capacity, 0);

        let mut sandboxes = Vec::with_capacity(num_sandboxes);
        for i in 0..num_sandboxes {
            let sandbox_id = SandboxId::from(uuid::Uuid::from_u128(0x5B00_0000 + i as u128));
            let session_id = SessionId::from(uuid::Uuid::from_u128(0x5E55_0000 + i as u128));
            // Private base manifest: N chunks all pointing at the single
            // deduped base chunk (tag 0), rooted at an entropy-minted id.
            let base_hash = store
                .put_chunk(&synth_chunk(0))
                .await
                .expect("seed base chunk");
            let base_ref = ManifestRef {
                manifest_id: entropy.uuid(),
                version: 1,
            };
            let manifest = base_manifest(base_hash);
            store
                .put_manifest(base_ref, &manifest)
                .await
                .expect("seed base manifest");
            let backend = build_backend(
                &store,
                fs.cache_dir(),
                fs.dirty_dir(),
                i,
                sandbox_id,
                base_ref,
                DirtyFileOpenMode::Truncate,
            )
            .await;
            coord.set_owner(sandbox_id, session_id);
            // Flow C steady state: the sandbox is live, coord-owned, and
            // locally bound — reconcile is a no-op until something perturbs it.
            reconcile.seed_sandbox(sandbox_id, session_id);
            // Flow B: claim this sandbox's rootfs device on the gen-1 pool.
            let nbd_device = Self::device_path(i);
            let lease = nbd_pool
                .claim(&nbd_device)
                .await
                .expect("gen-1 claim of a free sandbox device");
            sandboxes.push(SandboxSlot {
                sandbox_id,
                session_id,
                base_ref,
                published_ref: None,
                backend: Some(Arc::new(backend)),
                nbd_device,
                lease: Some(lease),
                served_by: Some(generation),
                kernel_owner: Some(generation),
                parked: false,
                poisoned_snapshot: false,
                migrating: false,
                // A fresh sandbox's guest is resident and holds its rootfs
                // device open.
                guest_holds_device: true,
                // A fresh sandbox has a tracked record (coord ownership + a
                // chain-head record); record loss is a seeded/faulted event.
                record_present: true,
                quarantine_parked: false,
            });
        }

        Self {
            host_id,
            clock,
            entropy,
            fs,
            store,
            coord,
            effects,
            seam_log,
            sandboxes,
            reconcile,
            ledger: AckedWriteLedger::default(),
            next_tag: 0,
            generation,
            nbd_pool,
            nbd_capacity,
            spare_leases: Vec::new(),
            pending_finalizes: Arc::new(dashmap::DashMap::new()),
            in_flight: BTreeMap::new(),
            destroyer: Arc::new(SimEvictionSandbox::default()),
            finalize_started: std::collections::BTreeSet::new(),
            finalize_stage_seen: parking_lot::Mutex::new(BTreeMap::new()),
            migrations: Arc::new(MigrationRegistry::default()),
            split_brain_unpauses: Vec::new(),
            quarantined_unknown: std::collections::BTreeSet::new(),
        }
    }

    /// The number of sandboxes seeded into the reconcile world (== the disk
    /// sandbox count). A stable index space `0..num` for the perturbation
    /// steps.
    pub fn num_sandboxes(&self) -> usize {
        self.sandboxes.len()
    }

    /// The sandbox/session ids for reconcile-world slot `idx` (the same ids
    /// seeded at construction).
    fn reconcile_ids(&self, idx: usize) -> Option<(SandboxId, SessionId)> {
        self.sandboxes
            .get(idx)
            .map(|s| (s.sandbox_id, s.session_id))
    }

    /// Drive one REAL `reconcile_once` tick against the sim world, over the
    /// caller-owned `strikes` ledger. Flow C's end-to-end path: list →
    /// classify (pure core + a coord call per sandbox) → strike-debounce →
    /// destroy/repair. Returns the list()-failure error verbatim (benign in
    /// the sim — `list()` never fails).
    pub async fn reconcile_tick(
        &self,
        strikes: &mut std::collections::HashMap<SandboxId, u32>,
    ) -> Result<(), String> {
        engram_host_agent::teardown_reconcile::reconcile_once(
            &*self.reconcile,
            &*self.coord,
            self.host_id,
            strikes,
        )
        .await
        .map_err(|e| format!("reconcile_once: {e}"))
    }

    /// Perturbation: drop the LOCAL binding for reconcile slot `idx` (the ADR
    /// 0090 survivor — coord still owns it). Next reconcile must REPAIR, not
    /// reap.
    pub fn drop_local_binding(&self, idx: usize) {
        if let Some((sandbox_id, _)) = self.reconcile_ids(idx) {
            self.reconcile.drop_local_binding(sandbox_id);
        }
    }

    /// Perturbation: revoke coordinator ownership of reconcile slot `idx` (a
    /// terminal/idle/rebound session). Reconcile SHOULD reap it after the
    /// strike debounce — and Oracle #9 confirms the coord genuinely no longer
    /// owns it, so the reap is correct.
    pub fn revoke_ownership(&self, idx: usize) {
        if let Some((sandbox_id, _)) = self.reconcile_ids(idx) {
            self.coord.revoke_owner(sandbox_id);
        }
    }

    /// Is sandbox `idx` frozen under an in-flight eviction finalize? The
    /// prod capture lock excludes every concurrent lifecycle op for the
    /// whole finalize (the guest is paused; the VM is destroyed at
    /// terminal) — the sim mirrors it by gating the register-time re-serve
    /// on this (the swarm found the resurrection:
    /// a re-serve rebuilt a captured sandbox from the OLD published ref,
    /// then the finalize published a NEWER floor over it).
    fn finalize_pending(&self, idx: usize) -> bool {
        self.pending_finalizes
            .contains_key(&self.sandboxes[idx].sandbox_id)
    }

    /// #898: was slot `idx`'s VM terminally destroyed by a completed
    /// eviction finalize? The destroy record is coordinator-side truth
    /// ([`SimHost::destroyer`] survives process rolls), so every recovery
    /// leg must consult it: production rehydrates only coordinator-provided
    /// survivors with a resident rootfs, and a terminally destroyed sandbox
    /// is neither. Only a finalized-snapshot resume
    /// ([`resume_finalized`](Self::resume_finalized)) re-creates it.
    fn terminally_destroyed(&self, idx: usize) -> bool {
        self.destroyer
            .destroyed()
            .contains(&self.sandboxes[idx].sandbox_id)
    }

    fn next_tag(&mut self) -> u64 {
        self.next_tag += 1;
        self.next_tag
    }

    /// A guest write of a fresh content-tagged chunk. Appends the ledger fact
    /// when `write` returns (the guest-ack instant). No-op on a crashed
    /// backend (the guest can't reach a dead daemon).
    pub async fn guest_write(&mut self, idx: usize, chunk_idx: u64) -> Result<(), String> {
        if idx >= self.sandboxes.len() || chunk_idx >= NUM_CHUNKS {
            return Ok(());
        }
        if self.sandboxes[idx].migrating {
            return Ok(()); // the frozen source acks nothing
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        let tag = self.next_tag();
        let offset = chunk_idx * CHUNK_SIZE;
        backend
            .write(offset, &synth_chunk(tag))
            .await
            .map_err(|e| format!("guest_write sandbox {idx} chunk {chunk_idx}: {e}"))?;
        let lineage = backend.manifest_ref().await;
        self.ledger.record(LedgerEntry {
            sandbox: idx,
            chunk_idx,
            content_tag: tag,
            lineage_at_ack: lineage,
        });
        Ok(())
    }

    /// Read a chunk and compare it with the latest guest-acked value.
    pub async fn guest_read(&self, idx: usize, chunk_idx: u64) -> Result<(), String> {
        if idx >= self.sandboxes.len() || chunk_idx >= NUM_CHUNKS {
            return Ok(());
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        let bytes = backend
            .read(chunk_idx * CHUNK_SIZE, CHUNK_SIZE)
            .await
            .map_err(|e| format!("guest_read sandbox {idx} chunk {chunk_idx}: {e}"))?;
        let got = decode_tag(&bytes);
        if let Some(latest) = self
            .ledger
            .latest_by_chunk()
            .get(&(idx, chunk_idx))
            .map(|e| e.content_tag)
        {
            if got != latest {
                return Err(format!(
                    "read-after-write: sandbox {idx} chunk {chunk_idx} read tag {got}, but the \
                     latest acked tag is {latest}"
                ));
            }
        }
        Ok(())
    }

    /// Upload dirty chunks and publish the new manifest ref.
    pub async fn flush_tick(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len() || self.sandboxes[idx].migrating {
            return Ok(()); // the export's capture lock excludes flushes
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        backend
            .flush()
            .await
            .map_err(|e| format!("flush sandbox {idx}: {e}"))?;
        self.note_flush_published(idx).await
    }

    /// Record a completed backend flush and tell the coordinator.
    pub async fn note_flush_published(&mut self, idx: usize) -> Result<(), String> {
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        let published = backend.manifest_ref().await;
        // Read the manifest from the store before the simulator records it.
        self.mark_flush_published(idx, published).await?;
        let req = LiveManifestPublishRequest {
            session_id: self.sandboxes[idx].session_id,
            sandbox_id: self.sandboxes[idx].sandbox_id,
            manifest_id: published.manifest_id,
            manifest_version: published.version,
        };
        let response = self
            .effects
            .coord
            .publish_live_manifest(self.host_id, &req)
            .await
            .map_err(|e| format!("publish sandbox {idx}: {e}"))?;
        if response.outcome == LiveManifestPublishOutcome::Applied {
            self.sandboxes[idx].published_ref = Some(published);
        }
        Ok(())
    }

    /// Raise the published-tier floor for every chunk a flush just published.
    /// Reads the published manifest and its chunks back out of the store — the
    /// REAL durable state — and stamps each positional chunk's decoded tag as
    /// the (permanent, monotonic) floor. A base (never-written) chunk decodes to
    /// tag 0 and leaves any existing floor untouched.
    async fn mark_flush_published(
        &mut self,
        idx: usize,
        published: ManifestRef,
    ) -> Result<(), String> {
        let manifest = self
            .store
            .get_manifest(published)
            .await
            .map_err(|e| format!("mark_flush_published get_manifest sandbox {idx}: {e}"))?;
        for cref in &manifest.chunks {
            let chunk_idx = cref.offset / CHUNK_SIZE;
            let bytes = self
                .store
                .get_chunk(cref.hash)
                .await
                .map_err(|e| format!("mark_flush_published get_chunk sandbox {idx}: {e}"))?;
            self.ledger
                .mark_published(idx, chunk_idx, decode_tag(&bytes));
        }
        Ok(())
    }

    /// Drop the process state. Stable dirty files survive this event.
    /// Orderly and abrupt process deaths have the same disk result.
    pub async fn crash_process(&mut self) -> Result<(), String> {
        self.die_process();
        Ok(())
    }

    /// Flow B (ADR 0098 P7): a host-agent process death is a ROLL — the
    /// successor comes up as a fresh GENERATION with a fresh slot pool. The
    /// kernel `/dev/nbdN` devices, and the OWNER pid the kernel recorded for
    /// them, SURVIVE (the pod-roll survival contract); only the in-process
    /// serve sockets and slot leases die. So `served_by` clears (no current-gen
    /// serve socket) while `kernel_owner` persists at the now-DEAD generation
    /// until the successor re-serves the device (RECONFIGURE) or the
    /// stale-binding sweep disconnects it. `parked` persists — the FC VM stays
    /// resident across the roll.
    fn roll_generation(&mut self) {
        self.generation += 1;
        for slot in &mut self.sandboxes {
            // Drop the dead generation's lease (the old pool is discarded; its
            // async release is unobservable). served_by clears; kernel_owner +
            // parked persist. quarantine_parked CLEARS — the allocator's
            // parked set is in-process memory, and the successor's fresh
            // allocator has no record of the predecessor's parks.
            slot.lease = None;
            slot.served_by = None;
            slot.quarantine_parked = false;
        }
        self.spare_leases.clear();
        // The successor builds a fresh pool (all devices free — it discovers
        // the surviving kernel bindings via rehydrate + the sweep).
        self.nbd_pool = NbdSlotAllocator::with_capacity(self.nbd_capacity, 0);
    }

    // ─────────────────────── Flow B lifecycle (ADR 0098 P7) ───────────────────

    /// Is sandbox `idx` a resident survivor whose device is bound to a DEAD
    /// (prior) generation — i.e. it needs rehydrating? (The reattach pass found
    /// the FC config; the kernel device is still configured under the old pid.)
    fn is_resident_survivor(&self, idx: usize) -> bool {
        self.sandboxes[idx]
            .kernel_owner
            .is_some_and(|g| g < self.generation)
    }

    /// Rung-2 PARK sandbox `idx`: the FC VM pauses but stays RESIDENT and its
    /// NBD device keeps being served by the current generation (status
    /// `evicting`-shaped). Models the coordinator parking a session at eviction
    /// rung 2 — the exact pre-condition of the 731df805 incident. A no-op on a
    /// sandbox this generation is not currently serving.
    pub fn park(&mut self, idx: usize) {
        if let Some(slot) = self.sandboxes.get_mut(idx) {
            if slot.served_by == Some(self.generation) {
                slot.parked = true;
            }
        }
    }

    /// Claim + serve sandbox `idx`'s device on the CURRENT generation
    /// (RECONFIGURE), rebuilding its backend if the RAM died. Idempotent: a
    /// device already served this generation is skipped (mirrors
    /// `rehydrate_sandbox`'s `nbd_sandboxes` presence check). The device is
    /// FREE in the fresh post-roll pool, so `claim`'s fast path takes it with
    /// no retry (safe under paused tokio).
    async fn reserve_and_serve(&mut self, idx: usize) -> Result<(), String> {
        if self.sandboxes[idx].served_by == Some(self.generation)
            || self.finalize_pending(idx)
            || self.terminally_destroyed(idx)
        {
            return Ok(());
        }
        let device = self.sandboxes[idx].nbd_device.clone();
        let pool = self.nbd_pool.clone();
        let Some(lease) = pool.claim(&device).await else {
            return Err(format!(
                "register rehydrate: claim of {} failed (not free?)",
                device.display()
            ));
        };
        // Recover the stable dirty file before the device is served.
        if self.sandboxes[idx].backend.is_none() {
            #[cfg(target_os = "linux")]
            self.rebuild(idx).await?;
            #[cfg(not(target_os = "linux"))]
            return Err("dirty-file recovery needs Linux extent semantics".to_string());
        }
        let g = self.generation;
        let s = &mut self.sandboxes[idx];
        s.lease = Some(lease);
        s.served_by = Some(g);
        s.kernel_owner = Some(g);
        Ok(())
    }

    /// The register-time rehydrate sequence (ADR 0098 P7): **coord-list pass →
    /// local ChainHeadRecord pass (#739) → stale-binding sweep**, driven over
    /// the pure verdicts. This is the callable sequence the 731df805 scenario
    /// pins.
    ///
    /// * `coord_includes_parked` — the ADVERSARIAL knob: the pre-#739 buggy
    ///   coordinator list filtered on `status='active'`, so it OMITTED
    ///   rung-parked (`evicting`) survivors. `false` replays that bug.
    /// * `local_pass_enabled` — whether the #739 defense-in-depth
    ///   ChainHeadRecord pass runs. `false` is the ungated variant (proving the
    ///   un-pause gate is the last line).
    pub async fn register_rehydrate(
        &mut self,
        coord_includes_parked: bool,
        local_pass_enabled: bool,
    ) -> Result<(), String> {
        let n = self.sandboxes.len();
        // 1. Coord-list pass: re-serve every LISTED survivor. The coord list is
        //    authoritative for what it contains (its listed survivors carry the
        //    coordinator's disk-manifest ref); a gap-A survivor is one the list
        //    OMITS (the `coord_includes_parked=false` knob), so this pass is NOT
        //    gated on the LOCAL record — only the #739 local pass below is.
        for idx in 0..n {
            if !self.is_resident_survivor(idx) {
                continue;
            }
            let listed = coord_includes_parked || !self.sandboxes[idx].parked;
            if listed {
                self.reserve_and_serve(idx).await?;
            }
        }
        // 2. Local ChainHeadRecord pass (#739 defense): re-serve any live
        //    survivor the coord list missed, via the pure candidate predicate
        //    (live ∧ unserved ∧ session-bound). A LOST record (gap A) leaves
        //    nothing for this pass to read either — the whole point of Layers
        //    2–3 is that the barrier catches what BOTH passes miss.
        if local_pass_enabled {
            for idx in 0..n {
                if !self.sandboxes[idx].record_present {
                    continue;
                }
                let live = self.is_resident_survivor(idx);
                let served = self.sandboxes[idx].served_by == Some(self.generation);
                if engram_host_core::is_local_survivor_candidate(live, served, true) {
                    self.reserve_and_serve(idx).await?;
                }
            }
        }
        // 3. The classification barrier: reconcile the kernel inventory against
        //    the records, then reap ONLY TerminalSafeToReap. A survivor invisible
        //    to both passes above (record lost, guest still live) is QUARANTINED
        //    here — classified, never skipped-or-severed.
        self.stale_sweep_tick();
        Ok(())
    }

    /// The Wave 7b (#784 layers 2–3) startup CLASSIFICATION over the REAL
    /// [`classify_startup_slots`](engram_host_core::classify_startup_slots):
    /// build a [`StartupSlot`](engram_host_core::StartupSlot) per sandbox device
    /// from the world state (owner liveness × the holder proof-of-death × whether
    /// a tracked record accounts for it) and partition into the four classes. The
    /// device handle is the slot INDEX. `has_record` mirrors prod: a device is
    /// recorded while a live guest still holds it AND a record is present
    /// (`record_present && guest_holds_device`) — a genuinely-gone guest's FC is
    /// not resident, so prod's `rootfs_device` reconcile can't map it, and it is
    /// reap-eligible on proof of death — OR while THIS generation
    /// quarantine-parked it (PR #828: the allocator's parked set is
    /// device-keyed, so a rehydrate-failed survivor stays tracked even after
    /// its FC entry vacates).
    pub fn classify_startup(&self) -> engram_host_core::StartupClassification<usize> {
        let gen = self.generation;
        let slots = self
            .sandboxes
            .iter()
            .enumerate()
            .map(|(idx, slot)| {
                let liveness = match slot.kernel_owner {
                    None => engram_host_core::PidLiveness::NoPid,
                    Some(g) if g == gen => engram_host_core::PidLiveness::SelfPid,
                    Some(_) => engram_host_core::PidLiveness::Dead,
                };
                // A resident guest still reading its rootfs is a live holder; a
                // genuinely-gone guest is NoHolder. The sim never produces
                // Unknown (no scan errors) — that fail-safe arm is pinned by the
                // pure-core unit test.
                let holder = if slot.guest_holds_device {
                    engram_host_core::DeviceHolder::LiveHolder
                } else {
                    engram_host_core::DeviceHolder::NoHolder
                };
                let has_record =
                    (slot.record_present && slot.guest_holds_device) || slot.quarantine_parked;
                engram_host_core::StartupSlot {
                    device: idx,
                    liveness,
                    holder,
                    has_record,
                }
            })
            .collect();
        engram_host_core::classify_startup_slots(slots)
    }

    /// The stale-binding sweep, now GATED by the startup classification barrier
    /// (ADR 0098 P7 + R6 + Wave 7b). Classify every device first, then reap ONLY
    /// the [`SlotClass::TerminalSafeToReap`](engram_host_core::SlotClass::TerminalSafeToReap)
    /// subset (`recover_stuck_nbd_devices` accepts nothing else — the ordering
    /// contract enforced by the [`ReapList`](engram_host_core::ReapList) type).
    ///
    /// A device THIS generation serves is `Serving` (self-owned) and never
    /// reaped — the 731df805 protection. A dead-owner device a live guest still
    /// holds is `ReconnectMe` (record) or `QuarantinedUnknown` (no record) —
    /// NEITHER is reaped, so a live guest's rootfs is never severed (#769 gap A).
    /// A `QuarantinedUnknown` device (a survivor invisible to the records) is
    /// recorded in [`quarantined_unknown`](SimHost::quarantined_unknown) + the
    /// counter — CLASSIFIED, never silently skipped.
    pub fn stale_sweep_tick(&mut self) {
        let classification = self.classify_startup();
        for &idx in &classification.quarantined {
            // Record the classification (oracle memory). Prod additionally fires
            // the `rehydrate-unknown-device` soft-invariant + counter; the sim's
            // observable is this set the quarantine oracles assert against.
            let id = self.sandboxes[idx].sandbox_id;
            self.quarantined_unknown.insert(id);
        }
        // reconnect devices are left kernel-bound (RECONNECTABLE) for a later
        // re-serve pass — never reaped. Only the terminal subset is DISCONNECTed.
        for idx in classification.reap.into_devices() {
            // NBD_CMD_DISCONNECT: proof of death met, no record — tear it down.
            self.sandboxes[idx].kernel_owner = None;
        }
    }

    /// Wave 7b (#784 layer 2): model the loss of a survivor's tracked records
    /// (its rehydrate ref missing from the coordinator list AND its durable
    /// `ChainHeadRecord` gone) — the #769 gap-A precondition. A resident guest
    /// still holds the device, but no record accounts for it, so the barrier
    /// must QUARANTINE (not skip, not sever) it. A no-op on an out-of-range
    /// index.
    pub fn lose_record(&mut self, idx: usize) {
        if let Some(slot) = self.sandboxes.get_mut(idx) {
            slot.record_present = false;
        }
    }

    /// PR #828: model THIS generation's rehydrate failing for sandbox `idx`'s
    /// device and parking it (`slot.quarantine()` → the allocator's parked
    /// set). The parked device stays a TRACKED record for the classification
    /// barrier even if every FC-derived record source subsequently vanishes —
    /// the 2026-07-21 incident: a concurrent sandbox destroy vacated
    /// `rootfs_device` between the park and the barrier, and the barrier fired
    /// the operator-alerting `rehydrate-unknown-device` invariant over a
    /// device this very process had parked on purpose. A no-op on an
    /// out-of-range index.
    pub fn quarantine_park(&mut self, idx: usize) {
        if let Some(slot) = self.sandboxes.get_mut(idx) {
            slot.quarantine_parked = true;
        }
    }

    /// The operator/runbook reconcile: a quarantined device's record is restored
    /// (the coordinator re-lists it / a ChainHeadRecord is re-written), so the
    /// next register pass re-serves the RECONNECTABLE device with zero loss — the
    /// recovery path the `rehydrate-unknown-device` alert drives.
    pub fn regain_record(&mut self, idx: usize) {
        if let Some(slot) = self.sandboxes.get_mut(idx) {
            slot.record_present = true;
        }
    }

    /// R6 (#769 gap A): model the FC guest process genuinely dying (crash /
    /// destroy), so it no longer holds its `/dev/nbdN` node open. After this a
    /// dead-owner sweep sees `NoHolder` and a DISCONNECT is legal — the
    /// un-pause gate's own coverage (a device whose holder is truly gone). A
    /// no-op on an out-of-range index.
    pub fn kill_guest(&mut self, idx: usize) {
        if let Some(slot) = self.sandboxes.get_mut(idx) {
            slot.guest_holds_device = false;
        }
    }

    /// Un-pause sandbox `idx` (rung-cancel resume). The **un-pause data-plane
    /// gate** (ADR 0098 P7): the guest is un-paused only when its rootfs device
    /// is served by THIS generation; otherwise the gate fires and the guest
    /// stays parked, routed to `evict_local → resume` — it NEVER lands on a
    /// dead data plane. Returns `true` if un-paused, `false` if the gate fired
    /// (or there was nothing to un-pause).
    pub fn unpause(&mut self, idx: usize) -> bool {
        let Some(slot) = self.sandboxes.get(idx) else {
            return false;
        };
        if !slot.parked {
            return false;
        }
        let served = slot.served_by == Some(self.generation);
        if engram_host_core::resume_data_plane_served(true, served) {
            self.sandboxes[idx].parked = false;
            true
        } else {
            // The gate fires — stay parked; the coordinator drives recovery.
            false
        }
    }

    /// The spare devices (`num_sandboxes..capacity`) the slot-allocator
    /// exercise uses. A `usize` ordinal maps into them.
    fn spare_device(&self, ordinal: usize) -> Option<std::path::PathBuf> {
        let n = self.sandboxes.len();
        let num_spares = (self.nbd_capacity as usize).saturating_sub(n);
        if num_spares == 0 {
            return None;
        }
        Some(Self::device_path(n + ordinal % num_spares))
    }

    /// Exercise the REAL slot allocator: `try_claim` a spare device (Free →
    /// Claimed), holding the lease. `try_claim` is single-shot (no retry), so
    /// it is hang-free under paused tokio and its synchronous reserved-bit
    /// protocol guarantees NO double-claim — a device a prior `SlotClaim`
    /// already holds returns `None` (benign). Held leases feed the
    /// slot-accounting oracle.
    pub async fn slot_claim(&mut self, ordinal: usize) {
        let Some(device) = self.spare_device(ordinal) else {
            return;
        };
        let pool = self.nbd_pool.clone();
        if let Some(lease) = pool.try_claim(&device).await {
            self.spare_leases.push(lease);
        }
    }

    /// Release the oldest held spare lease (Claimed → Free), exercising the
    /// allocator's `Drop`/`release` path. The async release is drained by the
    /// slot-accounting oracle's `yield_now` before it reads the pool counters,
    /// keeping the accounting deterministic.
    pub fn slot_populate_tick(&mut self) {
        // Drop the oldest spare so the populator/release path is exercised (the
        // freed slot returns to the pool for a future SlotClaim).
        if !self.spare_leases.is_empty() {
            let _ = self.spare_leases.remove(0);
        }
    }

    /// The count of slot leases this generation holds (per-sandbox served
    /// devices + spare leases) — the "claimed + parked" term of the
    /// slot-accounting identity `free + warm + held == capacity`.
    pub fn leases_held(&self) -> usize {
        let sandbox_leases = self.sandboxes.iter().filter(|s| s.lease.is_some()).count();
        sandbox_leases + self.spare_leases.len()
    }

    /// Restart the process and recover each surviving dirty file.
    #[cfg(target_os = "linux")]
    pub async fn restart(&mut self) -> Result<(), String> {
        // Flow D resume FIRST (the real startup order: `resume_pending_finalizes`
        // runs before serving traffic): re-drive every durable finalize record
        // through the REAL `load_all`, rebuilding the RAM idempotency map.
        self.resume_finalizes().await;
        for idx in 0..self.sandboxes.len() {
            // A sandbox mid-finalize stays paused (its acked writes live in the
            // durable staging files until the disk leg publishes them; the VM
            // is destroyed at the finalize's terminal — never rebuilt here).
            if self
                .pending_finalizes
                .contains_key(&self.sandboxes[idx].sandbox_id)
            {
                continue;
            }
            // #898: a terminally-finalized sandbox was DESTROYED — production
            // restart rehydrates only coordinator-provided survivors with a
            // resident rootfs, and this is neither. Rebuilding it here was
            // the resurrection channel that mis-surfaced the #897 finalize
            // bug through a recovery path production cannot take; the honest
            // detection path is [`Step::FinalizedResume`].
            if self.terminally_destroyed(idx) {
                continue;
            }
            if self.sandboxes[idx].backend.is_none() {
                self.rebuild(idx).await?;
            }
        }
        Ok(())
    }

    /// The `PooledBackend::resume_pending_finalizes` analog: reload every
    /// durable `EvictionFinalizeRecord` (REAL torn-tolerant `load_all`
    /// through the fs seam) into the RAM maps for `FinalizeTick` to re-drive.
    #[cfg(target_os = "linux")]
    async fn resume_finalizes(&mut self) {
        let finalizer = self.finalizer(self.effects.fs.clone());
        let records =
            EvictionFinalizeRecord::load_all(self.effects.fs.as_ref(), &finalizer.finalize_dir())
                .await;
        for record in records {
            let Some(idx) = self
                .sandboxes
                .iter()
                .position(|s| s.sandbox_id == record.sandbox_id)
            else {
                continue;
            };
            self.pending_finalizes
                .insert(record.sandbox_id, record.snapshot_id);
            self.finalize_started.insert(record.snapshot_id);
            self.in_flight.insert(idx, record);
        }
    }

    /// Rebuild one sandbox from its coordinator ref and dirty-file sidecar.
    #[cfg(target_os = "linux")]
    async fn rebuild(&mut self, idx: usize) -> Result<(), String> {
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        let dirty_path = dirty_file_path(self.fs.dirty_dir(), sandbox_id);
        let coordinator_ref = self.sandboxes[idx].rebuild_ref();
        let resolved_ref =
            resolve_recover_attach_ref(coordinator_ref, read_ref_sidecar(&dirty_path));
        let backend = build_backend(
            &self.store,
            self.fs.cache_dir(),
            self.fs.dirty_dir(),
            idx,
            sandbox_id,
            resolved_ref,
            DirtyFileOpenMode::Recover,
        )
        .await;
        self.sandboxes[idx].backend = Some(Arc::new(backend));
        Ok(())
    }

    /// The `/dev/nbdN`-shaped path a sandbox slot's device sync records
    /// against (the sim's `DeviceSync` seam is ordering-recorded, so any
    /// stable path suffices).
    fn device_path(idx: usize) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/dev/nbd{idx}"))
    }

    /// Build a REAL [`EvictionFinalizer`] over the sim's stores and the given
    /// fs handle (the prod TokioFs via `effects.fs`, or a [`CrashFs`] cut).
    /// `checkpoint_dir` is the tempdir root, so `finalize_dir()` / `records_dir()`
    /// resolve to the SimFs subtrees.
    fn finalizer(&self, fs: Arc<dyn HostFs>) -> EvictionFinalizer {
        EvictionFinalizer::new(
            Some((*self.store).clone()),
            None,
            self.fs.root().join("bundles"),
            ".bin",
            self.fs.root().to_path_buf(),
            self.pending_finalizes.clone(),
            self.destroyer.clone(),
            fs,
            SIM_FINALIZE_MAX_ATTEMPTS,
        )
    }

    /// Flow D entry (ADR 0098 P5): begin an eviction finalize for sandbox
    /// `idx` — the sim analog of `PooledBackend::snapshot_begin`. Drains the
    /// un-uploaded tier through the REAL `export_unflushed` →
    /// `persist_disk_pending_chunks` staging writer, persists the REAL
    /// `EvictionFinalizeRecord` through the fs seam (the durability
    /// boundary), and pauses the VM (backend drops — the guest is frozen for
    /// the whole finalize; the terminal leg destroys it).
    ///
    /// Idempotent like the real entry: a sandbox with a pending finalize
    /// re-observes the SAME snapshot id (the capture-lock lockout's
    /// observable contract — no second finalize, no concurrent chain
    /// mutation).
    pub async fn snapshot_begin(&mut self, idx: usize) -> Result<CaptureOutcome, String> {
        if idx >= self.sandboxes.len() {
            return Ok(CaptureOutcome::NotCapturable);
        }
        if self.sandboxes[idx].migrating {
            // The export holds the capture lock for its lifetime.
            return Ok(CaptureOutcome::NotCapturable);
        }
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        if let Some(existing) = self.pending_finalizes.get(&sandbox_id) {
            return Ok(CaptureOutcome::AlreadyPending(*existing));
        }
        // ADR 0098 G2 — the survivor-invisibility family's CAPTURE leg,
        // decided by the REAL pure verdict. `tracked` = a live backend
        // (the `nbd_sandboxes` entry analog: rebuild == rehydrate). An
        // untracked RESIDENT survivor (the VM is there, its disk server
        // was never rehydrated — the 03e6535e pre-condition) is REFUSED,
        // never silently skipped into a manifestless snapshot.
        let tracked = self.sandboxes[idx].backend.is_some();
        match engram_host_core::plan_capture_disk_drain(tracked, true, true) {
            engram_host_core::CaptureDrainPlan::Drain => {}
            engram_host_core::CaptureDrainPlan::RefuseUntracked => {
                return if self.is_resident_survivor(idx) {
                    Ok(CaptureOutcome::RefusedUntracked)
                } else {
                    Ok(CaptureOutcome::NotCapturable)
                };
            }
            engram_host_core::CaptureDrainPlan::NoNbdDisk => {
                unreachable!("every sim sandbox is NBD-backed on a data-plane host")
            }
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(CaptureOutcome::NotCapturable);
        };
        let snapshot_id = SnapshotId::from(self.entropy.uuid());
        let dest = self.fs.root().join("staging").join(snapshot_id.to_string());
        // Stage the FC snapshot artifacts (state.bin + sidecar). These are
        // FC's outputs in prod, not the flow's — seeding them with plain fs
        // is honest; the flow's own durable ops all cross the seam.
        tokio::fs::create_dir_all(&dest)
            .await
            .map_err(|e| format!("snapshot_begin staging dir: {e}"))?;
        tokio::fs::write(dest.join("state.bin"), b"sim fc state")
            .await
            .map_err(|e| format!("snapshot_begin state.bin: {e}"))?;
        tokio::fs::write(dest.join("manifest.json"), b"{}")
            .await
            .map_err(|e| format!("snapshot_begin manifest.json: {e}"))?;
        // The capture copies dirty-file content to durable staging.
        let (base_manifest, chunks) = backend.export_unflushed().await;
        let disk_chunks: Vec<(usize, ChunkHash, bytes::Bytes)> = chunks
            .iter()
            .map(|(i, b)| (*i, ChunkHash::of(b), bytes::Bytes::from(b.clone())))
            .collect();
        persist_disk_pending_chunks(&dest, &disk_chunks)
            .await
            .map_err(|e| format!("persist_disk_pending_chunks: {e}"))?;
        let now = self.effects.clock.now_utc();
        let record = EvictionFinalizeRecord {
            snapshot_id,
            session_id: self.sandboxes[idx].session_id,
            sandbox_id,
            image_version: "sim".to_string(),
            size_bytes: 0,
            paused_at: now,
            captured_at: now,
            dest,
            chain_prev_ref: None,
            disk_pending: Some(DiskPendingRecord {
                base_manifest,
                chunk_size: CHUNK_SIZE,
                total_bytes: NUM_CHUNKS * CHUNK_SIZE,
                chunks: disk_chunks.iter().map(|(i, h, _)| (*i, *h)).collect(),
            }),
            aux_bundles: Vec::new(),
            stage: FinalizeStage::Captured,
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        };
        let finalizer = self.finalizer(self.effects.fs.clone());
        record
            .persist(self.effects.fs.as_ref(), &finalizer.finalize_dir())
            .await
            .map_err(|e| format!("snapshot_begin record persist: {e}"))?;
        self.pending_finalizes.insert(sandbox_id, snapshot_id);
        self.in_flight.insert(idx, record);
        self.finalize_started.insert(snapshot_id);
        // The VM is paused for the whole finalize (and destroyed at its
        // terminal); the guest can no longer reach the data plane.
        self.sandboxes[idx].backend = None;
        Ok(CaptureOutcome::Began(snapshot_id))
    }

    /// The PRE-#743 capture on an untracked resident survivor — the G2
    /// seeds' bug reproduction, never driven by the swarm. The missing
    /// `nbd_sandboxes` entry was a SILENT skip: the snapshot records
    /// `disk_pending: None` (no drain — the acked disk writes are simply
    /// not in it) and the finalize completes with `disk_manifest=None`,
    /// poisoning the lineage the resume leg then boots.
    pub async fn snapshot_begin_pre743(&mut self, idx: usize) -> Result<SnapshotId, String> {
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        assert!(
            self.sandboxes[idx].backend.is_none() && self.is_resident_survivor(idx),
            "the pre-743 silent skip is only reachable for an untracked resident survivor",
        );
        let snapshot_id = SnapshotId::from(self.entropy.uuid());
        let dest = self.fs.root().join("staging").join(snapshot_id.to_string());
        tokio::fs::create_dir_all(&dest)
            .await
            .map_err(|e| format!("pre743 staging dir: {e}"))?;
        tokio::fs::write(dest.join("state.bin"), b"sim fc state")
            .await
            .map_err(|e| format!("pre743 state.bin: {e}"))?;
        tokio::fs::write(dest.join("manifest.json"), b"{}")
            .await
            .map_err(|e| format!("pre743 manifest.json: {e}"))?;
        let now = self.effects.clock.now_utc();
        let record = EvictionFinalizeRecord {
            snapshot_id,
            session_id: self.sandboxes[idx].session_id,
            sandbox_id,
            image_version: "sim".to_string(),
            size_bytes: 0,
            paused_at: now,
            captured_at: now,
            dest,
            chain_prev_ref: None,
            // THE BUG: no drain happened, so the record carries no disk
            // tier at all — indistinguishable from a legitimately
            // disk-less capture.
            disk_pending: None,
            aux_bundles: Vec::new(),
            stage: FinalizeStage::Captured,
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        };
        let finalizer = self.finalizer(self.effects.fs.clone());
        record
            .persist(self.effects.fs.as_ref(), &finalizer.finalize_dir())
            .await
            .map_err(|e| format!("pre743 record persist: {e}"))?;
        self.pending_finalizes.insert(sandbox_id, snapshot_id);
        self.in_flight.insert(idx, record);
        self.finalize_started.insert(snapshot_id);
        Ok(snapshot_id)
    }

    /// Resume a finalized (destroyed) sandbox from its snapshot — the G2
    /// RESUME leg, decided by the REAL `plan_resume_attach`. `gated` =
    /// the #743 guard active. A poisoned snapshot (no disk manifest — the
    /// sidecar still names the capture-time literal device) is REFUSED
    /// when gated; ungated it boots FC onto the stale literal, modeled as
    /// the base image's content — the acked-write oracle then catches the
    /// below-floor reads (the corruption #743 shipped).
    pub async fn resume_finalized(
        &mut self,
        idx: usize,
        gated: bool,
    ) -> Result<ResumeOutcome, String> {
        assert!(
            self.sandboxes[idx].backend.is_none() && !self.finalize_pending(idx),
            "resume targets a finalized (destroyed, non-pending) sandbox",
        );
        let poisoned = self.sandboxes[idx].poisoned_snapshot;
        let plan = engram_host_core::plan_resume_attach(!poisoned, true, poisoned);
        match plan {
            engram_host_core::ResumeAttachPlan::Attach => {
                // A resume starts a new sandbox from the finalized manifest.
                let backend = build_backend(
                    &self.store,
                    self.fs.cache_dir(),
                    self.fs.dirty_dir(),
                    idx,
                    self.sandboxes[idx].sandbox_id,
                    self.sandboxes[idx].rebuild_ref(),
                    DirtyFileOpenMode::Truncate,
                )
                .await;
                self.sandboxes[idx].backend = Some(Arc::new(backend));
                self.destroyer.forget(self.sandboxes[idx].sandbox_id);
                Ok(ResumeOutcome::Attached)
            }
            engram_host_core::ResumeAttachPlan::RefuseStaleLiteral if gated => {
                // The resume op requeues; the poisoned lineage surfaces
                // loudly. No boot, no corruption.
                Ok(ResumeOutcome::RefusedStaleLiteral)
            }
            engram_host_core::ResumeAttachPlan::RefuseStaleLiteral => {
                // PRE-#743: FC restores against the capture-time literal
                // /dev/nbdN — dead or foreign. The guest sees whatever
                // that device serves now: the base image's bytes, never
                // the session's acked writes.
                let backend = build_backend(
                    &self.store,
                    self.fs.cache_dir(),
                    self.fs.dirty_dir(),
                    idx,
                    self.sandboxes[idx].sandbox_id,
                    self.sandboxes[idx].base_ref,
                    DirtyFileOpenMode::Truncate,
                )
                .await;
                self.sandboxes[idx].backend = Some(Arc::new(backend));
                Ok(ResumeOutcome::BootedStaleLiteral)
            }
            engram_host_core::ResumeAttachPlan::Materialize => {
                unreachable!("every sim resume is on a data-plane host")
            }
        }
    }

    /// Swarm entry for [`Step::FinalizedResume`](crate::Step::FinalizedResume):
    /// no-op unless slot `idx` is a terminally-destroyed, non-pending finalize
    /// target — the only state the coordinator drives a post-eviction resume
    /// for. The swarm resumes GATED (the #743 guard on — safe default, like
    /// `RegisterRehydrate`); the adversarial ungated leg rides the G2 seeds.
    pub async fn finalized_resume(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len()
            || self.sandboxes[idx].backend.is_some()
            || self.finalize_pending(idx)
            || !self.terminally_destroyed(idx)
        {
            return Ok(());
        }
        self.resume_finalized(idx, /*gated=*/ true)
            .await
            .map(|_| ())
    }

    /// One finalize redrive attempt for slot `idx` — the REAL production
    /// loop body (`run_eviction_finalize_attempt`): a sleep-free pass over
    /// the legs + the retry/quarantine verdict. Backoff is modeled by the
    /// scheduler's `AdvanceTime`, never a literal sleep.
    pub async fn finalize_tick(&mut self, idx: usize) -> Result<(), String> {
        self.finalize_tick_with(idx, self.effects.fs.clone()).await
    }

    /// [`finalize_tick`](Self::finalize_tick) with an explicit fs handle —
    /// the [`FinalizeCrashAt`](crate::Step::FinalizeCrashAt) injector passes
    /// a [`CrashFs`] cut here.
    async fn finalize_tick_with(&mut self, idx: usize, fs: Arc<dyn HostFs>) -> Result<(), String> {
        let Some(mut record) = self.in_flight.remove(&idx) else {
            return Ok(());
        };
        let finalizer = self.finalizer(fs);
        match run_eviction_finalize_attempt(&finalizer, &mut record).await {
            FinalizeAttempt::Completed => {
                // The terminal leg published the disk manifest — the same
                // durability class as a flush publish: it raises the
                // published floor and becomes the durable rebuild pointer.
                // A completed finalize WITHOUT one (only reachable via the
                // pre-#743 silent-skip capture) is a poisoned lineage the
                // resume leg must reckon with.
                self.sandboxes[idx].poisoned_snapshot = record.disk_manifest.is_none();
                if let Some(published) = record.disk_manifest {
                    self.assert_finalize_covers_staging(idx, &record, published)
                        .await?;
                    self.sandboxes[idx].published_ref = Some(published);
                    self.mark_flush_published(idx, published).await?;
                }
            }
            FinalizeAttempt::Quarantined => {
                // The honest floor stays at the prior published tier; the
                // staging inputs are freed and nothing re-drives the record.
            }
            FinalizeAttempt::RetryAfter(_backoff) => {
                self.in_flight.insert(idx, record);
            }
        }
        Ok(())
    }

    /// ORACLE (#898 — the #897 laundering class made first-class): a
    /// COMPLETED finalize claims durability for every write it drained —
    /// the VM is destroyed and the staging deleted on the strength of the
    /// published manifest. Decode that manifest and require it to cover
    /// every staged chunk; a completed finalize whose publish omits one
    /// (e.g. a different-content store-ahead occupant accepted at the
    /// deterministic ref) rolled back an acked write. Loud at the moment
    /// of the lie — detection no longer depends on a later recovery leg
    /// (the old accidental resurrection channel) tripping over the gap.
    async fn assert_finalize_covers_staging(
        &self,
        idx: usize,
        record: &EvictionFinalizeRecord,
        published: ManifestRef,
    ) -> Result<(), String> {
        let Some(pending) = record.disk_pending.as_ref() else {
            return Ok(());
        };
        let manifest = self
            .store
            .get_manifest(published)
            .await
            .map_err(|e| format!("finalize-coverage get_manifest sandbox {idx}: {e}"))?;
        for (chunk_idx, hash) in &pending.chunks {
            let offset = (*chunk_idx as u64) * CHUNK_SIZE;
            let covered = manifest
                .chunks
                .iter()
                .any(|c| c.offset == offset && c.hash == *hash);
            if !covered {
                let tag = self
                    .store
                    .get_chunk(*hash)
                    .await
                    .map(|b| decode_tag(&b))
                    .unwrap_or(0);
                return Err(format!(
                    "sandbox {idx}: completed finalize published {published} but staged \
                     chunk {chunk_idx} tag {tag} is absent from it — the finalize's \
                     durability claim (VM destroyed, staging deleted) does not cover an \
                     acked write (the #897 deterministic-ref laundering class)"
                ));
            }
        }
        Ok(())
    }

    /// Flow D crash injection (ADR 0098 P5): drive one finalize attempt for
    /// slot `idx` under a [`CrashFs`] cut at fs-op index `op_index`, then the
    /// process dies (RAM drops, generation rolls). Ops before the cut ran for
    /// real, so the on-disk record/staging state is exactly what a death at
    /// that boundary leaves; the restart resume leg re-drives from it and
    /// oracle #6 asserts the stage never regresses.
    pub async fn finalize_crash_at(&mut self, idx: usize, op_index: usize) -> Result<(), String> {
        let crash_fs = CrashFs::with_crash_at(Some(op_index));
        self.finalize_tick_with(idx, crash_fs).await?;
        self.die_process();
        Ok(())
    }

    // ──────────────── Flow F: flush-pipeline interleavings (ADR 0098 P6) ──

    /// #204 as a seeded interleaving: park a REAL `flush()` at the
    /// dirty→pending handoff (both tier locks held), race a guest read AND
    /// a guest write of the drained chunk against it, then release. The
    /// racing read must decode a tag in the honest range (the drained
    /// content or the racing write — never pre-drain stale base), the
    /// racing write must survive to the ledger, and the standing oracle
    /// then holds. On the pre-#204 code the read serves stale base and the
    /// write RMWs from it, silently shadowing the drained bytes.
    pub async fn flush_handoff_race(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len()
            || self.finalize_pending(idx)
            || self.sandboxes[idx].migrating
        {
            return Ok(()); // the export's capture lock excludes flushes
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        // The drained-and-raced chunk is always chunk 0: write it (acked,
        // recorded) so the drain is guaranteed non-empty at that index.
        self.guest_write(idx, 0).await?;
        let drained_tag = self
            .ledger
            .latest_by_chunk()
            .get(&(idx, 0))
            .map(|e| e.content_tag)
            .expect("just wrote chunk 0");
        if backend.dirty_bytes().await == 0 {
            return Ok(()); // nothing to drain — the parked seam would never fire
        }

        let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::DirtyPendingHandoff);
        let flush_backend = backend.clone();
        let flush = tokio::spawn(async move { flush_backend.flush().await });
        arrived.notified().await;

        // The racing pair. The write's tag is minted BEFORE the spawn so
        // the id stream stays a pure function of the step sequence; its
        // ledger ack is recorded when the write returns (post-join).
        let racing_tag = self.next_tag();
        let read_backend = backend.clone();
        let read = tokio::spawn(async move { read_backend.read(0, CHUNK_SIZE).await });
        let write_backend = backend.clone();
        let write =
            tokio::spawn(async move { write_backend.write(0, &synth_chunk(racing_tag)).await });
        // Let the racers reach the held locks (single-threaded runtime:
        // one yield runs every ready task to its await point).
        tokio::task::yield_now().await;
        proceed.notify_one();

        let flush_outcome = flush
            .await
            .map_err(|e| format!("flush task join: {e}"))?
            .map_err(|e| format!("handoff-race flush sandbox {idx}: {e}"))?;
        let read_bytes = read
            .await
            .map_err(|e| format!("read task join: {e}"))?
            .map_err(|e| format!("handoff-race read sandbox {idx}: {e}"))?;
        write
            .await
            .map_err(|e| format!("write task join: {e}"))?
            .map_err(|e| format!("handoff-race write sandbox {idx}: {e}"))?;
        let _ = flush_outcome;
        // The racing write is now acked.
        let lineage = backend.manifest_ref().await;
        self.ledger.record(LedgerEntry {
            sandbox: idx,
            chunk_idx: 0,
            content_tag: racing_tag,
            lineage_at_ack: lineage,
        });
        // The racing read observed either the drained content or the
        // racing write — NEVER anything older (the #204 stale-base gap).
        let got = decode_tag(&read_bytes);
        if got != drained_tag && got != racing_tag {
            return Err(format!(
                "flush handoff race: read tag {got}, expected the drained                  {drained_tag} or the racing {racing_tag} — the #204                  tier-less stale-base gap"
            ));
        }
        self.note_flush_published(idx).await
    }

    /// #199's fence leg as a seeded interleaving: park a REAL `flush()`
    /// after its uploads but BEFORE the fence re-check + publish, raise
    /// the migration fence while parked, release — the flush must abort
    /// the publish (manifest not advanced, dirty re-queued), and after the
    /// fence drops a follow-up flush publishes the re-queued writes. The
    /// ledger floor never moves on the aborted attempt.
    pub async fn flush_fence_abort(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len()
            || self.finalize_pending(idx)
            || self.sandboxes[idx].migrating
        {
            return Ok(()); // the export's capture lock excludes flushes
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        self.guest_write(idx, 1).await?;
        if backend.dirty_bytes().await == 0 {
            return Ok(()); // an empty pipeline never reaches the parked seam
        }
        let before = backend.manifest_ref().await;

        let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::PostUploadPrePublish);
        let flush_backend = backend.clone();
        let flush = tokio::spawn(async move { flush_backend.flush().await });
        arrived.notified().await;
        // The #199 hazard: the fence rises while the flush is mid-pipeline.
        backend.set_migration_fence(true);
        proceed.notify_one();
        let outcome = flush
            .await
            .map_err(|e| format!("flush task join: {e}"))?
            .map_err(|e| format!("fence-abort flush sandbox {idx}: {e}"))?;
        if outcome.chunks_flushed != 0 || backend.manifest_ref().await != before {
            return Err(format!(
                "fence raised mid-pipeline did not abort the publish                  (chunks_flushed {}, manifest {} -> {}) — issue #199",
                outcome.chunks_flushed,
                before,
                backend.manifest_ref().await,
            ));
        }
        // Heal: drop the fence; the re-queued dirty tier publishes on the
        // next flush (driven here so the step leaves a converged sandbox).
        backend.set_migration_fence(false);
        backend
            .flush()
            .await
            .map_err(|e| format!("post-fence flush sandbox {idx}: {e}"))?;
        self.note_flush_published(idx).await
    }

    /// Park a flush after `put_manifest` and before rebase. Then kill the
    /// process. The dirty file keeps every acked write. The next flush also
    /// resolves the store version conflict.
    pub async fn flush_pre_rebase_crash(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len()
            || self.finalize_pending(idx)
            || self.sandboxes[idx].migrating
        {
            self.die_process();
            return Ok(());
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            self.die_process();
            return Ok(());
        };
        self.guest_write(idx, 2).await?;
        if backend.dirty_bytes().await == 0 {
            self.die_process();
            return Ok(()); // an empty pipeline never reaches the parked seam
        }
        let (arrived, proceed) = backend.arm_flush_seam(FlushSeamPoint::PreRebase);
        let flush_backend = backend.clone();
        let flush = tokio::spawn(async move { flush_backend.flush().await });
        arrived.notified().await;
        // The crash: the parked flush dies mid-instant. Abort is
        // deterministic here — the task is parked at the seam's Notify on
        // a single-threaded runtime, so it never resumes past the park.
        flush.abort();
        let _ = flush.await;
        drop(proceed);
        self.die_process();
        Ok(())
    }

    // ─────────────────────── Flow E: migration (ADR 0098 P8) ─────────────

    /// Open a migration export on sandbox `idx` — the sim analog of
    /// `migration_begin`: the guest freezes (the source becomes a page
    /// server) and a REAL [`MigrationExport`] lands in the REAL registry,
    /// its TTL clock the injected paused clock. Deterministic export id
    /// (entropy-minted — the prod OsRng id is a security nonce, which the
    /// sim must not launder into its replayable id stream). Returns false
    /// when nothing exportable (no backend / already exporting / frozen).
    pub async fn migration_begin(&mut self, idx: usize) -> Result<bool, String> {
        if idx >= self.sandboxes.len()
            || self.sandboxes[idx].migrating
            || self.sandboxes[idx].parked
            || self.finalize_pending(idx)
            || self.sandboxes[idx].backend.is_none()
        {
            return Ok(false);
        }
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        let export_id = format!(
            "{}{}",
            self.entropy.uuid().simple(),
            self.entropy.uuid().simple()
        );
        let snapshot_dir = self
            .fs
            .root()
            .join("staging")
            .join(format!("migration-{export_id}"));
        tokio::fs::create_dir_all(&snapshot_dir)
            .await
            .map_err(|e| format!("migration staging dir: {e}"))?;
        let clock: Arc<dyn engram_core::traits::Clock> = self.clock.clone();
        let guard = Arc::new(tokio::sync::Mutex::new(()))
            .try_lock_owned()
            .expect("fresh mutex");
        let inserted = self.migrations.insert(MigrationExport {
            export_id,
            sandbox_id,
            snapshot_dir,
            allowed_chunks: std::collections::HashSet::new(),
            disk_pending: None,
            disk_seal: None,
            clock,
            created_at: self.clock.now_mono(),
            post_copy: false,
            state_served: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: Arc::new(std::sync::Mutex::new(self.clock.now_mono())),
            capture_guard: guard,
        });
        if inserted {
            self.sandboxes[idx].migrating = true;
        }
        Ok(inserted)
    }

    /// `state.bin` leaves the host — the split-brain moment: from here the
    /// dest may be running this state and the source must NEVER
    /// self-resume ([`migration::ttl_verdict`]'s `state_served` arm).
    pub fn migration_serve_state(&mut self, idx: usize) {
        if idx >= self.sandboxes.len() {
            return;
        }
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        self.migrations.mark_state_served(sandbox_id);
    }

    /// A page/artifact serve — refreshes the REAL activity TTL anchor.
    pub fn migration_touch(&mut self, idx: usize) {
        if idx >= self.sandboxes.len() {
            return;
        }
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        self.migrations.touch(sandbox_id);
    }

    /// The dumb-host TTL sweep — the REAL `expired()` over the paused
    /// clock, the REAL `ttl_verdict` over the coordinator's (scriptable,
    /// adversarial) ownership answer, applied exactly as `lib.rs` does:
    /// AbortInPlace un-freezes the source (zero loss — the dirty tier
    /// never left the live backend), Destroy tears it down (ownership
    /// moved on), StayPaused retries next sweep. An abort-unpause applied
    /// to a `state_served` export is recorded as a split-brain (oracle #7
    /// — unreachable unless the decision table regresses).
    pub async fn migration_ttl_sweep(&mut self) -> Result<(), String> {
        for sandbox_id in self.migrations.expired() {
            let Some(idx) = self
                .sandboxes
                .iter()
                .position(|s| s.sandbox_id == sandbox_id)
            else {
                continue;
            };
            let session_id = self.sandboxes[idx].session_id;
            let ownership = self
                .effects
                .coord
                .sandbox_ownership(self.host_id, session_id, sandbox_id)
                .await
                .ok();
            let state_served = self.migrations.state_served(sandbox_id);
            match migration::ttl_verdict(true, ownership, state_served) {
                TtlVerdict::AbortInPlace => {
                    if state_served {
                        // The #216 split-brain: structurally recorded so
                        // oracle #7 fires. Unreachable unless ttl_verdict
                        // regresses.
                        self.split_brain_unpauses.push(sandbox_id);
                    }
                    self.migrations.remove(sandbox_id);
                    self.sandboxes[idx].migrating = false;
                }
                TtlVerdict::Destroy => {
                    self.migrations.remove(sandbox_id);
                    self.sandboxes[idx].migrating = false;
                    self.sandboxes[idx].backend = None;
                }
                TtlVerdict::StayPaused => {}
            }
        }
        Ok(())
    }

    /// Coordinator-driven commit: the dest owns the session now; the
    /// frozen source is torn down.
    pub fn migration_commit(&mut self, idx: usize) {
        if idx >= self.sandboxes.len() || !self.sandboxes[idx].migrating {
            return;
        }
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        self.migrations.remove(sandbox_id);
        self.sandboxes[idx].migrating = false;
        self.sandboxes[idx].backend = None;
    }

    /// Coordinator-driven EXPLICIT abort: the move never landed; the
    /// source un-freezes in place with every acked write intact. Legal
    /// even after `state.bin` shipped — the prod RPC documents that an
    /// explicit abort carries the coordinator's postcopy-never-loaded
    /// knowledge (ADR 0045 C2); the FORBIDDEN arm is only the dumb-host
    /// TTL self-resume, which `ttl_verdict` gates in the sweep. (The
    /// first CI run of the P9 host-sim lane caught the sim being
    /// STRICTER than prod here — calm seeds 14/16 fired oracle #7 on a
    /// legal explicit abort.)
    pub fn migration_abort(&mut self, idx: usize) {
        if idx >= self.sandboxes.len() || !self.sandboxes[idx].migrating {
            return;
        }
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        self.migrations.remove(sandbox_id);
        self.sandboxes[idx].migrating = false;
    }

    /// The common "process dies now" tail: RAM drops (backends, the
    /// in-flight finalize jobs, the pending-finalize idempotency map), the
    /// reconcile RAM dies, and the generation rolls.
    fn die_process(&mut self) {
        for slot in &mut self.sandboxes {
            slot.backend = None;
            // The export registry is RAM: the frozen source's export dies
            // with the process (the reattached-source story is
            // reattach_source_verdict's — pinned as a pure-fn seed).
            if slot.migrating {
                self.migrations.remove(slot.sandbox_id);
                slot.migrating = false;
            }
        }
        self.in_flight.clear();
        self.pending_finalizes.clear();
        self.reconcile.crash_ram();
        self.roll_generation();
    }
}

/// Build a fresh backend for sandbox `idx` at `manifest_ref`, over a private
/// cache subdir (deterministic, content-addressed).
async fn build_backend(
    store: &Arc<ChunkStore>,
    cache_root: &std::path::Path,
    dirty_root: &std::path::Path,
    idx: usize,
    sandbox_id: SandboxId,
    manifest_ref: ManifestRef,
    dirty_mode: DirtyFileOpenMode,
) -> ChunkedDiskBackend {
    let mut cfg = ChunkCacheConfig::new(cache_root.join(format!("sandbox-{idx}")));
    cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cfg);
    // `u64::MAX` threshold: the sim flushes explicitly (FlushTick), never via
    // the auto-threshold notify — no flush scheduler is installed.
    let backend = ChunkedDiskBackend::from_blob_with_dirty_file(
        manifest_ref,
        cache,
        store.clone(),
        u64::MAX,
        dirty_file_path(dirty_root, sandbox_id),
        dirty_mode,
    )
    .await
    .expect("build backend from base manifest");
    backend.retain_dirty_file().await;
    backend
}

fn dirty_file_path(dirty_root: &std::path::Path, sandbox_id: SandboxId) -> std::path::PathBuf {
    dirty_root.join(format!("{sandbox_id}.cache"))
}

/// The private base manifest: `NUM_CHUNKS` positional entries, all deduped
/// onto the single `base_hash` (tag-0 content).
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
