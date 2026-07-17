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
use engram_host_agent::disk_daemon::spool;
use engram_host_core::{HostEffects, LiveManifestPublishRequest};
use engram_sim::{SimClock, SimEntropy};
use engram_storage_local::LocalBlobStorage;

use crate::coord_stub::SimCoordClient;
use crate::effects::{sim_effects, SeamLog};
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
    /// The manifest lineage the write was acked against — recorded so a
    /// future oracle can reason about lineage-gated spool adoption.
    pub lineage_at_ack: ManifestRef,
}

/// The append-only acked-write ledger — the oracle's durable memory. It
/// survives a [`CrashProcess`](crate::Step::CrashProcess) (a real coordinator
/// / an external observer remembers what the guest was told); the RAM
/// backends do not.
#[derive(Default)]
pub struct AckedWriteLedger {
    log: Vec<LedgerEntry>,
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
    pub ledger: AckedWriteLedger,
    /// Monotonic content-tag source. Advances only on writes, so the id a
    /// given step mints is a pure function of the step sequence (seed).
    next_tag: u64,
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
        let (effects, seam_log) = sim_effects(clock.clone(), entropy.clone(), coord.clone());
        // Deterministic, readable host id.
        let host_id = HostId::from(uuid::Uuid::from_u128(0x0A57_0000));

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
            sandboxes.push(SandboxSlot {
                sandbox_id,
                session_id,
                base_ref,
                published_ref: None,
                backend: Some(Arc::new(backend)),
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
            ledger: AckedWriteLedger::default(),
            next_tag: 0,
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

    /// Read a chunk back and, if the ledger has an acked tag for it, assert
    /// the read-after-write property inline (the oracle re-checks every acked
    /// chunk each step, but this gives a targeted read trace).
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
        if let Some(want) = self
            .ledger
            .latest_by_chunk()
            .get(&(idx, chunk_idx))
            .map(|e| e.content_tag)
        {
            if got != want {
                return Err(format!(
                    "read-after-write: sandbox {idx} chunk {chunk_idx} read tag {got}, acked tag {want}"
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

    /// Write the shutdown spool for a live sandbox: export the un-uploaded
    /// tier (dirty ∪ drained-pending) and durably spool it under the
    /// sandbox's dir. Models the predecessor's SIGTERM spool leg.
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
        Ok(())
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

    /// Rebuild one sandbox's backend from the surviving disk: `from_blob` at
    /// the rebuild ref, lineage-gate + adopt any spool, then discard the
    /// consumed spool. The lineage gate mirrors the real call site: a spool
    /// from a DIFFERENT manifest lineage is a loud discard, never an adopt.
    async fn rebuild(&mut self, idx: usize) -> Result<(), String> {
        let rebuild_ref = self.sandboxes[idx].rebuild_ref();
        let sandbox_id = self.sandboxes[idx].sandbox_id;
        let backend = build_backend(&self.store, self.fs.cache_dir(), idx, rebuild_ref).await;
        match spool::read_spool(self.fs.spool_dir(), sandbox_id).await {
            Ok(Some((meta, chunks))) => {
                if meta.manifest_ref().manifest_id == rebuild_ref.manifest_id {
                    backend.adopt_unflushed(chunks).await;
                }
                // Adopted or stale-lineage: the spool is consumed either way.
                spool::discard_spool(self.fs.spool_dir(), sandbox_id)
                    .await
                    .map_err(|e| format!("discard_spool sandbox {idx}: {e}"))?;
            }
            Ok(None) => {}
            Err(e) => {
                // A rejected spool is a real finding (torn/spliced) — surface
                // it; in the benign P2 path the spool is always well-formed.
                return Err(format!("read_spool sandbox {idx} rejected: {e}"));
            }
        }
        self.sandboxes[idx].backend = Some(Arc::new(backend));
        Ok(())
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
