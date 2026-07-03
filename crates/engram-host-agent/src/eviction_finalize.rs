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
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_chunk_store::manifest::{ChunkHash, ChunkRef, Manifest, ManifestKind};
use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::types::ids::{SandboxId, SessionId, SnapshotId};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::AuxBundleRef;
use engram_core::SandboxError;
use engram_protocol::heartbeat::CheckpointKind;
use serde::{Deserialize, Serialize};

use crate::checkpoint::CheckpointRecord;
use crate::pooled_backend::PooledBackend;

/// Default cap on redrive attempts before a finalize job is quarantined.
/// Overridable via `ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS`.
const DEFAULT_MAX_ATTEMPTS: u32 = 10;
/// Backoff: `30s * attempt`, capped at 5 minutes.
const BACKOFF_UNIT: Duration = Duration::from_secs(30);
const BACKOFF_CAP: Duration = Duration::from_secs(300);
/// The disk manifest publish's own version-conflict retry budget —
/// mirrors `ChunkedDiskBackend::flush_upload`'s `MAX_FLUSH_RETRIES`.
const MAX_MANIFEST_PUBLISH_RETRIES: u32 = 32;

pub(crate) fn max_attempts() -> u32 {
    std::env::var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

/// Stage-explicit progress marker. Each variant means "everything up to
/// and including this leg is durable"; `run_eviction_finalize_once` skips
/// legs already past the persisted stage.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum FinalizeStage {
    #[default]
    Captured,
    DiskUploaded,
    MemoryChunked,
    BlobsUploaded,
}

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
    fn path_in(dir: &Path, id: SnapshotId) -> PathBuf {
        dir.join(format!("{id}.json"))
    }

    /// Durably persist (write + fsync via rename) into `dir`.
    pub async fn persist(&self, dir: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let dest = Self::path_in(dir, self.snapshot_id);
        let tmp = dest.with_extension("json.partial");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("serialize finalize record: {e}")))?;
        tokio::fs::write(&tmp, &bytes).await?;
        let f = tokio::fs::OpenOptions::new().read(true).open(&tmp).await?;
        f.sync_all().await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    /// All pending finalize records in `dir` — the host-agent startup
    /// re-drive set. Unreadable/partial files are skipped with a warn —
    /// a torn write must not wedge startup.
    pub async fn load_all(dir: &Path) -> Vec<Self> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return out;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if !p.is_file() || p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read(&p).await {
                Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e,
                            "unparseable eviction finalize record; skipping");
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "unreadable eviction finalize record; skipping");
                }
            }
        }
        out
    }

    async fn delete(dir: &Path, id: SnapshotId) {
        let p = Self::path_in(dir, id);
        if let Err(e) = tokio::fs::remove_file(&p).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %p.display(), error = %e,
                    "failed to delete completed eviction finalize record");
            }
        }
    }

    /// Terminal give-up: move the record to `finalize/failed/` (kept for
    /// operator forensics, never silently dropped) and free the local
    /// staging dir — the honest floor from here on is the prior periodic
    /// checkpoint.
    async fn quarantine(&self, finalize_dir: &Path) {
        let failed_dir = finalize_dir.join("failed");
        if let Err(e) = self.persist(&failed_dir).await {
            tracing::warn!(
                snapshot_id = %self.snapshot_id,
                sandbox_id = %self.sandbox_id,
                error = %e,
                "eviction finalize quarantine: failed to persist the quarantined record",
            );
        }
        Self::delete(finalize_dir, self.snapshot_id).await;
        if let Err(e) = tokio::fs::remove_dir_all(&self.dest).await {
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

/// ADR 0045 D5 (issue #529): the owned, 'static bundle of everything the
/// host-owned finalize job needs — Arc-clones of the `PooledBackend`'s
/// fields, exactly the `SnapshotFinisher` pattern, PLUS a weak
/// self-reference so the terminal stage can call the OUTER
/// `PooledBackend::destroy` (the full cleanup: egress unregister, NBD
/// slot release, checkpoint-chain teardown — not just the inner
/// backend's VM teardown) without needing an owned `Arc<PooledBackend>`
/// threaded through every call site.
#[derive(Clone)]
pub(crate) struct EvictionFinalizer {
    pub(crate) chunk_store: Option<ChunkStore>,
    pub(crate) chunk_cache: Option<ChunkCache>,
    pub(crate) bundle_dir: PathBuf,
    pub(crate) bundle_file_ext: &'static str,
    /// `<work_dir>/checkpoints` — `records/` and `finalize/` live under it.
    pub(crate) checkpoint_dir: PathBuf,
    pub(crate) pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
    pub(crate) self_ref: Arc<OnceLock<Weak<PooledBackend>>>,
    pub(crate) max_attempts: u32,
}

impl EvictionFinalizer {
    pub(crate) fn records_dir(&self) -> PathBuf {
        self.checkpoint_dir.join("records")
    }

    pub(crate) fn finalize_dir(&self) -> PathBuf {
        self.checkpoint_dir.join("finalize")
    }
}

/// Persist the drained disk-flush chunk bytes to `<dest>/disk-pending/`
/// (write + fsync + rename each) — called from `snapshot_begin`,
/// synchronously, before it returns. Empty `chunks` is a no-op (nothing
/// dirty this capture). `#[cfg]`'d like `PendingDiskFlush::into_chunks`
/// — its only caller (NBD is Linux-only).
#[cfg(target_os = "linux")]
pub(crate) async fn persist_disk_pending_chunks(
    dest: &Path,
    chunks: &[(usize, ChunkHash, Bytes)],
) -> std::io::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let dir = dest.join("disk-pending");
    tokio::fs::create_dir_all(&dir).await?;
    for (idx, hash, bytes) in chunks {
        let path = dir.join(format!("{idx}.{}", hash.to_hex()));
        let tmp = dir.join(format!("{idx}.{}.partial", hash.to_hex()));
        tokio::fs::write(&tmp, bytes).await?;
        let f = tokio::fs::OpenOptions::new().read(true).open(&tmp).await?;
        f.sync_all().await?;
        tokio::fs::rename(&tmp, &path).await?;
    }
    Ok(())
}

async fn read_disk_pending_chunks(
    dest: &Path,
    chunks: &[(usize, ChunkHash)],
) -> Result<Vec<(usize, ChunkHash, Bytes)>, SandboxError> {
    let dir = dest.join("disk-pending");
    let mut out = Vec::with_capacity(chunks.len());
    for (idx, hash) in chunks {
        let path = dir.join(format!("{idx}.{}", hash.to_hex()));
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            SandboxError::Snapshot(format!(
                "read disk-pending chunk {idx} ({}): {e}",
                path.display()
            ))
        })?;
        out.push((*idx, *hash, Bytes::from(bytes)));
    }
    Ok(out)
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
    // crashed after some (but not all) chunks landed.
    for (_, _hash, bytes) in chunks {
        chunk_store.put_chunk(bytes).await.map_err(|e| {
            SandboxError::Snapshot(format!("eviction finalize disk chunk upload: {e}"))
        })?;
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

    let mut attempt_ref = base_manifest.next_version();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match chunk_store.put_manifest(attempt_ref, &new_manifest).await {
            Ok(()) => return Ok(attempt_ref),
            Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                latest,
                manifest_id,
                ..
            }) => {
                if attempts >= MAX_MANIFEST_PUBLISH_RETRIES {
                    return Err(SandboxError::Snapshot(format!(
                        "eviction finalize disk manifest {manifest_id}: version conflict \
                         after {attempts} attempts (latest {latest})"
                    )));
                }
                attempt_ref = ManifestRef {
                    manifest_id,
                    version: latest + 1,
                };
            }
            Err(e) => {
                return Err(SandboxError::Snapshot(format!(
                    "eviction finalize: put disk manifest: {e}"
                )))
            }
        }
    }
}

async fn run_disk_leg(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    if record.stage != FinalizeStage::Captured {
        return Ok(());
    }
    let start = std::time::Instant::now();
    match (&f.chunk_store, &record.disk_pending) {
        (_, None) => {
            // No NBD disk tier at capture — nothing to carry (matches
            // `finish()`'s behavior: `metadata.disk_manifest` stays
            // whatever the bare backend produced, i.e. `None`, when
            // `nbd_pending_flush` was `None`).
        }
        (None, Some(pending)) => {
            // Defensive: chunk store vanished between snapshot_begin and
            // now (shouldn't happen — gated at snapshot_begin). Carry the
            // base forward unchanged rather than losing the disk tier.
            record.disk_manifest = Some(pending.base_manifest);
        }
        (Some(_), Some(pending)) if pending.chunks.is_empty() => {
            // NBD attached but nothing dirty — carry forward unchanged,
            // mirroring `flush_upload`'s empty-dirty-set short-circuit.
            record.disk_manifest = Some(pending.base_manifest);
        }
        (Some(chunk_store), Some(pending)) => {
            let chunks = read_disk_pending_chunks(&record.dest, &pending.chunks).await?;
            let published = publish_disk_manifest(
                chunk_store,
                pending.base_manifest,
                pending.chunk_size,
                pending.total_bytes,
                &chunks,
            )
            .await?;
            record.disk_manifest = Some(published);
        }
    }
    // Persist the stage bump (with the resolved `disk_manifest` ref)
    // BEFORE deleting `disk-pending/` — findings 1/3: deleting first made
    // a crash between delete and persist indistinguishable from "nothing
    // dirty this round" on redrive (`read_disk_pending_chunks` ENOENTs,
    // quarantining a snapshot whose manifest may already be durably
    // published). A failed persist here must not leave `record.stage`
    // mutated in RAM out from under the on-disk truth, so roll it back on
    // error — the next attempt re-observes `Captured` and safely redoes
    // the (idempotent, content-addressed) publish above.
    let prev_stage = record.stage;
    record.stage = FinalizeStage::DiskUploaded;
    if let Err(e) = record.persist(&f.finalize_dir()).await {
        record.stage = prev_stage;
        return Err(SandboxError::Snapshot(format!(
            "persist finalize record: {e}"
        )));
    }
    let pending_dir = record.dest.join("disk-pending");
    let _ = tokio::fs::remove_dir_all(&pending_dir).await;
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "disk")
        .record(start.elapsed().as_secs_f64());
    Ok(())
}

async fn run_memory_leg(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    if record.stage != FinalizeStage::DiskUploaded {
        return Ok(());
    }
    let start = std::time::Instant::now();
    // The input file to best-effort-delete AFTER the stage bump is
    // durable (finding 1). `None` when this attempt didn't consume a
    // fresh on-disk input (nothing to clean up, or the chunk_store leg
    // is disabled).
    let mut consumed: Option<PathBuf> = None;
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
                match chunk_store.put_manifest(next_ref, &next).await {
                    Ok(()) => {}
                    // Finding 2: `put_manifest` conflicts exactly when the
                    // (manifest_id, version) key already exists — and we
                    // always target the deterministic `next_ref`, so a
                    // conflict here can only mean a prior attempt already
                    // published this exact content (a crash between that
                    // `put_manifest` and this leg's stage-bump persist).
                    // That's idempotent success, not a real race.
                    Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                        attempted, ..
                    }) if attempted == next_ref.version => {}
                    Err(e) => {
                        return Err(SandboxError::Snapshot(format!(
                            "put manifest {next_ref}: {e}"
                        )))
                    }
                }
                consumed = Some(diff_path);
                Some(next_ref)
            } else {
                // No diff on disk and the stage is still `DiskUploaded`
                // (the guard above already skips this leg once the stage
                // bump persists) — nothing dirty this round; matches
                // `finish()`'s behavior for a diff-less capture.
                None
            }
        } else {
            let mem_path = record.dest.join("memory.bin");
            if tokio::fs::metadata(&mem_path).await.is_ok() {
                let mref = crate::pooled_backend::chunk_memory_to_store(
                    chunk_store,
                    &mem_path,
                    f.chunk_cache.as_ref(),
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
            record.memory_manifest = Some(mref);
        }
    }
    // Persist the stage bump (with `memory_manifest`) BEFORE deleting the
    // consumed input (finding 1) — same durability-ordering fix as the
    // disk leg. Roll back the in-RAM mutation on a failed persist so a
    // subsequent retry doesn't believe this stage is durable when it
    // isn't (it re-observes `DiskUploaded` and safely redoes the
    // idempotent work above).
    let prev_stage = record.stage;
    let prev_memory_manifest = record.memory_manifest;
    record.stage = FinalizeStage::MemoryChunked;
    if let Err(e) = record.persist(&f.finalize_dir()).await {
        record.stage = prev_stage;
        record.memory_manifest = prev_memory_manifest;
        return Err(SandboxError::Snapshot(format!(
            "persist finalize record: {e}"
        )));
    }
    // Only now — durably recorded — delete the consumed input. A crash
    // here just leaks the already-consumed source file; the manifest ref
    // is already durable and the stage guard skips this leg on redrive,
    // so it's never re-read.
    if let Some(p) = consumed {
        let _ = tokio::fs::remove_file(&p).await;
    }
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "memory")
        .record(start.elapsed().as_secs_f64());
    Ok(())
}

async fn run_blobs_leg(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    if record.stage != FinalizeStage::MemoryChunked {
        return Ok(());
    }
    let start = std::time::Instant::now();
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
            engram_chunk_store::snapshot_blob::upload_file(blob.as_ref(), &state_key, &state_path)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("upload state.bin: {e}")))?;
            engram_chunk_store::snapshot_blob::upload_file(
                blob.as_ref(),
                &sidecar_key,
                &sidecar_path,
            )
            .await
            .map_err(|e| SandboxError::Snapshot(format!("upload sidecar.json: {e}")))?;
        }
        if !record.aux_bundles.is_empty() {
            crate::bundles::BundleStore::new(blob.clone(), f.bundle_dir.clone(), f.bundle_file_ext)
                .publish(&record.aux_bundles)
                .await?;
        }
    }
    record.stage = FinalizeStage::BlobsUploaded;
    record
        .persist(&f.finalize_dir())
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
    let start = std::time::Instant::now();
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
        .persist(&f.records_dir())
        .await
        .map_err(|e| SandboxError::Snapshot(format!("persist eviction-final checkpoint: {e}")))?;

    EvictionFinalizeRecord::delete(&f.finalize_dir(), record.snapshot_id).await;
    f.pending_finalizes.remove(&record.sandbox_id);
    metrics::counter!(crate::metrics::EVICTION_FINALIZE_COMPLETED_TOTAL).increment(1);

    if let Some(pooled) = f.self_ref.get().and_then(Weak::upgrade) {
        use engram_core::traits::SandboxBackend as _;
        match pooled.destroy(record.sandbox_id).await {
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

async fn run_eviction_finalize_once(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    run_disk_leg(f, record).await?;
    run_memory_leg(f, record).await?;
    run_blobs_leg(f, record).await?;
    run_terminal(f, record).await?;
    Ok(())
}

/// The finalize job. Spawned by `snapshot_begin` (holding the sandbox's
/// capture lock for the job's whole lifetime — periodic checkpoints stay
/// locked out until finalize completes, same as today) and by
/// `PooledBackend::resume_pending_finalizes` at host-agent startup (for
/// each redriven record, re-acquiring the lock fresh).
pub(crate) async fn run_eviction_finalize(
    f: EvictionFinalizer,
    mut record: EvictionFinalizeRecord,
    _capture_guard: tokio::sync::OwnedMutexGuard<()>,
) {
    loop {
        match run_eviction_finalize_once(&f, &mut record).await {
            Ok(()) => return,
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
                if record.attempts >= f.max_attempts {
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
                    record.quarantine(&f.finalize_dir()).await;
                    return;
                }
                if let Err(persist_err) = record.persist(&f.finalize_dir()).await {
                    tracing::warn!(
                        snapshot_id = %record.snapshot_id,
                        error = %persist_err,
                        "eviction finalize: failed to persist attempt count; retrying anyway",
                    );
                }
                let backoff = (BACKOFF_UNIT * record.attempts).min(BACKOFF_CAP);
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
