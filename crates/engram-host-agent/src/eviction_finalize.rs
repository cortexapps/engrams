//! Host-durable eviction finalize (issue #529).
//!
//! Once `snapshot_begin` returns, the finalize is a host-owned job that
//! is a pure function of durable artifacts: the pending disk checkpoints
//! on the session disk, found by name, and the FC staging dir (`dest`).
//! It never touches the sandbox, the coordinator, or an in-RAM map it
//! did not rebuild from disk. A host-agent restart re-drives it from
//! [`EvictionFinalizeRecord::load_all`]; nothing but node loss loses it.
//!
//! `snapshot_begin` persists the record at `Captured`. A pass runs the
//! shared snapshot post phase ([`crate::snapshot_post`]), persists the
//! stage the legs reached, and at `BlobsUploaded` writes the terminal
//! checkpoint record. Each leg is idempotent, so a redrive re-runs
//! whatever the persisted record has not passed.
use engram_core::types::snapshot::DiskRole;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_chunk_store::ChunkCache;
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
    /// The extents of `memory.diff` after the capture's sync. `None` for
    /// a Full capture, or for a record from a build that did not record
    /// them: that diff publishes unchecked.
    #[serde(default)]
    pub diff_extents: Option<DiffExtents>,
}

/// The data extents of a `memory.diff`: the range count and byte sum. A
/// dirty page that a crash lost before writeback reads as a hole, so a
/// diff whose extents differ from the ones its capture recorded lost
/// pages, and publishing it would keep the previous chunk for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffExtents {
    pub ranges: u64,
    pub bytes: u64,
}

impl DiffExtents {
    pub(crate) fn of(ranges: &[(u64, u64)]) -> Self {
        Self {
            ranges: ranges.len() as u64,
            bytes: ranges.iter().map(|(_, len)| len).sum(),
        }
    }

    async fn read(diff: &Path) -> std::io::Result<Self> {
        let diff = diff.to_path_buf();
        let ranges = tokio::task::spawn_blocking(move || crate::checkpoint::dirty_ranges(&diff))
            .await
            .map_err(std::io::Error::other)??;
        Ok(Self::of(&ranges))
    }
}

impl std::fmt::Display for DiffExtents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ranges of {} bytes", self.ranges, self.bytes)
    }
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
    /// Make the staging dir the redrive's input: fsync every capture
    /// file, record the diff's extents, then persist the record at
    /// `Captured` into `dir`. No record exists unless the files reached
    /// the disk first.
    pub async fn persist_captured(&mut self, fs: &dyn HostFs, dir: &Path) -> std::io::Result<()> {
        crate::snapshot_post::sync_capture(fs, &self.dest, crate::snapshot_post::SyncScope::All)
            .await?;
        if self.chain_prev_ref.is_some() {
            self.diff_extents = Some(DiffExtents::read(&self.dest.join("memory.diff")).await?);
        }
        self.persist(fs, dir).await
    }

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
    pub(crate) chunks: Option<crate::snapshot_post::Chunks>,
    pub(crate) chunk_cache: Option<ChunkCache>,
    pub(crate) bundle_dir: PathBuf,
    pub(crate) bundle_file_ext: &'static str,
    /// `<work_dir>/checkpoints`: `records/` and `finalize/` live under it.
    pub(crate) checkpoint_dir: PathBuf,
    /// Signalled after the terminal checkpoint record lands in `records/`.
    pub(crate) checkpoint_recorded: Arc<tokio::sync::Notify>,
    pub(crate) pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
    pub(crate) destroyer: Arc<dyn EvictionSandbox>,
    pub(crate) fs: Arc<dyn HostFs>,
    pub(crate) max_attempts: u32,
}

impl EvictionFinalizer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        volumes: Option<Arc<engram_host_core::SessionVolumes>>,
        fill: Option<engram_fill::FillEngine>,
        chunk_cache: Option<ChunkCache>,
        bundle_dir: PathBuf,
        bundle_file_ext: &'static str,
        checkpoint_dir: PathBuf,
        checkpoint_recorded: Arc<tokio::sync::Notify>,
        pending_finalizes: Arc<DashMap<SandboxId, SnapshotId>>,
        destroyer: Arc<dyn EvictionSandbox>,
        fs: Arc<dyn HostFs>,
        max_attempts: u32,
    ) -> Self {
        Self {
            volumes,
            chunks: fill.map(crate::snapshot_post::Chunks::new),
            chunk_cache,
            bundle_dir,
            bundle_file_ext,
            checkpoint_dir,
            checkpoint_recorded,
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

impl EvictionFinalizer {
    fn post_phase(&self) -> crate::snapshot_post::PostPhase<'_> {
        crate::snapshot_post::PostPhase {
            volumes: self.volumes.as_deref(),
            chunks: self.chunks.as_ref(),
            chunk_cache: self.chunk_cache.as_ref(),
            bundle_dir: &self.bundle_dir,
            bundle_file_ext: self.bundle_file_ext,
            fs: self.fs.as_ref(),
        }
    }
}

/// Run the legs the record has not passed, then persist what the legs
/// that succeeded published, even when another leg failed: a redrive
/// after a failed blob upload must not chunk the whole memory file
/// again. The stage only moves up the ladder, so a stage that passes a
/// leg also passes every leg below it; a memory manifest recorded below
/// `MemoryChunked` marks the memory leg done for this build. The memory
/// file is retired only once a durable stage passes `MemoryChunked`, the
/// mark every build reads.
async fn run_post_phase(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    use crate::snapshot_post::{Disks, Memory, Work};
    let memory_published =
        record.stage >= FinalizeStage::MemoryChunked || record.memory_manifest.is_some();
    let memory = (!memory_published).then_some(match record.chain_prev_ref {
        Some(prev_ref) => Memory::Diff {
            prev_ref,
            prev: None,
            extents: record.diff_extents,
        },
        None => Memory::Full,
    });
    let disks = (record.stage == FinalizeStage::Captured).then(|| Disks::Named {
        sandbox: record.sandbox_id,
        pending: record.disk_pending.clone(),
    });
    let ran = f
        .post_phase()
        .run_legs(Work {
            snapshot_id: record.snapshot_id,
            dest: &record.dest,
            disks,
            memory,
            blobs: true,
            trace_lineage: Some(record.session_id),
            aux_bundles: &record.aux_bundles,
            source: "evict_finalize",
        })
        .await?;
    for (stage, seconds) in [
        ("disk", ran.seconds.disk),
        ("memory", ran.seconds.memory),
        ("blobs", ran.seconds.blobs),
    ] {
        if let Some(seconds) = seconds {
            metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => stage)
                .record(seconds);
        }
    }

    let before = record.clone();
    let mut error = None;
    let disks_done = match ran.disks {
        Ok((root, swap)) => {
            if record.stage == FinalizeStage::Captured {
                record.disk_manifest = root;
                record.swap_manifest = swap;
            }
            true
        }
        Err(e) => {
            error.get_or_insert(e);
            false
        }
    };
    let memory_done = match ran.memory {
        Ok(published) => {
            if let Some(memory) = published {
                record.memory_manifest = Some(memory.manifest_ref);
            }
            true
        }
        Err(e) => {
            error.get_or_insert(e);
            false
        }
    };
    let blobs_done = match ran.blobs {
        Ok(_) => true,
        Err(e) => {
            error.get_or_insert(e);
            false
        }
    };
    let reached = match (disks_done, memory_done, blobs_done) {
        (true, true, true) => FinalizeStage::BlobsUploaded,
        (true, true, false) => FinalizeStage::MemoryChunked,
        (true, false, _) => FinalizeStage::DiskUploaded,
        (false, _, _) => FinalizeStage::Captured,
    };
    record.stage = record.stage.max(reached);
    let changed = record.stage != before.stage || record.memory_manifest != before.memory_manifest;
    if changed {
        if let Err(e) = record.persist(f.fs.as_ref(), &f.finalize_dir()).await {
            // A retry must never believe an un-persisted stage is durable.
            *record = before;
            return Err(SandboxError::Snapshot(format!(
                "persist finalize record: {e}"
            )));
        }
    }
    // The capture's sync covered the memory file, so a failed retire
    // only leaks it.
    if before.stage < FinalizeStage::MemoryChunked && record.stage >= FinalizeStage::MemoryChunked {
        let file = record.dest.join(match record.chain_prev_ref {
            Some(_) => "memory.diff",
            None => "memory.bin",
        });
        if let Err(e) = crate::snapshot_post::retire_consumed(f.fs.clone(), file).await {
            tracing::warn!(snapshot_id = %record.snapshot_id, error = %e, "retire of a consumed memory file failed");
        }
    }
    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
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
    f.checkpoint_recorded.notify_one();

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

async fn run_eviction_finalize_once(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    if record.stage < FinalizeStage::BlobsUploaded {
        run_post_phase(f, record).await?;
    }
    run_terminal(f, record).await
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
            diff_extents: None,
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
        finalizer_over(tmp, sandbox, blob).await
    }

    async fn finalizer_over(
        tmp: &Path,
        sandbox: SandboxId,
        blob: Arc<dyn engram_core::traits::BlobStorage>,
    ) -> (EvictionFinalizer, Arc<engram_host_core::SessionVolumes>) {
        let fill = engram_fill::FillEngine::new(
            engram_chunk_store::ChunkStore::new(blob),
            engram_fill::Config::default(),
        );
        let disk: Arc<dyn engram_host_core::SessionDisk> =
            Arc::new(engram_host_core::FileDisk::new(tmp.join("volumes"), 64 * 1024).unwrap());
        let volumes = Arc::new(engram_host_core::SessionVolumes::new(disk, fill.clone()));
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
            Some(fill),
            None,
            tmp.join("bundles"),
            "bundle",
            tmp.join("checkpoints"),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(DashMap::new()),
            Arc::new(NoDestroy),
            Arc::new(engram_host_core::TokioFs),
            3,
        );
        (finalizer, volumes)
    }

    async fn publish_disks(
        f: &EvictionFinalizer,
        sandbox: SandboxId,
        pending: Vec<PendingDisk>,
    ) -> Result<(Option<ManifestRef>, Option<ManifestRef>), SandboxError> {
        let published = f
            .post_phase()
            .run(crate::snapshot_post::Work {
                snapshot_id: SnapshotId::new(),
                dest: Path::new("/nonexistent"),
                disks: Some(crate::snapshot_post::Disks::Named { sandbox, pending }),
                memory: None,
                blobs: false,
                trace_lineage: None,
                aux_bundles: &[],
                source: "test",
            })
            .await?;
        Ok((published.root, published.swap))
    }

    /// A staging dir holding what a Full FC capture leaves: a memory
    /// file, a state file and a sidecar.
    fn full_capture_dest(tmp: &Path) -> PathBuf {
        let dest = tmp.join("capture");
        std::fs::create_dir_all(&dest).unwrap();
        let mut memory = vec![0u8; 4 << 20];
        memory[..5].copy_from_slice(b"guest");
        std::fs::write(dest.join("memory.bin"), memory).unwrap();
        std::fs::write(dest.join("state.bin"), b"vmstate").unwrap();
        std::fs::write(dest.join("manifest.json"), b"{}").unwrap();
        dest
    }

    fn captured_record(
        sandbox: SandboxId,
        dest: PathBuf,
        stage: FinalizeStage,
    ) -> EvictionFinalizeRecord {
        EvictionFinalizeRecord {
            snapshot_id: SnapshotId::new(),
            session_id: SessionId::new(),
            sandbox_id: sandbox,
            image_version: "v".into(),
            size_bytes: 0,
            paused_at: chrono::DateTime::UNIX_EPOCH,
            captured_at: chrono::DateTime::UNIX_EPOCH,
            dest,
            chain_prev_ref: None,
            disk_pending: Vec::new(),
            aux_bundles: Vec::new(),
            stage,
            attempts: 0,
            disk_manifest: None,
            swap_manifest: None,
            memory_manifest: None,
            diff_extents: None,
        }
    }

    /// One pass runs every leg, persists one bump to `BlobsUploaded`,
    /// and writes the terminal checkpoint record with what it published.
    #[tokio::test]
    async fn one_pass_publishes_every_leg_and_records_the_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let pending = volumes.take(sandbox, DiskRole::Root).await.unwrap();
        let entry = PendingDisk {
            role: DiskRole::Root,
            previous: pending.previous(),
        };
        drop(pending);
        let dest = full_capture_dest(tmp.path());
        let mut record = captured_record(sandbox, dest.clone(), FinalizeStage::Captured);
        record.disk_pending = vec![entry];

        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(matches!(outcome, FinalizeAttempt::Completed), "{outcome:?}");
        assert_eq!(record.stage, FinalizeStage::BlobsUploaded);
        let disk = record.disk_manifest.expect("the disk leg ran");
        assert!(disk.version > entry.previous.version);
        let memory = record.memory_manifest.expect("the memory leg ran");

        let checkpoints =
            CheckpointRecord::load_all(&engram_host_core::TokioFs, &finalizer.records_dir()).await;
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].memory_manifest, Some(memory));
        assert_eq!(checkpoints[0].disk_manifest, Some(disk));
        assert!(
            EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, &finalizer.finalize_dir())
                .await
                .is_empty(),
            "the terminal leg deletes the finalize record",
        );
        let sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dest.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            sidecar["memory_manifest"],
            serde_json::to_value(memory).unwrap()
        );
        let blob = finalizer.chunks.as_ref().unwrap().store.blob_storage();
        let state_key = engram_chunk_store::snapshot_blob::state_blob_key(record.snapshot_id);
        assert_eq!(&blob.get(&state_key).await.unwrap()[..], b"vmstate");
    }

    /// A record an earlier build left at `DiskUploaded` runs only the
    /// memory and blob legs: its pending disk checkpoint is gone, so a
    /// disk leg would fail.
    #[tokio::test]
    async fn a_disk_uploaded_record_from_an_earlier_build_skips_the_disk_leg() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let dest = full_capture_dest(tmp.path());
        let mut record = captured_record(sandbox, dest, FinalizeStage::DiskUploaded);
        let published_disk = ManifestRef::new();
        record.disk_manifest = Some(published_disk);
        record.disk_pending = vec![PendingDisk {
            role: DiskRole::Root,
            previous: ManifestRef::new(),
        }];

        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(matches!(outcome, FinalizeAttempt::Completed), "{outcome:?}");
        assert_eq!(record.disk_manifest, Some(published_disk));
        assert!(record.memory_manifest.is_some());
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

        let (root, swap) = publish_disks(&finalizer, sandbox, vec![entry])
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
            publish_disks(&finalizer, sandbox, vec![entry]).await
        else {
            panic!("a lost pending checkpoint must fail the disk leg");
        };
        assert!(message.contains("never published"), "{message}");
    }

    /// Run redrive attempts until the record leaves the loop.
    async fn redrive_to_the_end(
        f: &EvictionFinalizer,
        record: &mut EvictionFinalizeRecord,
    ) -> FinalizeAttempt {
        loop {
            match run_eviction_finalize_attempt(f, record).await {
                FinalizeAttempt::RetryAfter(_) => continue,
                outcome => return outcome,
            }
        }
    }

    async fn checkpoints(f: &EvictionFinalizer) -> Vec<CheckpointRecord> {
        CheckpointRecord::load_all(&engram_host_core::TokioFs, &f.records_dir()).await
    }

    /// A sidecar shaped like the FC backend's, without a trailing newline:
    /// every strict prefix of it is invalid JSON.
    fn fc_sidecar() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "sandbox_id": "s-old",
            "format": "fc",
            "memory_manifest": null,
        }))
        .unwrap()
    }

    /// A capture with a memory file also wrote the sidecar, so a staging
    /// dir without it lost the file. The redrive fails instead of
    /// recording a checkpoint whose state and sidecar blobs were never
    /// uploaded.
    #[tokio::test]
    async fn a_redrive_over_a_staging_dir_without_its_sidecar_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let dest = full_capture_dest(tmp.path());
        std::fs::remove_file(dest.join("manifest.json")).unwrap();
        let mut record = captured_record(sandbox, dest, FinalizeStage::Captured);

        let outcome = redrive_to_the_end(&finalizer, &mut record).await;
        assert!(
            matches!(outcome, FinalizeAttempt::Quarantined),
            "{outcome:?}"
        );
        assert!(checkpoints(&finalizer).await.is_empty());
    }

    #[tokio::test]
    async fn a_redrive_over_a_zero_length_sidecar_quarantines() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let dest = full_capture_dest(tmp.path());
        std::fs::write(dest.join("manifest.json"), b"").unwrap();
        let mut record = captured_record(sandbox, dest.clone(), FinalizeStage::Captured);

        let outcome = redrive_to_the_end(&finalizer, &mut record).await;
        assert!(
            matches!(outcome, FinalizeAttempt::Quarantined),
            "{outcome:?}"
        );
        assert!(checkpoints(&finalizer).await.is_empty());
        let failed = EvictionFinalizeRecord::load_all(
            &engram_host_core::TokioFs,
            &finalizer.finalize_dir().join("failed"),
        )
        .await;
        assert_eq!(failed.len(), 1);
        assert!(!dest.exists(), "the quarantine frees the staging dir");
    }

    #[tokio::test]
    async fn a_redrive_over_a_sidecar_torn_at_every_offset_never_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let dest = full_capture_dest(tmp.path());
        std::fs::write(dest.join("memory.bin"), finalize_memory()).unwrap();
        let sidecar = fc_sidecar();
        for end in 0..sidecar.len() {
            std::fs::write(dest.join("manifest.json"), &sidecar[..end]).unwrap();
            let mut record = captured_record(sandbox, dest.clone(), FinalizeStage::Captured);
            let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
            assert!(
                matches!(outcome, FinalizeAttempt::RetryAfter(_)),
                "torn at {end}: {outcome:?}"
            );
            assert!(checkpoints(&finalizer).await.is_empty(), "torn at {end}");
        }
        std::fs::write(dest.join("manifest.json"), &sidecar).unwrap();
        let mut record = captured_record(sandbox, dest, FinalizeStage::Captured);
        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(matches!(outcome, FinalizeAttempt::Completed), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_redrive_over_a_zero_length_state_bin_never_records_the_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let dest = full_capture_dest(tmp.path());
        std::fs::write(dest.join("state.bin"), b"").unwrap();
        let mut record = captured_record(sandbox, dest, FinalizeStage::Captured);

        let outcome = redrive_to_the_end(&finalizer, &mut record).await;
        assert!(
            matches!(outcome, FinalizeAttempt::Quarantined),
            "{outcome:?}"
        );
        assert!(checkpoints(&finalizer).await.is_empty());
    }

    /// Every finalize record comes from a capture that wrote guest
    /// memory, so a staging dir that lost its memory file, its state file
    /// or its whole entry fails the redrive instead of recording a
    /// checkpoint with no memory.
    #[tokio::test]
    async fn a_redrive_over_a_full_capture_that_lost_its_memory_fails() {
        for lost in [
            &["memory.bin"][..],
            &["memory.bin", "state.bin"],
            &["memory.bin", "state.bin", "manifest.json"],
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let sandbox = SandboxId::new();
            let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
            let dest = full_capture_dest(tmp.path());
            for name in lost {
                std::fs::remove_file(dest.join(name)).unwrap();
            }
            if lost.len() == 3 {
                std::fs::remove_dir(&dest).unwrap();
            }
            let mut record = captured_record(sandbox, dest, FinalizeStage::Captured);

            let outcome = redrive_to_the_end(&finalizer, &mut record).await;
            assert!(
                matches!(outcome, FinalizeAttempt::Quarantined),
                "lost {lost:?}: {outcome:?}"
            );
            assert!(checkpoints(&finalizer).await.is_empty(), "lost {lost:?}");
        }
    }

    /// A failed `state.bin` upload keeps what the other legs published:
    /// the record durably reaches `MemoryChunked`, and the redrive uploads
    /// the blobs without chunking the memory file again.
    #[tokio::test]
    async fn a_failed_state_upload_keeps_the_chunked_memory_for_the_redrive() {
        use engram_testkit::storage::{
            FaultPlan, FaultyBlobStorage, InjectedError, KeyMatch, PutFault, PutFaultKind, When,
        };
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let dest = full_capture_dest(tmp.path());
        let mut record = captured_record(sandbox, dest.clone(), FinalizeStage::Captured);
        let plan = FaultPlan::new().with_put(PutFault {
            key: KeyMatch::Exact(engram_chunk_store::snapshot_blob::state_blob_key(
                record.snapshot_id,
            )),
            when: When::Nth(1),
            kind: PutFaultKind::FailBeforeWrite(InjectedError::Sdk("injected".into())),
        });
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(FaultyBlobStorage::new(
            Arc::new(engram_storage_local::LocalBlobStorage::new(
                tmp.path().join("blob"),
            )),
            plan,
        ));
        let (finalizer, _volumes) = finalizer_over(tmp.path(), sandbox, blob).await;

        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(
            matches!(outcome, FinalizeAttempt::RetryAfter(_)),
            "{outcome:?}"
        );
        assert_eq!(record.stage, FinalizeStage::MemoryChunked);
        let memory = record.memory_manifest.expect("the memory leg published");
        let durable =
            EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, &finalizer.finalize_dir())
                .await;
        assert_eq!(durable.len(), 1);
        assert_eq!(durable[0].stage, FinalizeStage::MemoryChunked);
        assert_eq!(durable[0].memory_manifest, Some(memory));
        assert!(
            !dest.join("memory.bin").exists(),
            "the memory file is retired"
        );

        let mut redriven = durable.into_iter().next().unwrap();
        let outcome = run_eviction_finalize_attempt(&finalizer, &mut redriven).await;
        assert!(matches!(outcome, FinalizeAttempt::Completed), "{outcome:?}");
        let checkpoints = checkpoints(&finalizer).await;
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].memory_manifest, Some(memory));
    }

    /// A memory leg that succeeds beside a failed disk leg is recorded
    /// without a stage bump, so the redrive skips it but every build still
    /// finds the memory file.
    #[tokio::test]
    async fn a_failed_disk_leg_keeps_the_memory_manifest_and_the_memory_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let pending = volumes.take(sandbox, DiskRole::Root).await.unwrap();
        let entry = PendingDisk {
            role: DiskRole::Root,
            previous: pending.previous(),
        };
        volumes.discard(pending).await.unwrap();
        let dest = full_capture_dest(tmp.path());
        let mut record = captured_record(sandbox, dest.clone(), FinalizeStage::Captured);
        record.disk_pending = vec![entry];

        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(
            matches!(outcome, FinalizeAttempt::RetryAfter(_)),
            "{outcome:?}"
        );
        assert_eq!(record.stage, FinalizeStage::Captured);
        let memory = record.memory_manifest.expect("the memory leg published");
        assert!(dest.join("memory.bin").exists());
        let durable =
            EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, &finalizer.finalize_dir())
                .await;
        assert_eq!(durable[0].memory_manifest, Some(memory));
    }

    /// 64 KiB of non-zero bytes: small, so a test can run many passes.
    fn finalize_memory() -> Vec<u8> {
        (0..64 * 1024).map(|i| (i % 251 + 1) as u8).collect()
    }

    // The diff-extents tests need a filesystem that reports holes, as the
    // ext4 under a Firecracker host does; APFS reports a sparse file as
    // one extent.
    #[cfg(target_os = "linux")]
    const DIFF_LEN: u64 = 4 << 20;
    #[cfg(target_os = "linux")]
    const DIRTY: [u64; 2] = [0, 2 << 20];

    /// Write a sparse `memory.diff` of `DIFF_LEN` bytes with 64 KiB of
    /// dirty pages at each of `dirty`.
    #[cfg(target_os = "linux")]
    fn write_diff(path: &Path, dirty: &[u64]) {
        use std::io::{Seek, SeekFrom, Write};
        let _ = std::fs::remove_file(path);
        let mut file = std::fs::File::create(path).unwrap();
        file.set_len(DIFF_LEN).unwrap();
        for &offset in dirty {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&[0xA5; 64 * 1024]).unwrap();
        }
        file.sync_all().unwrap();
    }

    /// A crash before writeback leaves a hole where a dirty page was,
    /// which reads as a clean page. The capture recorded the diff's
    /// extents, so the redrive refuses the torn file instead of
    /// publishing the previous chunk for the lost page.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_redrive_over_a_diff_whose_extents_differ_from_the_record_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxId::new();
        let (finalizer, _volumes) = finalizer_with_a_root(tmp.path(), sandbox).await;
        let store = finalizer.chunks.clone().unwrap().store;
        let prev_path = tmp.path().join("prev.bin");
        std::fs::write(
            &prev_path,
            (0..DIFF_LEN)
                .map(|i| (i % 251 + 1) as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let prev_ref =
            crate::snapshot_post::chunk_memory_to_store(&store, &prev_path, None, "test")
                .await
                .unwrap();

        let dest = tmp.path().join("capture");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("state.bin"), b"vmstate").unwrap();
        std::fs::write(dest.join("manifest.json"), fc_sidecar()).unwrap();
        let diff = dest.join("memory.diff");
        write_diff(&diff, &DIRTY);
        let mut record = captured_record(sandbox, dest, FinalizeStage::Captured);
        record.chain_prev_ref = Some(prev_ref);
        record
            .persist_captured(&engram_host_core::TokioFs, &finalizer.finalize_dir())
            .await
            .unwrap();
        let recorded = record
            .diff_extents
            .expect("a diff capture records its extents");

        write_diff(&diff, &DIRTY[..1]);
        assert_ne!(DiffExtents::read(&diff).await.unwrap(), recorded);
        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(
            matches!(outcome, FinalizeAttempt::RetryAfter(_)),
            "{outcome:?}"
        );
        assert_eq!(record.memory_manifest, None);
        assert!(checkpoints(&finalizer).await.is_empty());

        write_diff(&diff, &DIRTY);
        let outcome = run_eviction_finalize_attempt(&finalizer, &mut record).await;
        assert!(matches!(outcome, FinalizeAttempt::Completed), "{outcome:?}");
    }

    /// A record from a build that did not record the extents redrives
    /// as before.
    #[tokio::test]
    async fn a_record_without_diff_extents_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let record = captured_record(
            SandboxId::new(),
            tmp.path().join("c"),
            FinalizeStage::Captured,
        );
        let mut body = serde_json::to_value(&record).unwrap();
        body.as_object_mut().unwrap().remove("diff_extents");
        let body = serde_json::to_string(&body).unwrap();
        let id = record.snapshot_id.to_string();
        std::fs::write(
            tmp.path().join(format!("{id}.json")),
            crate::durable_envelope::seal(&id, &body),
        )
        .unwrap();
        let loaded = EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, tmp.path()).await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].diff_extents, None);
    }

    /// A [`HostFs`] over the real filesystem that logs each op and can
    /// fail the sync of one file name.
    #[derive(Default)]
    struct LoggingFs {
        ops: parking_lot::Mutex<Vec<(&'static str, PathBuf)>>,
        fail_sync_of: Option<&'static str>,
    }

    impl LoggingFs {
        fn ops(&self) -> Vec<(&'static str, PathBuf)> {
            self.ops.lock().clone()
        }
        fn log(&self, op: &'static str, path: &Path) {
            self.ops.lock().push((op, path.to_path_buf()));
        }
    }

    #[async_trait]
    impl HostFs for LoggingFs {
        async fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            self.log("write", path);
            engram_host_core::TokioFs.write(path, bytes).await
        }
        async fn sync_file(&self, path: &Path) -> std::io::Result<()> {
            self.log("sync_file", path);
            if self.fail_sync_of.is_some_and(|name| path.ends_with(name)) {
                return Err(std::io::Error::other("injected sync failure"));
            }
            engram_host_core::TokioFs.sync_file(path).await
        }
        async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.log("rename", to);
            engram_host_core::TokioFs.rename(from, to).await
        }
        async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
            self.log("sync_dir", dir);
            engram_host_core::TokioFs.sync_dir(dir).await
        }
        async fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
            engram_host_core::TokioFs.read(path).await
        }
        async fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
            engram_host_core::TokioFs.read_dir(dir).await
        }
        async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
            self.log("remove_file", path);
            engram_host_core::TokioFs.remove_file(path).await
        }
        async fn create_dir(&self, dir: &Path) -> std::io::Result<()> {
            engram_host_core::TokioFs.create_dir(dir).await
        }
        async fn remove_dir(&self, dir: &Path) -> std::io::Result<()> {
            engram_host_core::TokioFs.remove_dir(dir).await
        }
    }

    /// Every capture file, the sidecar too, and the staging dir reach the
    /// disk before the record that names them is written.
    #[tokio::test]
    async fn a_captured_record_is_written_only_after_every_capture_file_is_synced() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = full_capture_dest(tmp.path());
        let fs = LoggingFs::default();
        let mut record = captured_record(SandboxId::new(), dest.clone(), FinalizeStage::Captured);
        record
            .persist_captured(&fs, &tmp.path().join("finalize"))
            .await
            .unwrap();

        let ops = fs.ops();
        let first_write = ops.iter().position(|(op, _)| *op == "write").unwrap();
        let synced: Vec<_> = ops[..first_write]
            .iter()
            .filter(|(op, _)| *op == "sync_file")
            .map(|(_, path)| path.clone())
            .collect();
        for name in ["state.bin", "memory.bin", "manifest.json"] {
            assert!(
                synced.contains(&dest.join(name)),
                "{name} synced before the record: {ops:?}"
            );
        }
        assert!(ops[..first_write].contains(&("sync_dir", dest)), "{ops:?}");
    }

    #[tokio::test]
    async fn a_failed_capture_sync_writes_no_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = full_capture_dest(tmp.path());
        let fs = LoggingFs {
            fail_sync_of: Some("memory.bin"),
            ..Default::default()
        };
        let finalize_dir = tmp.path().join("finalize");
        let mut record = captured_record(SandboxId::new(), dest, FinalizeStage::Captured);
        assert!(record.persist_captured(&fs, &finalize_dir).await.is_err());
        assert!(!fs.ops().iter().any(|(op, _)| *op == "write"));
        assert!(
            EvictionFinalizeRecord::load_all(&engram_host_core::TokioFs, &finalize_dir)
                .await
                .is_empty()
        );
    }

    /// A File restore uses a `memory.bin` it finds in the staging dir. A
    /// periodic Full capture's memory file is never synced, so its retire
    /// must be durable before the checkpoint record lands: otherwise a
    /// crash can bring the file back with holes, and the restore reads
    /// those holes as zeroed guest memory.
    #[tokio::test]
    async fn a_retired_memory_file_is_durably_gone_before_the_retire_returns() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = full_capture_dest(tmp.path());
        let memory = dest.join("memory.bin");
        let fs = Arc::new(LoggingFs::default());
        crate::snapshot_post::retire_consumed(fs.clone(), memory.clone())
            .await
            .unwrap();

        assert!(!memory.exists());
        let ops: Vec<_> = fs
            .ops()
            .into_iter()
            .filter(|(op, _)| *op != "remove_file")
            .collect();
        assert_eq!(
            ops,
            vec![
                ("rename", dest.join("memory.bin.consumed")),
                ("sync_dir", dest),
            ]
        );
    }
}
