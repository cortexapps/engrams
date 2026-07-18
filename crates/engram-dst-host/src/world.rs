//! The simulated host world: the RAM/disk split and the acked-write ledger
//! (ADR 0098 Phase 2, P2).
//!
//! [`SimHost`] models one host-agent process. Its state is split exactly the
//! way a real crash splits it:
//!
//! * **RAM (dies on [`CrashProcess`](crate::Step::CrashProcess))** — the
//!   `Arc<ChunkedDiskBackend>` per sandbox. A backend's dirty tier is the
//!   in-RAM home of guest-acked-but-un-uploaded writes; the whole point of
//!   the 2026-07-16 session-85e0298a RCA is that this tier evaporates on a
//!   crash unless the shutdown spool captured it first.
//! * **Disk (survives)** — the per-run [`SimFs`] tempdir (records/finalize/
//!   spool/chunks/cache) and the shared [`ChunkStore`] over
//!   [`LocalBlobStorage`] (the GCS stand-in). Plus the durable per-sandbox
//!   pointers (`base_ref`, last-published `published_ref`) the coordinator
//!   would hold — modelled here as survivor state.
//! * **Oracle memory (survives)** — the [`AckedWriteLedger`]: one append per
//!   returned `write`, recording the `content_tag` and the manifest lineage
//!   the write was acked against. This is what the durability oracle
//!   ([`crate::invariants`]) replays against the rebuilt world after a crash.
//!
//! Guest writes are **content-tag-stamped synthetic chunks**: never real
//! block data, just `tag.to_le_bytes()` tiled across the chunk, so a read
//! decodes the tag back and the oracle can prove *which* acked write a
//! recovered chunk carries. `tag = 0` is the base (never-written) content.

use std::collections::{BTreeMap, BTreeSet};
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
use engram_host_agent::disk_daemon::backend::{ChunkedDiskBackend, FlushSeamPoint};
use engram_host_agent::disk_daemon::{spool, NbdSlot, NbdSlotAllocator};
use engram_host_agent::eviction_finalize::{
    persist_disk_pending_chunks, run_eviction_finalize_attempt, DiskPendingRecord,
    EvictionFinalizeRecord, EvictionFinalizer, EvictionSandbox, FinalizeAttempt,
};
use engram_host_agent::migration::{self, MigrationExport, MigrationRegistry, TtlVerdict};
use engram_host_core::{FinalizeStage, HostEffects, HostFs, LiveManifestPublishRequest};
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

/// The modeled virtual "cost" of the SIGTERM final-flush pass. A flush
/// deadline (from `plan_shutdown`) below this can't complete the pass, so it
/// overruns and every survivor is a straggler whose acked tier rides the
/// (complete, not deadline-bound) shutdown spool to the successor — the #225
/// deadline-overrun shape. Above it, the final flush completes and publishes.
/// Paused-tokio makes the literal `tokio::time::timeout` a no-op in the sim,
/// so the overrun is modeled at the extracted-plan level (the literal
/// per-survivor timeout race is Linux/FC-lane residue).
pub const SIM_FLUSH_COST: std::time::Duration = std::time::Duration::from_secs(1);

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
    /// The manifest lineage the write was acked against — recorded so a
    /// future oracle can reason about lineage-gated spool adoption.
    pub lineage_at_ack: ManifestRef,
}

/// The append-only acked-write ledger — the oracle's durable memory. It
/// survives a [`CrashProcess`](crate::Step::CrashProcess) (a real coordinator
/// / an external observer remembers what the guest was told); the RAM
/// backends do not.
///
/// It records two facts per chunk:
/// * the **latest acked tag** (`log`) — what the guest was last told is
///   written; and
/// * the **durable-handoff floor** (`published_floor`) — the tag of the most
///   recent write for that chunk that a flush PUBLISHED to the durable
///   (uploaded) tier, i.e. that survives ANY subsequent crash, including
///   abrupt process death.
///
/// The floor is the honest oracle's key (ADR 0098 P4.5). Two boundaries it
/// makes precise:
/// * A write acked from the RAM dirty tier but lost to abrupt process death
///   BEFORE it is published is an accepted, bounded loss (bounded by the flush
///   cadence + the periodic checkpoint, ADR 0028) — NOT a violation.
/// * A **shutdown-spool capture is a TRANSIENT handoff, not a floor.** The
///   spool preserves un-published writes across ONE orderly roll, but the
///   successor's `rebuild` adopts the spool back into the *volatile* dirty tier
///   and discards it — so the write is bounded by the next flush again. Baking
///   a chunk-bearing spool into a sticky floor would falsely demand recovery of
///   a write a later abrupt crash legitimately loses (the swarm found exactly
///   this once `AbruptCrash` was added). The published tier is the ONLY
///   permanent floor; spool RECOVERY is asserted by the dedicated regression
///   seeds that crash with a STANDING spool, not by the standing-state oracle.
///
/// The oracle therefore tolerates any read that is a MEMBER of this chunk's
/// acked-tag set (or the tag-0 base), bounded below by `published_floor`. It
/// flags a read OLDER than the published floor (a published write rolled back),
/// NEWER than the latest ack, or in-range but never acked for this chunk (a
/// misdirected read).
#[derive(Default)]
pub struct AckedWriteLedger {
    log: Vec<LedgerEntry>,
    /// The published-tier floor per `(sandbox, chunk_idx)`: the highest tag a
    /// flush has published to the durable/uploaded tier. Observed from the REAL
    /// published manifest (never optimistically flagged), monotonic per chunk
    /// (the published lineage only advances), and NEVER lowered — the published
    /// tier is permanently durable. A later un-published write mints a new tag
    /// that starts BELOW the acked value but does not move the floor until it is
    /// itself flushed.
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

    /// The durable-handoff floor for a chunk: the highest tag a flush published,
    /// or `None` if no write to this chunk was ever published (its acked writes
    /// are all still RAM-only or spool-transient, droppable by abrupt death).
    pub fn handed_off_tag(&self, sandbox: usize, chunk_idx: u64) -> Option<u64> {
        self.published_floor.get(&(sandbox, chunk_idx)).copied()
    }

    /// Every tag ever acked for `(sandbox, chunk_idx)` — the membership set
    /// the oracle checks reads against (a read must be one of THESE, never
    /// merely a numerically in-range tag minted for another chunk).
    pub fn acked_tags(&self, sandbox: usize, chunk_idx: u64) -> BTreeSet<u64> {
        self.log
            .iter()
            .filter(|e| e.sandbox == sandbox && e.chunk_idx == chunk_idx)
            .map(|e| e.content_tag)
            .collect()
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

/// One sandbox's slot: the durable pointers survive; `backend` is the RAM
/// tier that dies on crash.
pub struct SandboxSlot {
    pub sandbox_id: SandboxId,
    pub session_id: SessionId,
    /// The private base manifest the sandbox was created on (in the store).
    pub base_ref: ManifestRef,
    /// The last flush-published manifest ref (durable survivor pointer). The
    /// successor rebuilds `from_blob` at this (or `base_ref` if never
    /// flushed).
    pub published_ref: Option<ManifestRef>,
    /// The live backend — `None` after a crash, until a restart/adopt
    /// rebuilds it.
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
            let backend = build_backend(&store, fs.cache_dir(), i, base_ref).await;
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
    /// terminal) — the sim mirrors it by gating spool adoption and the
    /// register-time re-serve on this (the swarm found the resurrection:
    /// a re-serve rebuilt a captured sandbox from the OLD published ref,
    /// then the finalize published a NEWER floor over it).
    fn finalize_pending(&self, idx: usize) -> bool {
        self.pending_finalizes
            .contains_key(&self.sandboxes[idx].sandbox_id)
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

    /// Read a chunk back and, if the ledger has an acked tag for it, assert the
    /// HONEST read property inline (the oracle re-checks every acked chunk each
    /// step, but this gives a targeted read trace).
    ///
    /// A legitimate read-back is a tag actually acked for this chunk (or the
    /// tag-0 base), bounded below by the published floor: the live newest
    /// write, the permanent published floor (a recovery that dropped newer,
    /// un-published writes — the accepted, bounded loss of ADR 0098 P4.5), or a
    /// transiently-durable intermediate a standing spool adopted. A read older
    /// than the published floor (a rolled-back durable write), newer than the
    /// latest ack, or in-range but never acked for this chunk (misdirection) is
    /// a durability-pipeline violation.
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
            // `0` = base content (never published).
            let floor = self.ledger.handed_off_tag(idx, chunk_idx).unwrap_or(0);
            let acked = self.ledger.acked_tags(idx, chunk_idx);
            let member = got == 0 || acked.contains(&got);
            if !member || got < floor {
                let why = if got < floor {
                    "a lost or rolled-back durable write"
                } else if got > latest {
                    "a read newer than the latest ack (a never-acked tag)"
                } else {
                    "an in-range tag never acked for this chunk (misdirection)"
                };
                return Err(format!(
                    "read-after-write: sandbox {idx} chunk {chunk_idx} read tag {got} not a \
                     member of this chunk's acked set within [published_floor {floor}, \
                     latest_ack {latest}] — {why}"
                ));
            }
        }
        Ok(())
    }

    /// A full flush: drain+upload dirty chunks, tick the manifest version,
    /// advance `published_ref`, publish to the coordinator, and discard any
    /// now-superseded spool. No-op on a crashed backend.
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

    /// The post-flush bookkeeping every flush-shaped step shares: adopt the
    /// backend's (possibly advanced) manifest as the durable pointer,
    /// observe the REAL published chunk set into the ledger floor, discard
    /// the now-superseded spool, and tell the coordinator. Idempotent when
    /// the flush was a no-op/abort (the manifest didn't move; re-marking
    /// the same manifest is monotonic).
    pub async fn note_flush_published(&mut self, idx: usize) -> Result<(), String> {
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        let published = backend.manifest_ref().await;
        self.sandboxes[idx].published_ref = Some(published);
        // Durable handoff: observe the REAL published chunk set (read the
        // manifest + its chunks back out of the store) and record each
        // chunk's now-durable tag as the handoff floor — not an optimistic
        // flag, but what actually landed in the durable tier.
        self.mark_flush_published(idx, published).await?;
        // The flush uploaded the current dirty tier; any spool predates it
        // and is now superseded.
        spool::discard_spool(
            self.effects.fs.as_ref(),
            self.fs.spool_dir(),
            self.sandboxes[idx].sandbox_id,
        )
        .await
        .map_err(|e| format!("discard_spool sandbox {idx}: {e}"))?;
        // Tell the coordinator the survivor's disk moved (Flow A/D).
        let req = LiveManifestPublishRequest {
            session_id: self.sandboxes[idx].session_id,
            sandbox_id: self.sandboxes[idx].sandbox_id,
            manifest_id: published.manifest_id,
            manifest_version: published.version,
        };
        self.effects
            .coord
            .publish_live_manifest(self.host_id, &req)
            .await
            .map_err(|e| format!("publish sandbox {idx}: {e}"))?;
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

    /// Write the shutdown spool for a live sandbox: export the un-uploaded
    /// tier (dirty ∪ drained-pending) and durably spool it under the
    /// sandbox's dir. Models the predecessor's SIGTERM spool leg.
    ///
    /// Note it does NOT raise the published floor: a chunk-bearing spool is a
    /// TRANSIENT handoff (the successor adopts it back into the volatile dirty
    /// tier and discards it — see [`AckedWriteLedger`]). Only a flush that
    /// publishes to the durable tier moves the floor.
    pub async fn spool_export(&self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len() {
            return Ok(());
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        let (exported_ref, chunks) = backend.export_unflushed().await;
        spool::write_spool(
            self.effects.fs.as_ref(),
            self.fs.spool_dir(),
            self.sandboxes[idx].sandbox_id,
            exported_ref,
            &chunks,
        )
        .await
        .map_err(|e| format!("write_spool sandbox {idx}: {e}"))?;
        Ok(())
    }

    /// Successor spool adoption. Race-free self-handoff (#721): first capture
    /// the CURRENT live tier into the spool (so no un-exported acked write is
    /// dropped), then rebuild a fresh backend at the durable ref and adopt
    /// the spool into it — exactly the successor path, driven against the
    /// same host.
    pub async fn spool_adopt(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len() || self.finalize_pending(idx) {
            return Ok(());
        }
        // Capture the live tier first — losslessness gate.
        if self.sandboxes[idx].backend.is_some() {
            self.spool_export(idx).await?;
        }
        self.rebuild(idx).await
    }

    /// An orderly process crash: run the shutdown spool for every live
    /// sandbox (the shipped #712 machinery — spool the un-uploaded tier
    /// before dying), THEN drop all RAM backends. P2 always completes the
    /// spool (benign); P4's crash injector will cut at the [`CrashPoint`]s
    /// BEFORE/DURING it, which is what makes the oracle discriminating.
    ///
    /// [`CrashPoint`]: crate::CrashPoint
    pub async fn crash_process(&mut self) -> Result<(), String> {
        for idx in 0..self.sandboxes.len() {
            if self.sandboxes[idx].backend.is_some() {
                self.spool_export(idx).await?;
            }
        }
        // Flow C: the pooled/capture in-RAM tables (local bindings, migration
        // roles, live captures) die with the process; the live FC set and the
        // coordinator (a separate process) survive. The successor's reconcile
        // loop rebuilds the bindings from the coordinator — the None-arm path.
        // Flow B: a process death is a ROLL. Flow D: the in-flight finalize
        // jobs + the pending map die; the durable records survive for resume.
        self.die_abruptly();
        Ok(())
    }

    /// ABRUPT process death (SIGKILL / power loss / OOM) — ADR 0098 P4.5. Drop
    /// every RAM backend WITHOUT running the shutdown spool: the in-RAM dirty
    /// tier (guest-acked-but-un-flushed-un-spooled writes) evaporates. This is
    /// the **accepted, bounded loss** window — bounded by the flush cadence +
    /// the periodic checkpoint (ADR 0028), NOT a durability violation. The
    /// honest oracle keys on the durable-handoff floor, so a following
    /// `Restart` legitimately rolls an un-handed-off acked write back to its
    /// last flushed/spooled tag (or base) and no false "recovered" is claimed.
    ///
    /// The contrast with [`crash_process`](Self::crash_process): that models an
    /// ORDERLY shutdown that completes the spool first (every acked write handed
    /// off, so all recover). Only this variant exercises the post-ack /
    /// pre-handoff window the pipeline is explicitly permitted to lose.
    pub async fn abrupt_crash(&mut self) -> Result<(), String> {
        // RAM dies — NO shutdown spool. Flow B: an abrupt death is still a
        // ROLL (a fresh generation reattaches the surviving FC VMs / durable
        // disk).
        self.die_abruptly();
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
            // parked persist.
            slot.lease = None;
            slot.served_by = None;
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
        if self.sandboxes[idx].served_by == Some(self.generation) || self.finalize_pending(idx) {
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
        // Rebuild the backend from the durable ref + adopt any spool (the
        // seed-before-RECONFIGURE recovery leg). This is the recovery ARM the
        // local-rehydrate pass adds to oracle #1's closure.
        if self.sandboxes[idx].backend.is_none() {
            self.rebuild(idx).await?;
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
        // 1. Coord-list pass: re-serve every LISTED survivor. The buggy list
        //    omits parked survivors.
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
        //    (live ∧ unserved ∧ session-bound). Every sim sandbox's record
        //    carries a bound session.
        if local_pass_enabled {
            for idx in 0..n {
                let live = self.is_resident_survivor(idx);
                let served = self.sandboxes[idx].served_by == Some(self.generation);
                if engram_host_core::is_local_survivor_candidate(live, served, true) {
                    self.reserve_and_serve(idx).await?;
                }
            }
        }
        // 3. Stale-binding sweep.
        self.stale_sweep_tick();
        Ok(())
    }

    /// The stale-binding sweep (ADR 0098 P7): DISCONNECT devices whose recorded
    /// owner is a genuinely-dead generation, driven over the pure
    /// [`sweep_verdict`](engram_host_core::sweep_verdict). Only devices FREE in
    /// the pool are reached — the `served_by == current` (claimed) gate mirrors
    /// the driver's `try_claim` free-in-pool gate, so a device THIS generation
    /// serves (a re-served survivor) is NEVER swept (the 731df805 protection).
    pub fn stale_sweep_tick(&mut self) {
        let gen = self.generation;
        for slot in &mut self.sandboxes {
            // Served this generation ⇒ claimed ⇒ not free in the pool ⇒ the
            // sweep skips it (try_claim would return None).
            if slot.served_by == Some(gen) {
                continue;
            }
            let liveness = match slot.kernel_owner {
                None => engram_host_core::PidLiveness::NoPid,
                Some(g) if g == gen => engram_host_core::PidLiveness::SelfPid,
                // An older generation is a dead process.
                Some(_) => engram_host_core::PidLiveness::Dead,
            };
            if matches!(
                engram_host_core::sweep_verdict(liveness),
                engram_host_core::SweepAction::Disconnect
            ) {
                // NBD_CMD_DISCONNECT: the kernel binding is torn down.
                slot.kernel_owner = None;
            }
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

    /// Restart the process: bring up the sandboxes whose backend DIED — for
    /// each, rebuild over the surviving store/tempdir (`from_blob` at the last
    /// published/base ref, then adopt the shutdown spool). Only the PORTABLE
    /// recovery legs; PooledBackend/reattach_pass recovery is out of P2 scope.
    ///
    /// A restart brings up only what is down: a sandbox whose backend is still
    /// LIVE is left untouched (rebuilding it would drop its in-RAM dirty tier —
    /// un-flushed, un-spooled acked writes — which a real restart, running only
    /// after the process actually died, never does).
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
            if self.sandboxes[idx].backend.is_none() {
                self.rebuild(idx).await?;
            }
        }
        Ok(())
    }

    /// The `PooledBackend::resume_pending_finalizes` analog: reload every
    /// durable `EvictionFinalizeRecord` (REAL torn-tolerant `load_all`
    /// through the fs seam) into the RAM maps for `FinalizeTick` to re-drive.
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

    /// Rebuild one sandbox's backend from the surviving disk. Reads the spool
    /// FIRST so two real recovery rules can pick the rebuild ref and adoption:
    ///
    /// * **Store-ahead (85e0298a).** A same-lineage spool ref that is AHEAD of
    ///   the durable / coord pointer means the final flush uploaded a manifest
    ///   the coordinator never acked (its publish was lost), so the successor
    ///   MUST attach from the spool's ref — never roll back to coord's stale
    ///   one. Mirrors `abandon_nbd_data_planes_for_shutdown`'s store-ahead
    ///   comment + the spool's zero-chunk ref-only leg.
    /// * **Tolerant rejection (H5).** A torn/spliced spool (`read_spool`
    ///   `Err`) is a LOUD rollback at the real call site: discard it and
    ///   rebuild from the durable pointer, NEVER adopt corrupt bytes (the
    ///   spool's hard digest gate). The acked writes must then be covered by
    ///   the published tier — the crash-injection contract that keeps the
    ///   oracle unconditional. If they are NOT (a torn ONLY-copy spool, the
    ///   documented residual-loss window), the oracle catches the loss.
    ///
    /// The lineage gate mirrors the real call site: a spool from a DIFFERENT
    /// manifest lineage is a loud discard, never an adopt.
    async fn rebuild(&mut self, idx: usize) -> Result<(), String> {
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        // Read the spool BEFORE picking the rebuild ref (store-ahead rule).
        let spool = match spool::read_spool(
            self.effects.fs.as_ref(),
            self.fs.spool_dir(),
            sandbox_id,
        )
        .await
        {
            Ok(s) => s,
            Err(_torn) => {
                // Torn/spliced → discard + rebuild from the durable pointer.
                spool::discard_spool(self.effects.fs.as_ref(), self.fs.spool_dir(), sandbox_id)
                    .await
                    .map_err(|e| format!("discard_spool sandbox {idx}: {e}"))?;
                None
            }
        };
        let mut rebuild_ref = self.sandboxes[idx].rebuild_ref();
        if let Some((meta, _)) = &spool {
            let spool_ref = meta.manifest_ref();
            if spool_ref.manifest_id == rebuild_ref.manifest_id
                && spool_ref.version > rebuild_ref.version
            {
                // Store-ahead: attach from the spool's ref, adopt it as the
                // durable pointer (coord's publish was lost).
                rebuild_ref = spool_ref;
                self.sandboxes[idx].published_ref = Some(spool_ref);
            }
        }
        let backend = build_backend(&self.store, self.fs.cache_dir(), idx, rebuild_ref).await;
        if let Some((meta, chunks)) = spool {
            // The REAL call site's lineage gate (`rehydrate_sandbox`): adopt
            // only a same-lineage spool at-or-ahead-of the durable pointer
            // (`meta.version >= disk_manifest.version`); a STALE spool (an
            // older divergence than the durable tier — e.g. an eviction
            // finalize published past it) is a loud discard, never an adopt
            // of old bytes over newer durable state. The swarm found the sim
            // missing the version half of this gate once Flow D could
            // advance the durable pointer past a standing spool.
            let spool_ref = meta.manifest_ref();
            if spool_ref.manifest_id == rebuild_ref.manifest_id
                && spool_ref.version >= rebuild_ref.version
            {
                backend.adopt_unflushed(chunks).await;
            }
            // Adopted or stale: the spool is consumed either way.
            spool::discard_spool(self.effects.fs.as_ref(), self.fs.spool_dir(), sandbox_id)
                .await
                .map_err(|e| format!("discard_spool sandbox {idx}: {e}"))?;
        }
        self.sandboxes[idx].backend = Some(Arc::new(backend));
        Ok(())
    }

    /// The `/dev/nbdN`-shaped path a sandbox slot's device sync records
    /// against (the sim's `DeviceSync` seam is ordering-recorded, so any
    /// stable path suffices).
    fn device_path(idx: usize) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/dev/nbd{idx}"))
    }

    /// Drive the REAL extracted SIGTERM ladder (ADR 0098 P4, Flow A) over the
    /// sim host. `env_value` is the seeded `ENGRAM_SHUTDOWN_FLUSH_BUDGET_SECS`
    /// (`None` = env unset). The pure
    /// [`plan_shutdown`](engram_host_core::plan_shutdown) decides the deadline;
    /// a deadline below [`SIM_FLUSH_COST`] OVERRUNS → the final-flush leg is
    /// skipped and every survivor's dirty tier rides the (complete) spool
    /// (#225). The abandon + spool-export leg always completes; then RAM dies
    /// (the process exits; the successor reattaches). A following `Restart` /
    /// `SpoolAdopt` recovers every acked write — oracle #1 holds either way.
    pub async fn sigterm(&mut self, env_value: Option<f64>) -> Result<(), String> {
        let plan = engram_host_core::plan_shutdown(env_value);
        let overrun = plan.flush_deadline < SIM_FLUSH_COST;
        for idx in 0..self.sandboxes.len() {
            let Some(backend) = self.sandboxes[idx].backend.clone() else {
                continue;
            };
            // FinalFlush stage: force the host page cache down (seam-recorded),
            // then — unless the deadline overran — flush + classify + publish.
            self.effects
                .device
                .sync_device(&Self::device_path(idx))
                .await
                .map_err(|e| format!("sigterm device sync sandbox {idx}: {e}"))?;
            if !overrun {
                let outcome = backend
                    .flush()
                    .await
                    .map_err(|e| format!("sigterm flush sandbox {idx}: {e}"))?;
                let action =
                    engram_host_core::classify_survivor(engram_host_core::FlushProbe::Flushed {
                        chunks_flushed: outcome.chunks_flushed,
                        bound: true,
                    });
                if matches!(action, engram_host_core::SurvivorAction::Publish) {
                    let published = backend.manifest_ref().await;
                    self.sandboxes[idx].published_ref = Some(published);
                    // Durable handoff via the published tier (the final-flush
                    // leg completed, so these chunks are durable independent of
                    // the spool that follows).
                    self.mark_flush_published(idx, published).await?;
                    let req = LiveManifestPublishRequest {
                        session_id: self.sandboxes[idx].session_id,
                        sandbox_id: self.sandboxes[idx].sandbox_id,
                        manifest_id: published.manifest_id,
                        manifest_version: published.version,
                    };
                    self.effects
                        .coord
                        .publish_live_manifest(self.host_id, &req)
                        .await
                        .map_err(|e| format!("sigterm publish sandbox {idx}: {e}"))?;
                }
            }
            // Abandon + SpoolExport stage: always export the (still-dirty, if
            // overrun) tier to the shutdown spool — not deadline-bound.
            self.spool_export(idx).await?;
        }
        // Detached stage: RAM dies. Flow B: SIGTERM is a roll (the successor
        // pidfd-reattaches).
        self.die_abruptly();
        Ok(())
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
        // The capture disk drain: the un-uploaded tier moves from RAM to the
        // node-durable disk-pending staging files via the REAL writer.
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
                self.rebuild(idx).await?;
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
                    idx,
                    self.sandboxes[idx].base_ref,
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

    /// Flow D crash injection (ADR 0098 P5): drive one finalize attempt for
    /// slot `idx` under a [`CrashFs`] cut at fs-op index `op_index`, then the
    /// process dies (RAM drops, generation rolls). Ops before the cut ran for
    /// real, so the on-disk record/staging state is exactly what a death at
    /// that boundary leaves; the restart resume leg re-drives from it and
    /// oracle #6 asserts the stage never regresses.
    pub async fn finalize_crash_at(&mut self, idx: usize, op_index: usize) -> Result<(), String> {
        let crash_fs = CrashFs::with_crash_at(Some(op_index));
        self.finalize_tick_with(idx, crash_fs).await?;
        self.die_abruptly();
        Ok(())
    }

    /// Spool crash injection (ADR 0098 P5, replacing P4's post-hoc mangle):
    /// the predecessor's spool WRITE is cut at fs-op index `op_index` by the
    /// real seam. Shape mirrors the P4 scenario: the acked tier is first
    /// exported to a complete spool AND flush-published (redundant
    /// durability, floor raised), then the spool is RE-written through a
    /// [`CrashFs`] cut — leaving, per `op_index`: the intact prior spool
    /// (cut before its `remove_dir`), no spool, a marker-less partial, or a
    /// complete one. Every state must recover EVERY acked write via the
    /// published tier + the tolerant `read_spool` (adoption, when it
    /// happens, is tag-identical to the published set). Then the process
    /// dies.
    pub async fn spool_crash_at(&mut self, op_index: usize) -> Result<(), String> {
        // Guarantee a chunk-bearing spool exists to cut.
        if let Some(backend) = self.sandboxes[0].backend.clone() {
            if backend.dirty_bytes().await == 0 {
                self.guest_write(0, 0).await?;
            }
        }
        let Some(backend) = self.sandboxes[0].backend.clone() else {
            self.die_abruptly();
            return Ok(());
        };
        let sandbox_id = self.sandboxes[0].sandbox_id;
        // The pre-crash content: export the dirty set, spool it whole, then
        // flush-publish the SAME set (floor raised; the spool is redundant).
        let (exported_ref, chunks) = backend.export_unflushed().await;
        spool::write_spool(
            self.effects.fs.as_ref(),
            self.fs.spool_dir(),
            sandbox_id,
            exported_ref,
            &chunks,
        )
        .await
        .map_err(|e| format!("spool_crash_at baseline spool: {e}"))?;
        backend
            .flush()
            .await
            .map_err(|e| format!("spool_crash_at flush: {e}"))?;
        let published = backend.manifest_ref().await;
        self.sandboxes[0].published_ref = Some(published);
        self.mark_flush_published(0, published).await?;
        // The cut re-write: the process dies at fs-op `op_index` of a real
        // `write_spool` sequence. An error IS the crash landing.
        let crash_fs = CrashFs::with_crash_at(Some(op_index));
        let _ = spool::write_spool(
            crash_fs.as_ref(),
            self.fs.spool_dir(),
            sandbox_id,
            exported_ref,
            &chunks,
        )
        .await;
        self.die_abruptly();
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

    /// The pre-rebase store-ahead crash window as a seeded interleaving:
    /// park a REAL `flush()` AFTER `put_manifest` succeeded but BEFORE the
    /// rebase, then the process dies (the parked task is aborted — its
    /// held locks drop with it). The store now holds a manifest nothing
    /// references (85e0298a store-ahead); the durable pointer never
    /// advanced and the ledger floor never rose, so the loss is HONEST,
    /// and the successor's next flush recovers through the REAL
    /// version-conflict retry (attempts the stale next-version, hits the
    /// conflict, re-targets latest+1).
    pub async fn flush_pre_rebase_crash(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len()
            || self.finalize_pending(idx)
            || self.sandboxes[idx].migrating
        {
            self.die_abruptly();
            return Ok(());
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            self.die_abruptly();
            return Ok(());
        };
        self.guest_write(idx, 2).await?;
        if backend.dirty_bytes().await == 0 {
            self.die_abruptly();
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
        self.die_abruptly();
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
    fn die_abruptly(&mut self) {
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
    idx: usize,
    manifest_ref: ManifestRef,
) -> ChunkedDiskBackend {
    let mut cfg = ChunkCacheConfig::new(cache_root.join(format!("sandbox-{idx}")));
    cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cfg);
    // `u64::MAX` threshold: the sim flushes explicitly (FlushTick), never via
    // the auto-threshold notify — no flush scheduler is installed.
    ChunkedDiskBackend::from_blob(manifest_ref, cache, store.clone(), u64::MAX)
        .await
        .expect("build backend from base manifest")
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
