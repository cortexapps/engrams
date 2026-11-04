//! Host-durable eviction finalize (issue #529).
//!
//! Once `snapshot_begin` returns, the finalize is a host-owned job that
//! is a pure function of durable artifacts: the pending disk checkpoints
//! on the session disk, found by name, and the FC staging dir (`dest`).
//! It never touches the sandbox, the coordinator, or an in-RAM map it
//! did not rebuild from disk. A host-agent restart re-drives it from
//! [`EvictionFinalizeRecord::load_all`]; nothing but node loss loses it.
//!
//! Stages: `Captured → DiskUploaded → MemoryChunked → BlobsUploaded →
//! (terminal)`. Each leg is idempotent and persists the record with
//! `stage` advanced before the next leg runs.
use engram_core::types::snapshot::DiskRole;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_chunk_store::manifest::Manifest;
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
pub(crate) fn max_attempts() -> u32 {
    std::env::var("ENGRAM_EVICTION_FINALIZE_MAX_ATTEMPTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ATTEMPTS)
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
    /// The pending checkpoints the capture took; each is published from
    /// the session disk by name.
    #[serde(default)]
    pub disk_pending: Vec<PendingDisk>,
    pub aux_bundles: Vec<AuxBundleRef>,
    pub stage: FinalizeStage,
    pub attempts: u32,
    /// Filled by the `DiskUploaded` leg.
    pub disk_manifest: Option<ManifestRef>,
    #[serde(default)]
    pub swap_manifest: Option<ManifestRef>,
    /// Filled by the `MemoryChunked` leg.
    pub memory_manifest: Option<ManifestRef>,
}

/// A pending checkpoint of one disk and the checkpoint it was taken
/// over. A pending checkpoint that is gone counts as published only when
/// the disk holds a newer checkpoint of that lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDisk {
    pub role: DiskRole,
    pub previous: ManifestRef,
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
/// `PooledBackend::destroy` (the full cleanup: egress unregister, volume
/// release, checkpoint-chain teardown); the simulator records the
/// call. Destroy is BEST-EFFORT for the finalize: `NotFound` and a gone
/// backend are success, and any other error is logged, never a leg
/// failure — the teardown reconcile backstops it.
#[async_trait]
pub trait EvictionSandbox: Send + Sync {
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
}

/// The owned, 'static bundle of everything the finalize job needs:
/// Arc-clones of the `PooledBackend`'s fields plus the
/// [`EvictionSandbox`] seam for the terminal destroy. `pub` with a
/// constructor so the cosimulator builds one over its own stores.
#[derive(Clone)]
pub struct EvictionFinalizer {
    pub(crate) volumes: Option<Arc<engram_host_core::SessionVolumes>>,
    pub(crate) chunk_store: Option<ChunkStore>,
    pub(crate) chunk_cache: Option<ChunkCache>,
    pub(crate) bundle_dir: PathBuf,
    pub(crate) bundle_file_ext: &'static str,
    /// `<work_dir>/checkpoints`: `records/` and `finalize/` live under it.
    pub(crate) checkpoint_dir: PathBuf,
    pub(crate) pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
    pub(crate) destroyer: Arc<dyn EvictionSandbox>,
    pub(crate) fs: Arc<dyn HostFs>,
    pub(crate) max_attempts: u32,
}

impl EvictionFinalizer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        volumes: Option<Arc<engram_host_core::SessionVolumes>>,
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
            volumes,
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

/// The startup volume reconcile: keep the volumes of every `live`
/// sandbox and of every sandbox a durable finalize record still
/// publishes from; delete the rest. `finalizer` is `None` when
/// checkpointing is off, so no record can exist.
pub async fn reconcile_volumes(
    volumes: &engram_host_core::SessionVolumes,
    finalizer: Option<&EvictionFinalizer>,
    mut live: Vec<SandboxId>,
) -> std::io::Result<engram_host_core::session_volumes::Report> {
    if let Some(f) = finalizer {
        let records = EvictionFinalizeRecord::load_all(f.fs.as_ref(), &f.finalize_dir()).await;
        live.extend(records.iter().map(|r| r.sandbox_id));
    }
    volumes.reconcile(&live).await
}

/// The disk leg: publish each pending checkpoint from the session disk.
/// A pending checkpoint that is gone was published by an earlier attempt
/// that died before the stage bump only if the disk's checkpoint is newer
/// than the one it was taken over; otherwise it was lost, and the leg
/// fails instead of answering with the older checkpoint.
async fn disk_leg_work(
    f: &EvictionFinalizer,
    record: &EvictionFinalizeRecord,
) -> Result<(Option<ManifestRef>, Option<ManifestRef>), SandboxError> {
    let start = crate::time_source::metrics_now();
    let mut root_manifest = None;
    let mut swap_manifest = None;
    if !record.disk_pending.is_empty() {
        let volumes = f.volumes.as_ref().ok_or_else(|| {
            SandboxError::Snapshot(
                "finalize has pending disk checkpoints but this host has no session disk".into(),
            )
        })?;
        for &PendingDisk { role, previous } in &record.disk_pending {
            let published = match volumes.pending(record.sandbox_id, role).await {
                Ok(pending) => volumes.publish(pending).await.map_err(|e| {
                    SandboxError::Snapshot(format!("eviction finalize: publish {role} disk: {e}"))
                })?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let current = volumes
                        .checkpoint_of(record.sandbox_id, role)
                        .await
                        .map_err(|e| {
                            SandboxError::Snapshot(format!(
                                "eviction finalize: {role} disk has no pending checkpoint and no checkpoint: {e}"
                            ))
                        })?;
                    if current.manifest_id != previous.manifest_id
                        || current.version <= previous.version
                    {
                        return Err(SandboxError::Snapshot(format!(
                            "eviction finalize: the pending {role} disk checkpoint is gone and was \
                             never published (the disk holds {current}, taken over {previous})"
                        )));
                    }
                    current
                }
                Err(e) => {
                    return Err(SandboxError::Snapshot(format!(
                        "eviction finalize: open pending {role} disk checkpoint: {e}"
                    )));
                }
            };
            match role {
                DiskRole::Root => root_manifest = Some(published),
                DiskRole::Swap => swap_manifest = Some(published),
            }
        }
    }
    metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => "disk")
        .record(start.elapsed().as_secs_f64());
    Ok((root_manifest, swap_manifest))
}

/// Publish deterministic finalize content without conflating an occupied
/// version with an identical prior publish.
async fn publish_manifest_collision_safe(
    chunk_store: &ChunkStore,
    deterministic_ref: ManifestRef,
    manifest: &Manifest,
) -> Result<ManifestRef, engram_chunk_store::ChunkStoreError> {
    const MAX_RETRIES: u32 = 32;
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
                if attempts >= MAX_RETRIES {
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
        swap_manifest: record.swap_manifest,
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
                "eviction finalize: best-effort destroy failed; the teardown reconcile backstops it",
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

/// Persist the `DiskUploaded` bump (with the resolved refs), rolling
/// back the in-RAM mutations on a failed persist so a retry never
/// believes an un-persisted stage is durable.
async fn persist_disk_bump(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
    manifests: (Option<ManifestRef>, Option<ManifestRef>),
) -> Result<(), SandboxError> {
    let prev_stage = record.stage;
    let prev_disk_manifest = record.disk_manifest;
    let prev_swap_manifest = record.swap_manifest;
    (record.disk_manifest, record.swap_manifest) = manifests;
    record.stage = FinalizeStage::DiskUploaded;
    if let Err(e) = record.persist(f.fs.as_ref(), &f.finalize_dir()).await {
        record.stage = prev_stage;
        record.disk_manifest = prev_disk_manifest;
        record.swap_manifest = prev_swap_manifest;
        return Err(SandboxError::Snapshot(format!(
            "persist finalize record: {e}"
        )));
    }
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

/// The disk and memory legs are independent idempotent publishes, so
/// they overlap when both are pending. The durable ladder is preserved
/// by persisting the bumps in order after the join.
async fn run_eviction_finalize_once(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    match record.stage {
        FinalizeStage::Captured => {
            let (disk_res, mem_res) =
                tokio::join!(disk_leg_work(f, record), memory_leg_work(f, record));
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

/// One redrive attempt: a sleep-free pass over the legs plus the
/// retry/quarantine verdict. `pub` so the cosimulator drives the exact
/// production loop body per tick; [`run_eviction_finalize`] only adds
/// the backoff sleep.
pub async fn run_eviction_finalize_attempt(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> FinalizeAttempt {
    match run_eviction_finalize_once(f, record).await {
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

/// The finalize job: spawned by `snapshot_begin` holding the sandbox's
/// capture lock for the job's lifetime, and by `resume_pending_finalizes`
/// at startup for each redriven record.
pub(crate) async fn run_eviction_finalize(
    f: EvictionFinalizer,
    mut record: EvictionFinalizeRecord,
    _capture_guard: tokio::sync::OwnedMutexGuard<()>,
) {
    loop {
        match run_eviction_finalize_attempt(&f, &mut record).await {
            FinalizeAttempt::Completed | FinalizeAttempt::Quarantined => return,
            FinalizeAttempt::RetryAfter(backoff) => tokio::time::sleep(backoff).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_torn_record_is_skipped_and_a_whole_one_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let record = EvictionFinalizeRecord {
            snapshot_id: SnapshotId::new(),
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            image_version: "v".into(),
            size_bytes: 0,
            paused_at: chrono::DateTime::UNIX_EPOCH,
            captured_at: chrono::DateTime::UNIX_EPOCH,
            dest: tmp.path().join("capture"),
            chain_prev_ref: None,
            disk_pending: vec![
                PendingDisk {
                    role: DiskRole::Root,
                    previous: ManifestRef::new(),
                },
                PendingDisk {
                    role: DiskRole::Swap,
                    previous: ManifestRef::new(),
                },
            ],
            aux_bundles: Vec::new(),
            stage: FinalizeStage::default(),
            attempts: 0,
            disk_manifest: None,
            swap_manifest: None,
            memory_manifest: None,
        };
        let body = serde_json::to_string(&record).unwrap();
        let bytes = crate::durable_envelope::seal(&record.snapshot_id.to_string(), &body);
        let path = tmp.path().join(format!("{}.json", record.snapshot_id));
        for end in 0..bytes.len() {
            std::fs::write(&path, &bytes[..end]).unwrap();
            assert!(
                EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, tmp.path())
                    .await
                    .is_empty()
            );
        }
        std::fs::write(&path, &bytes).unwrap();
        std::fs::write(tmp.path().join("torn.partial"), b"{").unwrap();
        let loaded = EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, tmp.path()).await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].disk_pending, record.disk_pending);
    }

    struct NoDestroy;

    #[async_trait]
    impl EvictionSandbox for NoDestroy {
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    /// A finalizer over file-backed session volumes holding one sandbox
    /// with an empty root volume.
    async fn finalizer_with_a_root(
        tmp: &Path,
        sandbox: SandboxId,
    ) -> (EvictionFinalizer, Arc<engram_host_core::SessionVolumes>) {
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.join("blob")),
        );
        let store = ChunkStore::new(blob);
        let disk: Arc<dyn engram_host_core::SessionDisk> =
            Arc::new(engram_host_core::FileDisk::new(tmp.join("volumes"), 64 * 1024).unwrap());
        let volumes = Arc::new(engram_host_core::SessionVolumes::new(disk, store.clone()));
        volumes
            .provision(
                sandbox,
                engram_host_core::session_volumes::Source::Empty {
                    size_bytes: 128 * 1024,
                },
                None,
            )
            .await
            .unwrap();
        let finalizer = EvictionFinalizer::new(
            Some(volumes.clone()),
            Some(store),
            None,
            tmp.join("bundles"),
            "bundle",
            tmp.join("checkpoints"),
            Arc::new(DashMap::new()),
            Arc::new(NoDestroy),
            Arc::new(engram_host_core::TokioFs),
            3,
        );
        (finalizer, volumes)
    }

    fn record_for(sandbox: SandboxId, disk_pending: Vec<PendingDisk>) -> EvictionFinalizeRecord {
        EvictionFinalizeRecord {
            snapshot_id: SnapshotId::new(),
            session_id: SessionId::new(),
            sandbox_id: sandbox,
            image_version: "v".into(),
            size_bytes: 0,
            paused_at: chrono::DateTime::UNIX_EPOCH,
            captured_at: chrono::DateTime::UNIX_EPOCH,
            dest: PathBuf::from("/nonexistent"),
            chain_prev_ref: None,
            disk_pending,
            aux_bundles: Vec::new(),
            stage: FinalizeStage::default(),
            attempts: 0,
            disk_manifest: None,
            swap_manifest: None,
            memory_manifest: None,
        }
    }

    /// An attempt that published and died before the stage bump leaves
    /// no pending checkpoint; the redrive answers with the published one.
    #[tokio::test]
    async fn a_redrive_after_the_publish_answers_with_the_published_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let pending = volumes.take(sandbox, DiskRole::Root).await.unwrap();
        let entry = PendingDisk {
            role: DiskRole::Root,
            previous: pending.previous(),
        };
        let published = volumes.publish(pending).await.unwrap();

        let (root, swap) = disk_leg_work(&finalizer, &record_for(sandbox, vec![entry]))
            .await
            .unwrap();
        assert_eq!(root, Some(published));
        assert_eq!(swap, None);
    }

    /// A pending checkpoint lost without a publish leaves only the
    /// checkpoint it was taken over. The leg fails rather than answer
    /// with it, which would drop every write since that checkpoint.
    #[tokio::test]
    async fn a_lost_pending_checkpoint_fails_the_disk_leg() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let pending = volumes.take(sandbox, DiskRole::Root).await.unwrap();
        let entry = PendingDisk {
            role: DiskRole::Root,
            previous: pending.previous(),
        };
        volumes.discard(pending).await.unwrap();

        let Err(SandboxError::Snapshot(message)) =
            disk_leg_work(&finalizer, &record_for(sandbox, vec![entry])).await
        else {
            panic!("a lost pending checkpoint must fail the disk leg");
        };
        assert!(message.contains("never published"), "{message}");
    }
}
