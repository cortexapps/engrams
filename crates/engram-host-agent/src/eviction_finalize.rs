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
//! record at `BlobsUploaded`, then writes the terminal checkpoint record.
//! Each leg is idempotent, so a redrive re-runs whatever the persisted
//! stage has not passed.
use engram_core::types::snapshot::DiskRole;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
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
        chunk_store: Option<ChunkStore>,
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
            chunk_store,
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
            chunk_store: self.chunk_store.as_ref(),
            chunk_cache: self.chunk_cache.as_ref(),
            bundle_dir: &self.bundle_dir,
            bundle_file_ext: self.bundle_file_ext,
        }
    }
}

/// Run the legs the record's stage has not yet passed, then persist one
/// bump to `BlobsUploaded` with what they published. The consumed memory
/// file is deleted only after that bump is durable; a crash after the
/// bump only leaks it.
async fn run_post_phase(
    f: &EvictionFinalizer,
    record: &mut EvictionFinalizeRecord,
) -> Result<(), SandboxError> {
    use crate::snapshot_post::{Disks, Memory, Work};
    let memory =
        (record.stage < FinalizeStage::MemoryChunked).then_some(match record.chain_prev_ref {
            Some(prev_ref) => Memory::Diff {
                prev_ref,
                prev: None,
            },
            None => Memory::Full,
        });
    let disks = (record.stage == FinalizeStage::Captured).then(|| Disks::Named {
        sandbox: record.sandbox_id,
        pending: record.disk_pending.clone(),
    });
    let published = f
        .post_phase()
        .run(Work {
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
        ("disk", published.seconds.disk),
        ("memory", published.seconds.memory),
        ("blobs", published.seconds.blobs),
    ] {
        if let Some(seconds) = seconds {
            metrics::histogram!(crate::metrics::EVICTION_FINALIZE_STAGE_SECONDS, "stage" => stage)
                .record(seconds);
        }
    }

    let before = record.clone();
    if record.stage == FinalizeStage::Captured {
        record.disk_manifest = published.root;
        record.swap_manifest = published.swap;
    }
    let consumed = if record.stage < FinalizeStage::MemoryChunked {
        record.memory_manifest = published.memory.as_ref().map(|m| m.manifest_ref);
        published.memory.map(|m| m.consumed)
    } else {
        None
    };
    record.stage = FinalizeStage::BlobsUploaded;
    if let Err(e) = record.persist(f.fs.as_ref(), &f.finalize_dir()).await {
        // A retry must never believe an un-persisted stage is durable.
        *record = before;
        return Err(SandboxError::Snapshot(format!(
            "persist finalize record: {e}"
        )));
    }
    if let Some(path) = consumed {
        crate::snapshot_post::retire_consumed(f.fs.clone(), path).await;
    }
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
        let volumes = Arc::new(engram_host_core::SessionVolumes::new(
            disk,
            engram_fill::FillEngine::new(store.clone(), engram_fill::Config::default()),
        ));
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
        let blob = finalizer.chunk_store.as_ref().unwrap().blob_storage();
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
}
