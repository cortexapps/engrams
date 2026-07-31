//! Issue #529: host-durable eviction finalize.
//!
//! ADR 0045 D5's original shape made the eviction finalize a function of
//! THREE ephemeral things: a coordinator-RAM tokio task, a host-RAM tokio
//! task + `snapshot_waits` slot, and live RPC routing to a sandbox PG
//! already says nobody owns. Any of the three dying mid-upload silently
//! dropped the snapshot — resume then fell back to the prior periodic
//! checkpoint (up to `ENGRAM_CHECKPOINT_INTERVAL_SECS`, default 600s,
//! stale) and replayed turns the user already saw. Prod evidence: ~24%
//! of idle evictions (25/104, 14d window) hit this.
//!
//! The fix: once [`crate::pooled_backend::PooledBackend::snapshot_begin`]
//! returns, the finalize is a HOST-OWNED job that is a pure function of
//! DURABLE ON-DISK ARTIFACTS. It never touches the sandbox, the
//! coordinator, or any in-RAM map it didn't itself just (re)build from
//! disk. A host-agent process restart re-drives it from
//! [`EvictionFinalizeRecord::load_all`] (see
//! `PooledBackend::resume_pending_finalizes`, called at host-agent
//! startup); nothing but node loss can lose it.
//!
//! ## The record
//!
//! [`EvictionFinalizeRecord`] is the sibling of
//! [`crate::checkpoint::CheckpointRecord`] for the in-flight window
//! before a checkpoint record can be written: it carries everything the
//! job needs FROM DISK ALONE — `session_id`/`sandbox_id` from the
//! capture (not the RAM `session_bindings` map), the FC staging dir
//! (`dest`), the diff chain's previous manifest REF (the manifest
//! CONTENT is re-fetched from the chunk store, not from the RAM
//! `checkpoint_chains` map), and — when the capture had a dirty NBD disk
//! tier — the drained chunk bytes, persisted to `dest/disk-pending/`
//! (see [`crate::disk_daemon::PendingDiskFlush::into_chunks`]) because
//! that's the one input that otherwise lives ONLY in host-agent process
//! RAM until an upload consumes it.
//!
//! ## Stages
//!
//! `Captured → DiskUploaded → MemoryChunked → BlobsUploaded → (terminal)`.
//! Each leg is idempotent and persists the record (with `stage` advanced)
//! before the next leg runs, so a crash between any two legs re-drives
//! from exactly where it left off — never redoing durable work, never
//! skipping any.
//!
//! ## Deviation from the issue's sketch (documented, not silent)
//!
//! The issue sketched a bifurcated disk leg: a "live path" that reuses
//! `ChunkedDiskBackend::flush_upload` (preserving its rebase of the live
//! backend's `state`/`pending_uploads`) when the NBD data plane still
//! exists, and a "redrive path" that reconstructs the manifest from the
//! persisted bytes when it doesn't. This implementation always uses the
//! reconstruction path: the eviction flavor destroys the sandbox
//! immediately after finalize completes, so nothing ever reads the live
//! `ChunkedDiskBackend`'s rebased state again — the rebase `flush_upload`
//! performs is unobservable for this flavor. Collapsing to one code path
//! is simpler and lower-risk than keeping `PendingDiskFlush`'s live
//! backend handle + flush-pipeline guard pinned across a (now
//! backgrounded, potentially long) upload window, at the cost of a
//! redundant NVMe write+read of the dirty chunk bytes on the common
//! (non-crash) path — an O(dirty-set) cost, not O(image).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_chunk_store::manifest::{ChunkHash, ChunkRef, Manifest, ManifestKind};
use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::types::ids::{SandboxId, SessionId, SnapshotId};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::AuxBundleRef;
use engram_core::SandboxError;
pub use engram_host_core::FinalizeStage;
use engram_host_core::{plan_finalize_retry, FinalizeRetry, HostFs};
use engram_protocol::heartbeat::CheckpointKind;
use serde::{Deserialize, Serialize};

use crate::checkpoint::CheckpointRecord;

/// Default cap on redrive attempts before a finalize job is quarantined.
/// Overridable via `ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS`.
const DEFAULT_MAX_ATTEMPTS: u32 = 10;
/// The finalize manifest publish's version-conflict retry budget —
/// mirrors `ChunkedDiskBackend::flush_upload`'s `MAX_FLUSH_RETRIES`.
const MAX_MANIFEST_PUBLISH_RETRIES: u32 = 32;
/// ADR 0101 A: upload fan-out for the eviction-final disk publish —
/// the NBD flush path's width (`DISK_FLUSH_UPLOAD_CONCURRENCY`); the
/// host-global `UploadBudget` stays the cross-workload arbiter.
const DISK_PUBLISH_CONCURRENCY: usize = 32;
/// ADR 0101 A: fan-out for staging the drained chunks to
/// `disk-pending/` — independent files whose write→fsync→rename cost
/// is fsync-dominated; 16-way keeps the NVMe queue fed on the
/// `snapshot_begin` critical path.
const DISK_STAGE_CONCURRENCY: usize = 16;

pub(crate) fn max_attempts() -> u32 {
    std::env::var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

// `FinalizeStage` (the stage-explicit progress marker) moved to
// `engram_host_core::finalize` (ADR 0098 P5) — the ladder is a pure
// decision surface the host-internal simulator drives; serde variant
// names are unchanged, so persisted records are byte-compatible.

/// The drained NBD disk chunks + enough context to publish a disk
/// manifest without the live `ChunkedDiskBackend`. `base_manifest` is
/// the disk manifest ref that was live at capture time — the redrive's
/// publish base (`parent`); `chunks` is empty when the NBD data plane
/// was attached but nothing was dirty (the manifest just carries
/// `base_manifest` forward unchanged, mirroring `flush_upload`'s
/// empty-dirty-set short-circuit). The bytes themselves live in
/// `<dest>/disk-pending/<chunk_idx>.<hash>`, not in this record — they
/// can be multiple MiB each; the record stays small JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskPendingRecord {
    pub base_manifest: ManifestRef,
    pub chunk_size: u64,
    pub total_bytes: u64,
    pub chunks: Vec<(usize, ChunkHash)>,
}

/// Durable, self-describing record of one in-flight eviction finalize.
/// Written to `<checkpoint_dir>/finalize/<snapshot_id>.json` (write +
/// fsync + rename, same pattern as [`CheckpointRecord`]) BEFORE
/// `snapshot_begin` returns — this is the durability boundary moving
/// from "upload complete" to "this instant". Deleted on successful
/// completion; moved to `finalize/failed/<snapshot_id>.json` on
/// terminal quarantine (`attempts >= max_attempts`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvictionFinalizeRecord {
    pub snapshot_id: SnapshotId,
    /// From `session_bindings` AT CAPTURE TIME — the re-drive must not
    /// need the RAM map (it may not even exist in a fresh process).
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub image_version: String,
    pub size_bytes: u64,
    pub paused_at: DateTime<Utc>,
    pub captured_at: DateTime<Utc>,
    /// The FC snapshot staging dir (state.bin, memory.diff|bin,
    /// manifest.json, disk-pending/) — node-durable hostPath (ADR 0044).
    pub dest: PathBuf,
    /// Diff-chain input: the previous memory manifest REF. The CONTENT
    /// is re-fetched from the chunk store (not the RAM
    /// `checkpoint_chains` map) — durable, re-drive-safe. `None` ⇒ Full
    /// capture.
    pub chain_prev_ref: Option<ManifestRef>,
    /// `None` ⇒ no NBD disk tier was attached at capture (VZ/Process, or
    /// FC without NBD wired). `Some` with empty `chunks` ⇒ attached but
    /// nothing dirty.
    pub disk_pending: Option<DiskPendingRecord>,
    pub aux_bundles: Vec<AuxBundleRef>,
    pub stage: FinalizeStage,
    pub attempts: u32,
    /// Filled by the `DiskUploaded` leg.
    pub disk_manifest: Option<ManifestRef>,
    /// Filled by the `MemoryChunked` leg.
    pub memory_manifest: Option<ManifestRef>,
}

impl EvictionFinalizeRecord {
    /// Durably persist into `dir` via the shared [`crate::durable_record`]
    /// engine (write `.partial` → fsync → rename → **fsync parent dir**).
    /// This used to be a hand-rolled copy that OMITTED the parent-dir
    /// fsync — a crash right after the rename could lose the directory
    /// entry and silently drop a pending finalize (flagged in #707;
    /// retired here per simplify-via-abstractions).
    pub async fn persist(&self, fs: &dyn HostFs, dir: &Path) -> std::io::Result<()> {
        crate::durable_record::persist(fs, dir, self.snapshot_id, self, "eviction finalize record")
            .await
    }

    /// All pending finalize records in `dir` — the host-agent startup
    /// re-drive set. Torn-write tolerant via the shared engine.
    pub async fn load_all(fs: &dyn HostFs, dir: &Path) -> Vec<Self> {
        crate::durable_record::load_all(fs, dir, "eviction finalize record").await
    }

    async fn delete(fs: &dyn HostFs, dir: &Path, id: SnapshotId) {
        crate::durable_record::delete_acked(fs, dir, [id], "eviction finalize record").await;
    }

    /// Terminal give-up: move the record to `finalize/failed/` (kept for
    /// operator forensics, never silently dropped) and free the local
    /// staging dir — the honest floor from here on is the prior periodic
    /// checkpoint.
    async fn quarantine(&self, fs: &dyn HostFs, finalize_dir: &Path) {
        let failed_dir = finalize_dir.join("failed");
        if let Err(e) = self.persist(fs, &failed_dir).await {
            tracing::warn!(
                snapshot_id = %self.snapshot_id,
                sandbox_id = %self.sandbox_id,
                error = %e,
                "eviction finalize quarantine: failed to persist the quarantined record",
            );
        }
        Self::delete(fs, finalize_dir, self.snapshot_id).await;
        if let Err(e) = fs.remove_dir(&self.dest).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    snapshot_id = %self.snapshot_id,
                    dest = %self.dest.display(),
                    error = %e,
                    "eviction finalize quarantine: failed to free the local staging dir",
                );
            }
        }
        tracing::error!(
            snapshot_id = %self.snapshot_id,
            session_id = %self.session_id,
            sandbox_id = %self.sandbox_id,
            attempts = self.attempts,
            "eviction finalize quarantined after exhausting retries; resume falls back to \
             the prior periodic checkpoint (ENGRAM_CHECKPOINT_INTERVAL_SECS floor)",
        );
        metrics::counter!(crate::metrics::EVICTION_FINALIZE_QUARANTINED_TOTAL).increment(1);
    }
}

/// The terminal leg's sandbox-teardown seam (ADR 0098 P5). The prod impl
/// (`PooledDestroyer`, in `pooled_backend.rs`) upgrades a weak
/// `PooledBackend` ref at call time and calls the OUTER
/// `PooledBackend::destroy` (the full cleanup: egress unregister, NBD
/// slot release, checkpoint-chain teardown); the simulator records the
/// call. Destroy is BEST-EFFORT for the finalize: `NotFound` and a gone
/// backend are success, and any other error is logged, never a leg
/// failure — `orphan_reap` backstops it.
#[async_trait]
pub trait EvictionSandbox: Send + Sync {
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
}

/// ADR 0045 D5 (issue #529): the owned, 'static bundle of everything the
/// host-owned finalize job needs — Arc-clones of the `PooledBackend`'s
/// fields, exactly the `SnapshotFinisher` pattern, PLUS the
/// [`EvictionSandbox`] seam so the terminal stage can destroy the sandbox
/// without an owned `Arc<PooledBackend>` threaded through every call
/// site. `pub` with a constructor so the host-internal simulator
/// (`engram-dst-host`) can build one over its own stores and drive the
/// REAL legs (ADR 0098 P5).
#[derive(Clone)]
pub struct EvictionFinalizer {
    pub(crate) chunk_store: Option<ChunkStore>,
    pub(crate) chunk_cache: Option<ChunkCache>,
    pub(crate) bundle_dir: PathBuf,
    pub(crate) bundle_file_ext: &'static str,
    /// `<work_dir>/checkpoints` — `records/` and `finalize/` live under it.
    pub(crate) checkpoint_dir: PathBuf,
    pub(crate) pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
    pub(crate) destroyer: Arc<dyn EvictionSandbox>,
    pub(crate) fs: Arc<dyn HostFs>,
    pub(crate) max_attempts: u32,
}

impl EvictionFinalizer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chunk_store: Option<ChunkStore>,
        chunk_cache: Option<ChunkCache>,
        bundle_dir: PathBuf,
        bundle_file_ext: &'static str,
        checkpoint_dir: PathBuf,
        pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
        destroyer: Arc<dyn EvictionSandbox>,
        fs: Arc<dyn HostFs>,
        max_attempts: u32,
    ) -> Self {
        Self {
            chunk_store,
            chunk_cache,
            bundle_dir,
            bundle_file_ext,
            checkpoint_dir,
            pending_finalizes,
            destroyer,
            fs,
            max_attempts,
        }
    }

    pub fn records_dir(&self) -> PathBuf {
        self.checkpoint_dir.join("records")
    }

    pub fn finalize_dir(&self) -> PathBuf {
        self.checkpoint_dir.join("finalize")
    }
}

/// Persist the drained disk-flush chunk bytes to `<dest>/disk-pending/`
/// (write + fsync + rename each) — called from `snapshot_begin`,
/// synchronously, before it returns. Empty `chunks` is a no-op (nothing
/// dirty this capture). `pub` and cfg-free (pure fs): the host-internal
/// simulator seeds its finalize inputs through the REAL staging writer
/// on macOS (ADR 0098 P5); production's only caller stays the Linux-only
/// NBD drain.
pub async fn persist_disk_pending_chunks(
    dest: &Path,
    chunks: &[(usize, ChunkHash, Bytes)],
) -> std::io::Result<()> {
    use futures::stream::{StreamExt, TryStreamExt};
    if chunks.is_empty() {
        return Ok(());
    }
    let dir = dest.join("disk-pending");
    tokio::fs::create_dir_all(&dir).await?;
    // ADR 0101 A: fanned out — each chunk's own write→fsync→rename
    // ordering is preserved per file; only cross-file order relaxes,
    // which the redrive never depended on (it re-reads by recorded
    // index+hash). This runs on the eviction critical path (before
    // `snapshot_begin` returns), so the fsync serialization was
    // user-visible teardown time.
    async fn stage_one(
        dir: PathBuf,
        idx: usize,
        hash: ChunkHash,
        bytes: Bytes,
    ) -> std::io::Result<()> {
        let path = dir.join(format!("{idx}.{}", hash.to_hex()));
        let tmp = dir.join(format!("{idx}.{}.partial", hash.to_hex()));
        tokio::fs::write(&tmp, &bytes).await?;
        let f = tokio::fs::OpenOptions::new().read(true).open(&tmp).await?;
        f.sync_all().await?;
        tokio::fs::rename(&tmp, &path).await
    }
    // Owned items (a `Bytes` clone is a refcount bump), collected with a
    // plain loop — any closure over `&tuple` living inside the spawned
    // 'static finalize future trips rustc's FnOnce-not-general-enough
    // limitation (rust-lang/rust#102211).
    let mut items: Vec<(usize, ChunkHash, Bytes)> = Vec::with_capacity(chunks.len());
    for (idx, hash, bytes) in chunks {
        items.push((*idx, *hash, bytes.clone()));
    }
    futures::stream::iter(items)
        .map(|(idx, hash, bytes)| stage_one(dir.clone(), idx, hash, bytes))
        .buffer_unordered(DISK_STAGE_CONCURRENCY)
        .try_collect::<()>()
        .await
}

async fn read_disk_pending_chunks(
    fs: &dyn HostFs,
    dest: &Path,
    chunks: &[(usize, ChunkHash)],
) -> Result<Vec<(usize, ChunkHash, Bytes)>, SandboxError> {
    let dir = dest.join("disk-pending");
    let mut out = Vec::with_capacity(chunks.len());
    for (idx, hash) in chunks {
        let path = dir.join(format!("{idx}.{}", hash.to_hex()));
        let bytes = fs.read(&path).await.map_err(|e| {
            SandboxError::Snapshot(format!(
                "read disk-pending chunk {idx} ({}): {e}",
                path.display()
            ))
        })?;
        let actual_hash = ChunkHash::of(&bytes);
        if actual_hash != *hash {
            return Err(SandboxError::Snapshot(format!(
                "disk-pending chunk {} recorded hash {} does not match actual hash {}",
                path.display(),
                hash,
                actual_hash
            )));
        }
        out.push((*idx, *hash, Bytes::from(bytes)));
    }
    Ok(out)
}

/// Publish deterministic finalize content without conflating an occupied
/// version with an identical prior publish.
async fn publish_manifest_collision_safe(
    chunk_store: &ChunkStore,
    deterministic_ref: ManifestRef,
    manifest: &Manifest,
) -> Result<ManifestRef, engram_chunk_store::ChunkStoreError> {
    let mut attempt_ref = deterministic_ref;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match chunk_store.put_manifest(attempt_ref, manifest).await {
            Ok(()) => return Ok(attempt_ref),
            Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                latest,
                attempted,
                manifest_id,
            }) => {
                if attempt_ref == deterministic_ref && attempted == deterministic_ref.version {
                    let existing = chunk_store.get_manifest(deterministic_ref).await?;
                    if existing == *manifest {
                        return Ok(deterministic_ref);
                    }
                }
                if attempts >= MAX_MANIFEST_PUBLISH_RETRIES {
                    return Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                        latest,
                        attempted,
                        manifest_id,
                    });
                }
                attempt_ref = ManifestRef {
                    manifest_id,
                    version: latest + 1,
                };
            }
            Err(e) => return Err(e),
        }
    }
}

/// Publish a disk manifest by layering `chunks` onto `base_manifest`,
/// with retry-on-version-conflict — the redrive-safe equivalent of
/// `ChunkedDiskBackend::flush_upload`'s manifest rebuild, minus the live
/// backend's `state`/`pending_uploads` rebase (see the module-level
/// deviation note: unobservable once the sandbox is destroyed).
async fn publish_disk_manifest(
    chunk_store: &ChunkStore,
    base_manifest: ManifestRef,
    chunk_size: u64,
    total_bytes: u64,
    chunks: &[(usize, ChunkHash, Bytes)],
) -> Result<ManifestRef, SandboxError> {
    // Idempotent content-addressed puts — safe to redo on a redrive that
    // crashed after some (but not all) chunks landed. ADR 0101 A: fanned
    // out at the NBD flush path's width and UNCHECKED — the dirty set is
    // freshly re-chunked, so the old per-chunk dedup HEAD was a
    // guaranteed-miss round trip. `put_chunk_unchecked` recomputes the
    // hash from the bytes it uploads, so the recorded-hash comparison
    // keeps the corruption check the checked path provided.
    {
        use futures::stream::{StreamExt, TryStreamExt};
        async fn put_one(
            chunk_store: ChunkStore,
            recorded_hash: ChunkHash,
            bytes: Bytes,
        ) -> Result<(), SandboxError> {
            let returned_hash = chunk_store.put_chunk_unchecked(&bytes).await.map_err(|e| {
                SandboxError::Snapshot(format!("eviction finalize disk chunk upload: {e}"))
            })?;
            if returned_hash != recorded_hash {
                return Err(SandboxError::Snapshot(format!(
                    "eviction finalize disk chunk upload returned hash {returned_hash}, not \
                     recorded hash {recorded_hash}; the manifest would reference a chunk that \
                     does not exist under the recorded hash"
                )));
            }
            Ok(())
        }
        // Owned items (Bytes clone = refcount bump; ChunkStore clone =
        // Arc bumps) — see `persist_disk_pending_chunks` for why a
        // closure over `&tuple` can't live inside the spawned finalize
        // task.
        let mut items: Vec<(ChunkHash, Bytes)> = Vec::with_capacity(chunks.len());
        for (_, recorded_hash, bytes) in chunks {
            items.push((*recorded_hash, bytes.clone()));
        }
        futures::stream::iter(items)
            .map(|(recorded_hash, bytes)| put_one(chunk_store.clone(), recorded_hash, bytes))
            .buffer_unordered(DISK_PUBLISH_CONCURRENCY)
            .try_collect::<()>()
            .await?;
    }

    let base = chunk_store.get_manifest(base_manifest).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "eviction finalize: get base disk manifest {base_manifest}: {e}"
        ))
    })?;
    let mut refs: Vec<ChunkRef> = base.chunks;
    for (idx, hash, bytes) in chunks {
        let offset = (*idx as u64) * chunk_size;
        if let Some(existing) = refs.iter_mut().find(|c| c.offset == offset) {
            existing.hash = *hash;
        } else {
            refs.push(ChunkRef {
                offset,
                hash: *hash,
            });
        }
        let _ = bytes; // sizing only informs the caller's metrics, not the manifest
    }
    refs.sort_by_key(|c| c.offset);
    let new_manifest = Manifest {
        schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
        kind: ManifestKind::Disk,
        chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(chunk_size),
        total_bytes,
        chunks: refs,
        parent: Some(base_manifest),
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };

    let deterministic_ref = base_manifest.next_version();
    publish_manifest_collision_safe(chunk_store, deterministic_ref, &new_manifest)
        .await
        .map_err(|e| SandboxError::Snapshot(format!("eviction finalize: put disk manifest: {e}")))
}

/// The disk leg's publish work — pure (no stage/record mutation):
/// resolve the disk manifest ref this capture carries. `hot_chunks` is
/// the same-process fast path (ADR 0101 A): `snapshot_begin` hands the
/// drained bytes it just staged, so the common path skips the NVMe
/// read-back and `disk-pending/` serves purely as the crash-redrive
/// journal. A redrive (fresh process — no hot bytes — or a mismatch
/// against the record) falls back to reading + re-verifying the journal.
async fn disk_leg_work(
    f: &EvictionFinalizer,
    record: &EvictionFinalizeRecord,
    hot_chunks: Option<&[(usize, ChunkHash, Bytes)]>,
) -> Result<Option<ManifestRef>, SandboxError> {
    let start = crate::time_source::metrics_now();
    let resolved = match (&f.chunk_store, &record.disk_pending) {
        (_, None) => {
            // No NBD disk tier at capture — nothing to carry (matches
            // `finish()`'s behavior: `metadata.disk_manifest` stays
            // whatever the bare backend produced, i.e. `None`, when
            // `nbd_pending_flush` was `None`).
            None
        }
        (None, Some(pending)) => {
            // Defensive: chunk store vanished between snapshot_begin and
            // now (shouldn't happen — gated at snapshot_begin). Carry the
            // base forward unchanged rather than losing the disk tier.
            Some(pending.base_manifest)
        }
        (Some(_), Some(pending)) if pending.chunks.is_empty() => {
            // NBD attached but nothing dirty — carry forward unchanged,
            // mirroring `flush_upload`'s empty-dirty-set short-circuit.
            Some(pending.base_manifest)
        }
        (Some(chunk_store), Some(pending)) => {
            // The hot bytes are trusted only when they match the durable
            // record exactly (same indices, same hashes, in order) —
            // anything else means they belong to a different capture
            // generation, and the journal is the truth.
            let hot = hot_chunks.filter(|hot| {
                hot.len() == pending.chunks.len()
                    && hot
                        .iter()
                        .zip(&pending.chunks)
                        .all(|((hi, hh, _), (ri, rh))| hi == ri && hh == rh)
            });
            let owned;
            let chunks: &[(usize, ChunkHash, Bytes)] = match hot {
                Some(hot) => hot,
                None => {
                    owned = read_disk_pending_chunks(f.fs.as_ref(), &record.dest, &pending.chunks)
                        .await?;
                    &owned
                }
            };
            let published = publish_disk_manifest(
                chunk_store,
                pending.base_manifest,
                pending.chunk_size,
                pending.total_bytes,
                chunks,
            )
            .await?;
            Some(published)
        }
    };
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "disk")
        .record(start.elapsed().as_secs_f64());
    Ok(resolved)
}

/// The memory leg's publish work — pure (no stage/record mutation):
/// re-chunk + publish the memory manifest and patch the FC sidecar.
/// Returns the published ref (`None` = diff-less capture / chunk store
/// disabled) and the consumed input file to best-effort-delete once the
/// stage bump is durable (finding 1). Idempotent from any point: an
/// exact prior publish is reused, while a different occupant at the
/// deterministic ref is preserved and the capture publishes at latest+1.
async fn memory_leg_work(
    f: &EvictionFinalizer,
    record: &EvictionFinalizeRecord,
) -> Result<(Option<ManifestRef>, Option<PathBuf>), SandboxError> {
    let start = crate::time_source::metrics_now();
    let mut consumed: Option<PathBuf> = None;
    let mut memory_manifest: Option<ManifestRef> = None;
    if let Some(chunk_store) = f.chunk_store.as_ref() {
        let manifest_ref = if let Some(prev_ref) = record.chain_prev_ref {
            let diff_path = record.dest.join("memory.diff");
            if tokio::fs::metadata(&diff_path).await.is_ok() {
                let prev_manifest = chunk_store.get_manifest(prev_ref).await.map_err(|e| {
                    SandboxError::Snapshot(format!(
                        "eviction finalize: get chain-prev manifest {prev_ref}: {e}"
                    ))
                })?;
                let ranges = crate::checkpoint::dirty_ranges(&diff_path)
                    .map_err(|e| SandboxError::Snapshot(format!("dirty ranges: {e}")))?;
                let next = chunk_store
                    .update_for_dirty_ranges_sparse(&prev_manifest, &diff_path, &ranges)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("sparse re-chunk: {e}")))?;
                let next_ref = prev_ref.next_version();
                let published_ref = publish_manifest_collision_safe(chunk_store, next_ref, &next)
                    .await
                    .map_err(|e| SandboxError::Snapshot(format!("put manifest {next_ref}: {e}")))?;
                consumed = Some(diff_path);
                Some(published_ref)
            } else {
                // No diff on disk and the memory stage hasn't persisted
                // (the caller's ladder guard skips this leg once it has)
                // — nothing dirty this round; matches `finish()`'s
                // behavior for a diff-less capture.
                None
            }
        } else {
            let mem_path = record.dest.join("memory.bin");
            if tokio::fs::metadata(&mem_path).await.is_ok() {
                let mref = crate::pooled_backend::chunk_memory_to_store(
                    chunk_store,
                    &mem_path,
                    f.chunk_cache.as_ref(),
                    "evict_finalize",
                )
                .await?;
                consumed = Some(mem_path);
                Some(mref)
            } else {
                None
            }
        };
        if let Some(mref) = manifest_ref {
            let manifest_json = record.dest.join("manifest.json");
            crate::pooled_backend::patch_fc_manifest_memory_ref(
                &manifest_json,
                mref,
                Some(record.session_id),
            )
            .await?;
            memory_manifest = Some(mref);
        }
    }
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "memory")
        .record(start.elapsed().as_secs_f64());
    Ok((memory_manifest, consumed))
}

async fn run_blobs_leg(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    if record.stage != FinalizeStage::MemoryChunked {
        return Ok(());
    }
    let start = crate::time_source::metrics_now();
    if let Some(chunk_store) = f.chunk_store.as_ref() {
        let blob = chunk_store.blob_storage();
        let state_path = record.dest.join("state.bin");
        let sidecar_path = record.dest.join("manifest.json");
        if tokio::fs::metadata(&state_path).await.is_ok()
            && tokio::fs::metadata(&sidecar_path).await.is_ok()
        {
            let state_key = engram_chunk_store::snapshot_blob::state_blob_key(record.snapshot_id);
            let sidecar_key =
                engram_chunk_store::snapshot_blob::sidecar_blob_key(record.snapshot_id);
            // ADR 0101 A: independent blob objects — upload concurrently.
            let (state_res, sidecar_res) = tokio::join!(
                engram_chunk_store::snapshot_blob::upload_file(
                    blob.as_ref(),
                    &state_key,
                    &state_path
                ),
                engram_chunk_store::snapshot_blob::upload_file(
                    blob.as_ref(),
                    &sidecar_key,
                    &sidecar_path
                )
            );
            state_res.map_err(|e| SandboxError::Snapshot(format!("upload state.bin: {e}")))?;
            sidecar_res.map_err(|e| SandboxError::Snapshot(format!("upload sidecar.json: {e}")))?;
        }
        if !record.aux_bundles.is_empty() {
            crate::bundles::BundleStore::new(blob.clone(), f.bundle_dir.clone(), f.bundle_file_ext)
                .publish(&record.aux_bundles)
                .await?;
        }
    }
    record.stage = FinalizeStage::BlobsUploaded;
    record
        .persist(f.fs.as_ref(), &f.finalize_dir())
        .await
        .map_err(|e| SandboxError::Snapshot(format!("persist finalize record: {e}")))?;
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "blobs")
        .record(start.elapsed().as_secs_f64());
    Ok(())
}

/// Terminal: write the ordinary durable `CheckpointRecord { kind:
/// EvictionFinal }` (re-advertised on every heartbeat until the coord
/// acks it into PG via the reconcile — the ONLY place the snapshot row
/// now lands for this flavor), delete the `EvictionFinalizeRecord`,
/// clear `pending_finalizes`, then best-effort destroy the sandbox
/// (idempotent — a `NotFound` on redrive, where it's usually already
/// gone, is success).
async fn run_terminal(
    f: &EvictionFinalizer,
    record: &EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    if record.stage != FinalizeStage::BlobsUploaded {
        return Ok(());
    }
    let start = crate::time_source::metrics_now();
    let checkpoint = CheckpointRecord {
        snapshot_id: record.snapshot_id,
        session_id: record.session_id,
        sandbox_id: record.sandbox_id,
        image_version: record.image_version.clone(),
        size_bytes: record.size_bytes,
        disk_manifest: record.disk_manifest,
        memory_manifest: record.memory_manifest,
        aux_bundles: record.aux_bundles.clone(),
        paused_at: record.paused_at,
        captured_at: record.captured_at,
        kind: CheckpointKind::EvictionFinal,
    };
    checkpoint
        .persist(f.fs.as_ref(), &f.records_dir())
        .await
        .map_err(|e| SandboxError::Snapshot(format!("persist eviction-final checkpoint: {e}")))?;

    EvictionFinalizeRecord::delete(f.fs.as_ref(), &f.finalize_dir(), record.snapshot_id).await;
    f.pending_finalizes.remove(&record.sandbox_id);
    metrics::counter!(crate::metrics::EVICTION_FINALIZE_COMPLETED_TOTAL).increment(1);

    match f.destroyer.destroy(record.sandbox_id).await {
        Ok(()) => {}
        Err(SandboxError::NotFound) => {
            // Already gone (a prior attempt's destroy, the teardown
            // reconcile, or kubelet) — success, not a failure.
        }
        Err(e) => {
            tracing::warn!(
                sandbox_id = %record.sandbox_id,
                error = %e,
                "eviction finalize: best-effort destroy failed; orphan_reap backstops it",
            );
        }
    }
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "terminal")
        .record(start.elapsed().as_secs_f64());
    tracing::info!(
        snapshot_id = %record.snapshot_id,
        session_id = %record.session_id,
        sandbox_id = %record.sandbox_id,
        "idle eviction finalize completed (host-owned, issue #529)",
    );
    Ok(())
}

/// Persist the `DiskUploaded` bump (with the resolved ref), rolling
/// back the in-RAM mutations on a failed persist so a retry never
/// believes an un-persisted stage is durable — it re-observes
/// `Captured` and safely redoes the idempotent publish (findings 1/3).
/// Only after the bump is durable is `disk-pending/` deleted: deleting
/// first made a crash between delete and persist indistinguishable
/// from "nothing dirty this round" on redrive.
async fn persist_disk_bump(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
    disk_manifest: Option<ManifestRef>,
) -> Result<(), SandboxError> {
    let prev_stage = record.stage;
    let prev_disk_manifest = record.disk_manifest;
    record.disk_manifest = disk_manifest;
    record.stage = FinalizeStage::DiskUploaded;
    if let Err(e) = record.persist(f.fs.as_ref(), &f.finalize_dir()).await {
        record.stage = prev_stage;
        record.disk_manifest = prev_disk_manifest;
        return Err(SandboxError::Snapshot(format!(
            "persist finalize record: {e}"
        )));
    }
    let pending_dir = record.dest.join("disk-pending");
    let _ = f.fs.remove_dir(&pending_dir).await;
    Ok(())
}

/// Persist the `MemoryChunked` bump — same rollback-on-failed-persist
/// discipline as [`persist_disk_bump`]; the consumed input file is
/// deleted only once the bump is durable (a crash after the persist
/// just leaks an already-consumed source file, never re-read).
async fn persist_memory_bump(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
    memory_manifest: Option<ManifestRef>,
    consumed: Option<PathBuf>,
) -> Result<(), SandboxError> {
    let prev_stage = record.stage;
    let prev_memory_manifest = record.memory_manifest;
    record.memory_manifest = memory_manifest;
    record.stage = FinalizeStage::MemoryChunked;
    if let Err(e) = record.persist(f.fs.as_ref(), &f.finalize_dir()).await {
        record.stage = prev_stage;
        record.memory_manifest = prev_memory_manifest;
        return Err(SandboxError::Snapshot(format!(
            "persist finalize record: {e}"
        )));
    }
    if let Some(p) = consumed {
        let _ = f.fs.remove_file(&p).await;
    }
    Ok(())
}

/// One sleep-free pass over the legs — `pub` so the host-internal
/// simulator drives the REAL leg bodies step-by-step (ADR 0098 P5); the
/// backoff/quarantine verdict between passes is
/// [`engram_host_core::plan_finalize_retry`], which the sim consults the
/// same way [`run_eviction_finalize`] does.
pub async fn run_eviction_finalize_once(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    run_eviction_finalize_once_hot(f, record, None).await
}

/// ADR 0101 A: the disk and memory legs are independent idempotent
/// publishes — overlap them when both are still pending. The durable
/// ladder (`Captured → DiskUploaded → MemoryChunked`) is preserved by
/// persisting the bumps in order AFTER the join, so persisted records
/// (and redrive semantics) are byte-identical to the serial shape.
/// Failure note: if the disk leg fails while memory succeeded, no bump
/// persists and the retry re-runs both — the memory redo lands in its
/// deterministic-ref `VersionConflict` arm (idempotent success), at the
/// cost of one wasted re-chunk on that already-failing path.
async fn run_eviction_finalize_once_hot(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
    hot_disk: Option<&[(usize, ChunkHash, Bytes)]>,
) -> Result<(), SandboxError> {
    match record.stage {
        FinalizeStage::Captured => {
            let (disk_res, mem_res) = tokio::join!(
                disk_leg_work(f, record, hot_disk),
                memory_leg_work(f, record)
            );
            let disk_manifest = disk_res?;
            let (memory_manifest, consumed) = mem_res?;
            persist_disk_bump(f, record, disk_manifest).await?;
            persist_memory_bump(f, record, memory_manifest, consumed).await?;
        }
        FinalizeStage::DiskUploaded => {
            let (memory_manifest, consumed) = memory_leg_work(f, record).await?;
            persist_memory_bump(f, record, memory_manifest, consumed).await?;
        }
        _ => {}
    }
    run_blobs_leg(f, record).await?;
    run_terminal(f, record).await?;
    Ok(())
}

/// The finalize job. Spawned by `snapshot_begin` (holding the sandbox's
/// capture lock for the job's whole lifetime — periodic checkpoints stay
/// locked out until finalize completes, same as today) and by
/// `PooledBackend::resume_pending_finalizes` at host-agent startup (for
/// each redriven record, re-acquiring the lock fresh).
/// The outcome of one redrive attempt ([`run_eviction_finalize_attempt`]).
#[derive(Debug)]
pub enum FinalizeAttempt {
    /// The terminal leg ran — record deleted, destroy issued.
    Completed,
    /// The pass failed; retry after this backoff (the attempt count is
    /// already bumped + persisted).
    RetryAfter(Duration),
    /// Attempts exhausted — the record is quarantined and `pending_finalizes`
    /// cleared; nothing re-drives it.
    Quarantined,
}

/// One redrive attempt: a sleep-free pass over the legs plus the REAL
/// retry/quarantine verdict handling. `pub` so the host-internal simulator
/// drives the exact production loop body per tick (ADR 0098 P5) — the only
/// thing [`run_eviction_finalize`] adds is the backoff sleep.
pub async fn run_eviction_finalize_attempt(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> FinalizeAttempt {
    run_eviction_finalize_attempt_hot(f, record, None).await
}

/// [`run_eviction_finalize_attempt`] with the ADR 0101 A same-process
/// hot disk bytes (see [`disk_leg_work`]). The sim and startup redrive
/// use the plain flavor (`None` — a fresh process has no hot bytes by
/// definition); only `snapshot_begin`'s spawn threads them through.
async fn run_eviction_finalize_attempt_hot(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
    hot_disk: Option<&[(usize, ChunkHash, Bytes)]>,
) -> FinalizeAttempt {
    match run_eviction_finalize_once_hot(f, record, hot_disk).await {
        Ok(()) => FinalizeAttempt::Completed,
        Err(e) => {
            record.attempts += 1;
            tracing::warn!(
                snapshot_id = %record.snapshot_id,
                sandbox_id = %record.sandbox_id,
                stage = ?record.stage,
                attempts = record.attempts,
                error = %e,
                "eviction finalize leg failed; will retry with backoff",
            );
            match plan_finalize_retry(record.attempts, f.max_attempts) {
                FinalizeRetry::Quarantine => {
                    // Clear the idempotency entry BEFORE the record becomes
                    // externally observable as quarantined (quarantine()
                    // writes the finalize/failed/ marker and deletes `dest`).
                    // The reverse order leaves a window where a racing
                    // `snapshot_begin` (`pending_finalizes.get`) can hand
                    // back this snapshot_id as still-in-flight after it's
                    // already permanently dead — nothing re-drives a
                    // quarantined record, so that caller would wait out the
                    // row-watcher deadline for a row that will never land.
                    f.pending_finalizes.remove(&record.sandbox_id);
                    record.quarantine(f.fs.as_ref(), &f.finalize_dir()).await;
                    FinalizeAttempt::Quarantined
                }
                FinalizeRetry::Retry { backoff } => {
                    if let Err(persist_err) = record.persist(f.fs.as_ref(), &f.finalize_dir()).await
                    {
                        tracing::warn!(
                            snapshot_id = %record.snapshot_id,
                            error = %persist_err,
                            "eviction finalize: failed to persist attempt count; retrying anyway",
                        );
                    }
                    FinalizeAttempt::RetryAfter(backoff)
                }
            }
        }
    }
}

pub(crate) async fn run_eviction_finalize(
    f: EvictionFinalizer,
    mut record: EvictionFinalizeRecord,
    _capture_guard: tokio::sync::OwnedMutexGuard<()>,
    mut hot_disk: Option<Vec<(usize, ChunkHash, Bytes)>>,
) {
    loop {
        match run_eviction_finalize_attempt_hot(&f, &mut record, hot_disk.as_deref()).await {
            FinalizeAttempt::Completed | FinalizeAttempt::Quarantined => return,
            FinalizeAttempt::RetryAfter(backoff) => {
                if record.stage != FinalizeStage::Captured {
                    // The disk bump persisted — the drained bytes served
                    // their purpose; free them rather than pinning the
                    // dirty set in RAM across retry backoffs.
                    hot_disk = None;
                }
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

// `PooledBackend::eviction_finalizer` / `resume_pending_finalizes` live in
// `pooled_backend.rs` (they need direct access to several private fields —
// `chunk_store`, `checkpoint_dir`, `pending_finalizes`, `self_ref`,
// `capture_locks` — the same reasoning `finisher()` follows for
// `SnapshotFinisher`).

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopDestroyer;

    #[async_trait]
    impl EvictionSandbox for NoopDestroyer {
        async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    async fn disk_store_with_empty_base(root: &Path) -> (ChunkStore, ManifestRef) {
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(root.join("blob")),
        );
        let chunk_store = ChunkStore::new(blob);
        let base_ref = ManifestRef::new();
        let base = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(4096),
            total_bytes: 4096,
            chunks: Vec::new(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        chunk_store
            .put_manifest(base_ref, &base)
            .await
            .expect("seed base manifest");
        (chunk_store, base_ref)
    }

    #[tokio::test]
    async fn corrupted_staged_disk_chunk_retries_then_quarantines_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let (chunk_store, base_ref) = disk_store_with_empty_base(tmp.path()).await;
        let dest = tmp.path().join("capture");
        let bytes = Bytes::from(vec![0xaa; 4096]);
        let recorded_hash = ChunkHash::of(&bytes);
        persist_disk_pending_chunks(&dest, &[(0, recorded_hash, bytes)])
            .await
            .expect("stage disk chunk");
        let staged_path = dest
            .join("disk-pending")
            .join(format!("0.{}", recorded_hash.to_hex()));
        tokio::fs::write(&staged_path, vec![0xbb; 4096])
            .await
            .expect("corrupt staged chunk");

        let checkpoint_dir = tmp.path().join("checkpoints");
        let pending_finalizes = Arc::new(DashMap::new());
        let mut record = EvictionFinalizeRecord {
            snapshot_id: SnapshotId::new(),
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            image_version: "test".to_owned(),
            size_bytes: 0,
            paused_at: DateTime::<Utc>::UNIX_EPOCH,
            captured_at: DateTime::<Utc>::UNIX_EPOCH,
            dest,
            chain_prev_ref: None,
            disk_pending: Some(DiskPendingRecord {
                base_manifest: base_ref,
                chunk_size: 4096,
                total_bytes: 4096,
                chunks: vec![(0, recorded_hash)],
            }),
            aux_bundles: Vec::new(),
            stage: FinalizeStage::Captured,
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        };
        pending_finalizes.insert(record.sandbox_id, record.snapshot_id);
        let finalizer = EvictionFinalizer::new(
            Some(chunk_store.clone()),
            None,
            tmp.path().join("bundles"),
            "tar",
            checkpoint_dir,
            pending_finalizes,
            Arc::new(NoopDestroyer),
            Arc::new(engram_host_core::TokioFs),
            2,
        );

        assert!(matches!(
            run_eviction_finalize_attempt(&finalizer, &mut record).await,
            FinalizeAttempt::RetryAfter(_)
        ));
        assert!(matches!(
            run_eviction_finalize_attempt(&finalizer, &mut record).await,
            FinalizeAttempt::Quarantined
        ));
        assert!(chunk_store
            .get_manifest(base_ref.next_version())
            .await
            .is_err());
    }

    fn record_with_one_staged_chunk(
        dest: PathBuf,
        base_ref: ManifestRef,
        recorded_hash: ChunkHash,
    ) -> EvictionFinalizeRecord {
        EvictionFinalizeRecord {
            snapshot_id: SnapshotId::new(),
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            image_version: "test".to_owned(),
            size_bytes: 0,
            paused_at: DateTime::<Utc>::UNIX_EPOCH,
            captured_at: DateTime::<Utc>::UNIX_EPOCH,
            dest,
            chain_prev_ref: None,
            disk_pending: Some(DiskPendingRecord {
                base_manifest: base_ref,
                chunk_size: 4096,
                total_bytes: 4096,
                chunks: vec![(0, recorded_hash)],
            }),
            aux_bundles: Vec::new(),
            stage: FinalizeStage::Captured,
            attempts: 0,
            disk_manifest: None,
            memory_manifest: None,
        }
    }

    fn finalizer_over(
        chunk_store: &ChunkStore,
        tmp: &Path,
        record: &EvictionFinalizeRecord,
    ) -> EvictionFinalizer {
        let pending_finalizes = Arc::new(DashMap::new());
        pending_finalizes.insert(record.sandbox_id, record.snapshot_id);
        EvictionFinalizer::new(
            Some(chunk_store.clone()),
            None,
            tmp.join("bundles"),
            "tar",
            tmp.join("checkpoints"),
            pending_finalizes,
            Arc::new(NoopDestroyer),
            Arc::new(engram_host_core::TokioFs),
            2,
        )
    }

    /// ADR 0101 A: the hot-bytes fast path — `snapshot_begin` hands the
    /// drained chunk bytes to the finalize job in memory, so the common
    /// path never reads `disk-pending/` back (it is purely the
    /// crash-redrive journal). Proven by corrupting the staged journal:
    /// with matching hot bytes the finalize must complete and publish
    /// the recorded hash without touching the corrupted files.
    #[tokio::test]
    async fn hot_disk_bytes_bypass_the_staged_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let (chunk_store, base_ref) = disk_store_with_empty_base(tmp.path()).await;
        let dest = tmp.path().join("capture");
        let bytes = Bytes::from(vec![0xaa; 4096]);
        let recorded_hash = ChunkHash::of(&bytes);
        persist_disk_pending_chunks(&dest, &[(0, recorded_hash, bytes.clone())])
            .await
            .expect("stage disk chunk");
        let staged_path = dest
            .join("disk-pending")
            .join(format!("0.{}", recorded_hash.to_hex()));
        tokio::fs::write(&staged_path, vec![0xbb; 4096])
            .await
            .expect("corrupt staged chunk");

        let mut record = record_with_one_staged_chunk(dest, base_ref, recorded_hash);
        let finalizer = finalizer_over(&chunk_store, tmp.path(), &record);

        let hot = vec![(0usize, recorded_hash, bytes)];
        assert!(matches!(
            run_eviction_finalize_attempt_hot(&finalizer, &mut record, Some(&hot)).await,
            FinalizeAttempt::Completed
        ));
        let published = chunk_store
            .get_manifest(base_ref.next_version())
            .await
            .expect("manifest published from the hot bytes");
        assert_eq!(published.chunks.len(), 1);
        assert_eq!(published.chunks[0].hash, recorded_hash);
    }

    /// ADR 0101 A: hot bytes that don't match the durable record (a
    /// different capture generation than the journal) are ignored — the
    /// journal is the truth, and the finalize reads + re-verifies it.
    #[tokio::test]
    async fn mismatched_hot_bytes_fall_back_to_the_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let (chunk_store, base_ref) = disk_store_with_empty_base(tmp.path()).await;
        let dest = tmp.path().join("capture");
        let bytes = Bytes::from(vec![0xaa; 4096]);
        let recorded_hash = ChunkHash::of(&bytes);
        persist_disk_pending_chunks(&dest, &[(0, recorded_hash, bytes)])
            .await
            .expect("stage disk chunk");

        let mut record = record_with_one_staged_chunk(dest, base_ref, recorded_hash);
        let finalizer = finalizer_over(&chunk_store, tmp.path(), &record);

        // Hot bytes whose hash list disagrees with the record — must be
        // rejected by the trust filter, not uploaded.
        let bogus = Bytes::from(vec![0xcc; 4096]);
        let hot = vec![(0usize, ChunkHash::of(&bogus), bogus)];
        assert!(matches!(
            run_eviction_finalize_attempt_hot(&finalizer, &mut record, Some(&hot)).await,
            FinalizeAttempt::Completed
        ));
        let published = chunk_store
            .get_manifest(base_ref.next_version())
            .await
            .expect("manifest published from the journal");
        assert_eq!(
            published.chunks[0].hash, recorded_hash,
            "the journal's recorded chunk (not the bogus hot bytes) is what published",
        );
        assert_eq!(
            chunk_store.get_chunk(recorded_hash).await.unwrap(),
            Bytes::from(vec![0xaa; 4096]),
            "the durable chunk bytes came from the journal",
        );
    }

    #[tokio::test]
    async fn publish_disk_manifest_rejects_recorded_hash_mismatch_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let (chunk_store, base_ref) = disk_store_with_empty_base(tmp.path()).await;
        let bytes = Bytes::from(vec![0xaa; 4096]);
        let recorded_hash = ChunkHash::of(b"other");

        let result = publish_disk_manifest(
            &chunk_store,
            base_ref,
            4096,
            4096,
            &[(0, recorded_hash, bytes)],
        )
        .await;

        assert!(matches!(result, Err(SandboxError::Snapshot(_))));
        assert!(chunk_store
            .get_manifest(base_ref.next_version())
            .await
            .is_err());
    }

    /// An exact prior publish at the deterministic ref is an idempotent
    /// redrive and reuses that ref.
    #[tokio::test]
    async fn publish_disk_manifest_conflict_at_deterministic_ref_is_idempotent_success() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let chunk_store = ChunkStore::new(blob);

        // Base disk manifest, as it stood at capture time.
        let base_ref = ManifestRef::new();
        let base = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(4096),
            total_bytes: 4096,
            chunks: Vec::new(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        chunk_store
            .put_manifest(base_ref, &base)
            .await
            .expect("seed base manifest");

        let bytes = Bytes::from_static(b"one dirty 4KiB-ish chunk's worth of bytes");
        let hash = chunk_store.put_chunk(&bytes).await.expect("put chunk");
        let chunks = vec![(0usize, hash, bytes)];

        // Simulate a prior crash-redrive that already published the
        // deterministic next version with EXACTLY the content this
        // redrive attempt would independently reconstruct.
        let deterministic_ref = base_ref.next_version();
        let prior_publish = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(4096),
            total_bytes: 4096,
            chunks: vec![ChunkRef { offset: 0, hash }],
            parent: Some(base_ref),
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        chunk_store
            .put_manifest(deterministic_ref, &prior_publish)
            .await
            .expect("seed prior-crash publish at the deterministic ref");

        // The redrive: must treat the resulting VersionConflict as
        // idempotent success, returning the SAME deterministic ref rather
        // than erroring or bumping to `version + 2`.
        let result = publish_disk_manifest(&chunk_store, base_ref, 4096, 4096, &chunks)
            .await
            .expect("a conflict at the deterministic ref is idempotent success, not an error");
        assert_eq!(
            result, deterministic_ref,
            "must return the already-published deterministic ref, not bump past it"
        );
    }

    #[tokio::test]
    async fn publish_disk_manifest_different_deterministic_occupant_publishes_at_latest_plus_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (chunk_store, base_ref) = disk_store_with_empty_base(tmp.path()).await;

        let dest = tmp.path().join("capture");
        let staged_bytes = Bytes::from(vec![0xaa; 4096]);
        let staged_hash = ChunkHash::of(&staged_bytes);
        persist_disk_pending_chunks(&dest, &[(0, staged_hash, staged_bytes)])
            .await
            .expect("stage authoritative capture chunk");
        let mut record = record_with_one_staged_chunk(dest, base_ref, staged_hash);
        let finalizer = finalizer_over(&chunk_store, tmp.path(), &record);

        let occupant_bytes = Bytes::from(vec![0x11; 4096]);
        let occupant_hash = chunk_store
            .put_chunk(&occupant_bytes)
            .await
            .expect("put occupant chunk");
        let deterministic_ref = base_ref.next_version();
        let occupant = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(4096),
            total_bytes: 4096,
            chunks: vec![ChunkRef {
                offset: 0,
                hash: occupant_hash,
            }],
            parent: Some(base_ref),
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        chunk_store
            .put_manifest(deterministic_ref, &occupant)
            .await
            .expect("seed a different manifest at the deterministic ref");

        assert!(matches!(
            run_eviction_finalize_attempt(&finalizer, &mut record).await,
            FinalizeAttempt::Completed
        ));
        let result = record
            .disk_manifest
            .expect("finalize persisted the authoritative disk ref");

        assert_eq!(result, deterministic_ref.next_version());
        let published = chunk_store
            .get_manifest(result)
            .await
            .expect("get authoritative capture manifest");
        for (idx, hash) in &record.disk_pending.as_ref().unwrap().chunks {
            assert!(published
                .chunks
                .iter()
                .any(|chunk| chunk.offset == (*idx as u64) * 4096 && chunk.hash == *hash));
        }
        assert_eq!(
            chunk_store
                .get_manifest(deterministic_ref)
                .await
                .expect("get original occupant"),
            occupant
        );
    }
}
