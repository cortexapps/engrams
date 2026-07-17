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

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::manifest::{
    ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
};
use engram_chunk_store::store::ChunkStore;
use engram_core::traits::{BlobStorage, Entropy as _};
use engram_core::types::manifest::ManifestRef;
use engram_core::{HostId, SandboxId, SessionId};
use engram_host_agent::disk_daemon::backend::ChunkedDiskBackend;
use engram_host_agent::disk_daemon::{spool, NbdSlot, NbdSlotAllocator};
use engram_host_core::{HostEffects, LiveManifestPublishRequest};
use engram_sim::{SimClock, SimEntropy};
use engram_storage_local::LocalBlobStorage;

use crate::coord_stub::SimCoordClient;
use crate::crash_state;
use crate::effects::{sim_effects, SeamLog};
use crate::reconcile::SimReconcileBackend;
use crate::simfs::{CrashPoint, SimFs};

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
/// The oracle therefore tolerates any read in `[published_floor, latest_ack]`
/// (by tag order) and flags only a read OLDER than the published floor (a
/// published write rolled back) or NEWER than the latest ack.
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
}

impl SandboxSlot {
    /// The ref a successor rebuilds from: the last publish, else the base.
    pub fn rebuild_ref(&self) -> ManifestRef {
        self.published_ref.unwrap_or(self.base_ref)
    }
}

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
    /// A legitimate read-back falls in the honest range
    /// `[published_floor, latest_ack]` (by tag order): the live newest write,
    /// the permanent published floor (a recovery that dropped newer,
    /// un-published writes — the accepted, bounded loss of ADR 0098 P4.5), or a
    /// transiently-durable intermediate a standing spool adopted. A read older
    /// than the published floor (a rolled-back durable write) or newer than the
    /// latest ack is a durability-pipeline violation.
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
            if got < floor || got > latest {
                return Err(format!(
                    "read-after-write: sandbox {idx} chunk {chunk_idx} read tag {got} outside \
                     [published_floor {floor}, latest_ack {latest}] — a lost or rolled-back \
                     durable write"
                ));
            }
        }
        Ok(())
    }

    /// A full flush: drain+upload dirty chunks, tick the manifest version,
    /// advance `published_ref`, publish to the coordinator, and discard any
    /// now-superseded spool. No-op on a crashed backend.
    pub async fn flush_tick(&mut self, idx: usize) -> Result<(), String> {
        if idx >= self.sandboxes.len() {
            return Ok(());
        }
        let Some(backend) = self.sandboxes[idx].backend.clone() else {
            return Ok(());
        };
        backend
            .flush()
            .await
            .map_err(|e| format!("flush sandbox {idx}: {e}"))?;
        let published = backend.manifest_ref().await;
        self.sandboxes[idx].published_ref = Some(published);
        // Durable handoff: the flush published a new manifest. Observe the REAL
        // published chunk set (read the manifest + its chunks back out of the
        // store) and record each chunk's now-durable tag as the handoff floor —
        // not an optimistic flag, but what actually landed in the durable tier.
        self.mark_flush_published(idx, published).await?;
        // The flush uploaded the current dirty tier; any spool predates it
        // and is now superseded.
        spool::discard_spool(self.fs.spool_dir(), self.sandboxes[idx].sandbox_id)
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
        if idx >= self.sandboxes.len() {
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
        for slot in &mut self.sandboxes {
            slot.backend = None; // RAM dies
        }
        // Flow C: the pooled/capture in-RAM tables (local bindings, migration
        // roles, live captures) die with the process; the live FC set and the
        // coordinator (a separate process) survive. The successor's reconcile
        // loop rebuilds the bindings from the coordinator — the None-arm path.
        self.reconcile.crash_ram();
        // Flow B: a process death is a ROLL.
        self.roll_generation();
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
        for slot in &mut self.sandboxes {
            slot.backend = None; // RAM dies — NO shutdown spool
        }
        self.reconcile.crash_ram();
        // Flow B: an abrupt death is still a ROLL (a fresh generation reattaches
        // the surviving FC VMs / durable disk).
        self.roll_generation();
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
        if self.sandboxes[idx].served_by == Some(self.generation) {
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
        for idx in 0..self.sandboxes.len() {
            if self.sandboxes[idx].backend.is_none() {
                self.rebuild(idx).await?;
            }
        }
        Ok(())
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
        let spool = match spool::read_spool(self.fs.spool_dir(), sandbox_id).await {
            Ok(s) => s,
            Err(_torn) => {
                // Torn/spliced → discard + rebuild from the durable pointer.
                spool::discard_spool(self.fs.spool_dir(), sandbox_id)
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
            if meta.manifest_ref().manifest_id == rebuild_ref.manifest_id {
                backend.adopt_unflushed(chunks).await;
            }
            // Adopted or stale-lineage: the spool is consumed either way.
            spool::discard_spool(self.fs.spool_dir(), sandbox_id)
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
        // Detached stage: RAM dies.
        for slot in &mut self.sandboxes {
            slot.backend = None;
        }
        self.reconcile.crash_ram();
        // Flow B: SIGTERM is a roll (the successor pidfd-reattaches).
        self.roll_generation();
        Ok(())
    }

    /// Seeded crash-point injection (ADR 0098 P4). Model the SIGTERM ladder's
    /// final-flush leg COMPLETING (every acked write durable in the published
    /// tier), then a crash landing at durable-operation boundary `cp` while
    /// the (now redundant) shutdown spool is being written / a durable_record
    /// is being persisted — construct that exact post-crash on-disk state via
    /// [`crash_state`], then drop RAM. The following `Restart` runs the REAL
    /// recovery, and oracle #1 must recover every acked write UNCONDITIONALLY:
    /// the spool is redundant here, so a torn/absent one is safely rejected
    /// and the published tier covers the writes. (The spool-as-only-copy path
    /// is the separate #225 `Sigterm` overrun; a torn ONLY-copy spool is the
    /// documented residual-loss window H5 tolerates, never constructed here.)
    pub async fn crash_at(&mut self, cp: CrashPoint) -> Result<(), String> {
        match cp {
            CrashPoint::SpoolChunks
            | CrashPoint::SpoolChunkMissing
            | CrashPoint::SpoolMarker
            | CrashPoint::SpoolDir => self.crash_at_spool(cp).await?,
            CrashPoint::PersistWritePartial
            | CrashPoint::PersistFsyncTemp
            | CrashPoint::PersistRename
            | CrashPoint::PersistFsyncParent => {
                // Durability first: complete the spool for every live sandbox
                // (no un-exported acked write is dropped), THEN exercise the
                // durable_record recovery at the boundary.
                for idx in 0..self.sandboxes.len() {
                    if self.sandboxes[idx].backend.is_some() {
                        self.spool_export(idx).await?;
                    }
                }
                crash_state::persist_boundary_recovers(self.fs.records_dir(), cp).await?;
            }
        }
        for slot in &mut self.sandboxes {
            slot.backend = None;
        }
        self.reconcile.crash_ram();
        // Flow B: the injected crash is a roll.
        self.roll_generation();
        Ok(())
    }

    /// The spool-boundary crash injection: complete the final-flush leg for
    /// every live sandbox (acked writes → published tier, redundant with the
    /// spool), then mangle sandbox 0's spool to `cp`'s post-crash state.
    async fn crash_at_spool(&mut self, cp: CrashPoint) -> Result<(), String> {
        // Sandbox 0 is the boundary target: guarantee ≥1 dirty chunk so a
        // chunk-bearing spool exists to mangle (the chunk boundaries need
        // content; a clean survivor's spool would be zero-chunk).
        if let Some(backend) = self.sandboxes[0].backend.clone() {
            if backend.dirty_bytes().await == 0 {
                self.guest_write(0, 0).await?;
            }
        }
        // Final-flush leg completes: export the dirty tier into a COMPLETE
        // spool (consistent with the ledger), then flush to publish the same
        // chunks (redundant durability). Direct `flush` (NOT `flush_tick`,
        // which would discard the spool) so the on-disk spool survives to be
        // mangled.
        for idx in 0..self.sandboxes.len() {
            let Some(backend) = self.sandboxes[idx].backend.clone() else {
                continue;
            };
            self.spool_export(idx).await?;
            backend
                .flush()
                .await
                .map_err(|e| format!("crash_at flush sandbox {idx}: {e}"))?;
            let published = backend.manifest_ref().await;
            self.sandboxes[idx].published_ref = Some(published);
            // Durable handoff via the (redundant) published tier — the spool
            // this crash then mangles is not the only copy.
            self.mark_flush_published(idx, published).await?;
        }
        let sandbox_id = self.sandboxes[0].sandbox_id;
        crash_state::mangle_spool(self.fs.spool_dir(), sandbox_id, cp).await
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
