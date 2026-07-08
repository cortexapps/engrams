//! ADR 0081 P1b: capture as a durable, heartbeat-dispatched host-owned
//! job.
//!
//! Base-snapshot capture used to run inside a streaming `BuildBaseSnapshot`
//! RPC (`grpc_server.rs`, deleted this commit): the coordinator called
//! `HostClient::build_base_snapshot` and consumed a `CaptureProgress`
//! stream; a dropped client stream did NOT cancel the host-side capture —
//! it ran to completion, its terminal frame sent into the void — while
//! the coordinator's own retry booted a SECOND capture VM with no
//! anti-affinity or awareness that the first attempt was still running.
//!
//! This module makes capture a durable job instead: the coordinator
//! dispatches `(job_id, epoch)` assignments over `HeartbeatAck.
//! capture_assignments`; the host claims the full dispatch
//! (`CoordClient::claim_capture_job`, the authed HTTP channel — secrets
//! ride only that response, never PG/heartbeat/the durable record below)
//! and runs [`engram_core::traits::sandbox::SandboxBackend::
//! build_base_snapshot`] UNCHANGED — this module is a layer ABOVE that
//! seam, not a reimplementation of it. What's new:
//!
//! - [`CaptureJobRecord`]: a durable, write+fsync+rename record at
//!   `<records_dir>/capture-jobs/<job_id>.json` (the `CheckpointRecord`
//!   pattern verbatim) — survives a host-agent restart mid-capture.
//! - [`CaptureJobExecutor`]: drains `build_base_snapshot`'s progress
//!   channel into the durable record + an in-memory report the heartbeat
//!   loop reads every tick (re-advertised until the coordinator acks it),
//!   and tracks each job's live `(epoch, sandbox_id)` so a reassignment
//!   to a higher epoch destroys the stale attempt first (the
//!   duplicate-capture-VM class becomes unrepresentable host-side).
//! - The reaper exemption ADR 0050 E relied on (`PooledBackend::
//!   is_base_capture`, deleted) is replaced by
//!   [`CaptureJobExecutor::is_live_sandbox`] — "this sandbox belongs to a
//!   live (non-terminal) job", checked against the SAME in-memory
//!   registry the executor drives from, not a separate flag on the
//!   backend.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::capture_job::{CaptureJobSpec, CaptureJobStage};
use engram_core::types::{
    CaptureJobAssignment, CaptureJobId, CaptureJobProgress, CaptureJobReport, CaptureProgress,
    CaptureTerminalReport, SandboxId,
};
use serde::{Deserialize, Serialize};

/// The durable, on-disk shape of one capture job's host-side state —
/// mirrors `checkpoint::CheckpointRecord` exactly (write + fsync +
/// rename; torn-write-tolerant `load_all`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureJobRecord {
    pub job_id: CaptureJobId,
    pub epoch: i64,
    pub stage: CaptureJobStage,
    /// The capture VM's sandbox id, once known (the executor learns this
    /// off the FIRST `CaptureProgress` event `build_base_snapshot` sends,
    /// which now carries it — see `CaptureProgress::sandbox_id`). `None`
    /// before that first event, or for a job that never got far enough
    /// to create a VM (e.g. failed at claim/dispatch).
    pub sandbox_id: Option<SandboxId>,
    pub terminal: Option<CaptureTerminalReport>,
}

impl CaptureJobRecord {
    pub fn path_in(dir: &Path, id: CaptureJobId) -> PathBuf {
        dir.join(format!("{id}.json"))
    }

    /// Durably persist (write + fsync via rename) into `dir`.
    pub async fn persist(&self, dir: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let dest = Self::path_in(dir, self.job_id);
        let tmp = dest.with_extension("json.partial");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("serialize capture job record: {e}")))?;
        tokio::fs::write(&tmp, &bytes).await?;
        // fsync the temp file so the rename publishes complete bytes.
        let f = tokio::fs::OpenOptions::new().read(true).open(&tmp).await?;
        f.sync_all().await?;
        tokio::fs::rename(&tmp, &dest).await?;
        Ok(())
    }

    /// All records in `dir` (the heartbeat advert payload + rehydrate
    /// source). Unreadable/partial files are skipped with a warn — a
    /// torn write must not wedge the heartbeat loop.
    pub async fn load_all(dir: &Path) -> Vec<CaptureJobRecord> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return out;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read(&p).await {
                Ok(bytes) => match serde_json::from_slice::<CaptureJobRecord>(&bytes) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e,
                            "unparseable capture job record; skipping");
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "unreadable capture job record; skipping");
                }
            }
        }
        out
    }

    /// Coord acked these — the PG rows own the references now.
    pub async fn delete_acked(dir: &Path, acked: &[CaptureJobId]) {
        for id in acked {
            let p = Self::path_in(dir, *id);
            if let Err(e) = tokio::fs::remove_file(&p).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %p.display(), error = %e,
                        "failed to delete acked capture job record");
                }
            }
        }
    }
}

fn report_from_record(rec: &CaptureJobRecord) -> CaptureJobReport {
    CaptureJobReport {
        job_id: rec.job_id,
        epoch: rec.epoch,
        stage: rec.stage,
        progress: None,
        fc_snapshot_version: None,
        terminal: rec.terminal.clone(),
    }
}

/// Map a raw `CaptureProgress` event (from `SandboxBackend::
/// build_base_snapshot`'s progress channel) to the `capture_jobs` stage
/// taxonomy: `Boot -> Booting`, `Warm -> Warming`, `Snapshot -> Freezing`.
fn stage_from_phase(phase: engram_core::types::CapturePhase) -> CaptureJobStage {
    use engram_core::types::CapturePhase;
    match phase {
        CapturePhase::Boot => CaptureJobStage::Booting,
        CapturePhase::Warm => CaptureJobStage::Warming,
        CapturePhase::Snapshot => CaptureJobStage::Freezing,
    }
}

/// One job's live-execution bookkeeping — what lets a stale (lower-
/// epoch) attempt be detected and cancelled the instant a fresher
/// assignment arrives.
struct RunningJob {
    epoch: i64,
    sandbox_id: Option<SandboxId>,
    handle: tokio::task::JoinHandle<()>,
}

/// The host-agent's capture-job executor + live registry. One instance
/// per host-agent process, held by the heartbeat loop (to snapshot
/// reports / drive claims) and the teardown reconcile (to check the
/// live-sandbox exemption).
pub struct CaptureJobExecutor {
    backend: Arc<dyn SandboxBackend>,
    records_dir: PathBuf,
    /// Current report per job — what the heartbeat loop snapshots every
    /// tick. Entries are removed once the coordinator acks a terminal
    /// report (`ack`); non-terminal entries are refreshed continuously
    /// by the running executor task.
    reports: Arc<DashMap<CaptureJobId, CaptureJobReport>>,
    /// `std::sync::Mutex`, not `DashMap`: `start` needs an atomic
    /// check-current-epoch-then-replace-and-spawn under one lock (a
    /// `DashMap` entry API can't hold a non-`Send` guard across the
    /// `tokio::spawn` call cleanly, and the critical section here is a
    /// few micro-ops — a blocking mutex is the simpler correct choice).
    running: Mutex<HashMap<CaptureJobId, RunningJob>>,
    /// Self-reference (the `PooledBackend::set_self_ref` pattern
    /// verbatim): `start` needs to hand a spawned task an OWNED
    /// `Arc<Self>` while itself only borrowing `&self` — callers hold a
    /// shared `Arc<CaptureJobExecutor>` and call `start`/`should_claim`/
    /// etc. repeatedly on it, so `start` can't consume `self` by value.
    self_ref: std::sync::OnceLock<std::sync::Weak<Self>>,
}

impl CaptureJobExecutor {
    pub fn new(backend: Arc<dyn SandboxBackend>, records_dir: PathBuf) -> Arc<Self> {
        let arc = Arc::new(Self {
            backend,
            records_dir,
            reports: Arc::new(DashMap::new()),
            running: Mutex::new(HashMap::new()),
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = arc.self_ref.set(Arc::downgrade(&arc));
        arc
    }

    fn strong_self(&self) -> Arc<Self> {
        self.self_ref
            .get()
            .and_then(|w| w.upgrade())
            .expect("self_ref is set at construction and this instance is still alive")
    }

    /// Restart rehydrate (call once at host-agent startup, alongside the
    /// checkpoint records' own rehydrate): a non-terminal record means
    /// the capture VM (if any) is orphaned — destroy it if still alive,
    /// then rewrite the record terminal-Failed-retryable so the
    /// coordinator reassigns rather than waiting out a stage deadline
    /// for a job nothing is driving anymore. A terminal record simply
    /// re-seeds the in-memory report so it keeps re-advertising until
    /// acked (a restart must not silently drop an un-acked outcome).
    pub async fn rehydrate(&self) {
        let records = CaptureJobRecord::load_all(&self.records_dir).await;
        for rec in records {
            if rec.stage.is_terminal() {
                self.reports.insert(rec.job_id, report_from_record(&rec));
                continue;
            }
            tracing::warn!(
                job_id = %rec.job_id,
                stage = rec.stage.as_str(),
                sandbox_id = ?rec.sandbox_id,
                "capture job record non-terminal at host-agent restart; destroying any \
                 surviving VM and reporting a retryable failure",
            );
            if let Some(sid) = rec.sandbox_id {
                if let Err(e) = self.backend.destroy(sid).await {
                    tracing::debug!(job_id = %rec.job_id, sandbox_id = %sid, error = %e,
                        "capture job rehydrate: destroy of orphaned capture VM failed (best-effort)");
                }
            }
            let failed = CaptureJobRecord {
                job_id: rec.job_id,
                epoch: rec.epoch,
                stage: CaptureJobStage::Failed,
                sandbox_id: rec.sandbox_id,
                terminal: Some(CaptureTerminalReport::Failed {
                    error: "host-agent restarted mid-capture".to_string(),
                    error_stage: rec.stage.as_str().to_string(),
                    retryable: true,
                }),
            };
            if let Err(e) = failed.persist(&self.records_dir).await {
                tracing::warn!(job_id = %failed.job_id, error = %e,
                    "capture job rehydrate: failed to persist the rewound terminal record");
            }
            self.reports
                .insert(failed.job_id, report_from_record(&failed));
        }
    }

    /// Current reports for the heartbeat's `capture_job_reports` field —
    /// every job this executor knows about, running or un-acked-terminal.
    pub fn current_reports(&self) -> Vec<CaptureJobReport> {
        self.reports.iter().map(|e| e.value().clone()).collect()
    }

    /// True while `id` is the live sandbox of a non-terminal job — the
    /// teardown reconcile's exemption predicate (replaces `PooledBackend
    /// ::is_base_capture`). Exempt only while the job lives, never
    /// forever: cleared the instant the executor task finishes (success
    /// or failure) or a stale attempt is cancelled by a reassignment.
    pub fn is_live_sandbox(&self, id: SandboxId) -> bool {
        self.running
            .lock()
            .unwrap()
            .values()
            .any(|j| j.sandbox_id == Some(id))
    }

    /// The coordinator acked these terminal reports into PG — drop the
    /// in-memory report (stop re-advertising) and the durable record
    /// file.
    pub async fn ack(&self, acked: &[CaptureJobId]) {
        if acked.is_empty() {
            return;
        }
        for id in acked {
            self.reports.remove(id);
        }
        CaptureJobRecord::delete_acked(&self.records_dir, acked).await;
    }

    /// Whether the heartbeat-ack loop should call
    /// `CoordClient::claim_capture_job` for `(job_id, epoch)`: `true`
    /// unless this exact epoch is already running. A LOWER epoch than
    /// what's running is implicitly "no" too (the assignment is stale —
    /// the coordinator hasn't caught up to a reassignment this host
    /// already knows about), matching ADR 0081's "ignore a
    /// lower-than-running epoch" rule without a separate branch.
    /// Convergence cancel (ADR 0081 §A): destroy the VM of any
    /// still-running attempt whose `job_id` is entirely absent from the
    /// coordinator's AUTHORITATIVE assignment list for this host — it was
    /// reassigned to another host or terminally superseded, so its writes
    /// are fenced off and its VM is pure capacity waste. Deliberately
    /// destroys the VM WITHOUT aborting the executor task: the dying VM
    /// makes `build_base_snapshot` fail fast, and `run_one` then walks its
    /// normal terminal-bookkeeping path (durable record + report), whose
    /// stale-epoch report the coordinator acks-and-discards. Aborting the
    /// task instead would race its record/report bookkeeping.
    ///
    /// Call ONLY with a `Some` assignment list (a coord-side read failure
    /// is `None` = unknown — cancelling on it would let a PG blip destroy
    /// healthy in-flight captures fleet-wide). Epoch-mismatched entries
    /// are NOT cancelled here — the claim path's stale-epoch cancel in
    /// [`Self::start`] owns that transition.
    pub fn cancel_absent(&self, assignments: &[CaptureJobAssignment]) {
        let present: std::collections::HashSet<CaptureJobId> =
            assignments.iter().map(|a| a.job_id).collect();
        let stale: Vec<(CaptureJobId, i64, SandboxId)> = {
            let running = self.running.lock().unwrap();
            running
                .iter()
                .filter(|(job_id, _)| !present.contains(job_id))
                .filter_map(|(job_id, r)| r.sandbox_id.map(|sid| (*job_id, r.epoch, sid)))
                .collect()
        };
        for (job_id, epoch, sid) in stale {
            tracing::warn!(
                %job_id,
                epoch,
                sandbox_id = %sid,
                "capture job absent from the coordinator's assignment list \
                 (reassigned away or superseded); destroying its VM",
            );
            let backend = self.backend.clone();
            tokio::spawn(async move {
                if let Err(e) = backend.destroy(sid).await {
                    tracing::debug!(sandbox_id = %sid, error = %e,
                        "destroy of an unassigned capture VM failed (best-effort)");
                }
            });
        }
    }

    pub fn should_claim(&self, job_id: CaptureJobId, epoch: i64) -> bool {
        match self.running.lock().unwrap().get(&job_id) {
            Some(running) => epoch > running.epoch,
            None => true,
        }
    }

    /// Start executing `job_id` at `epoch` with the claimed `spec`. If a
    /// LOWER-epoch attempt is currently running for the same `job_id`,
    /// cancel it first (abort the task, destroy its VM if one exists) —
    /// the stale-epoch-cancel invariant that makes a duplicate capture
    /// VM unrepresentable host-side. Call only after `should_claim`
    /// returned `true` for the same `(job_id, epoch)` (the heartbeat loop
    /// claims-then-starts as one sequenced step per job per tick, so
    /// there's no concurrent-start race to guard here).
    pub fn start(&self, job_id: CaptureJobId, epoch: i64, spec: CaptureJobSpec) {
        {
            let mut running = self.running.lock().unwrap();
            if let Some(stale) = running.remove(&job_id) {
                if stale.epoch >= epoch {
                    // Raced with a duplicate `start` call for the same
                    // (or an already-superseded) epoch — put it back and
                    // do nothing.
                    running.insert(job_id, stale);
                    return;
                }
                tracing::warn!(
                    %job_id,
                    stale_epoch = stale.epoch,
                    new_epoch = epoch,
                    sandbox_id = ?stale.sandbox_id,
                    "capture job reassigned to a fresh epoch while a stale attempt was still \
                     running; cancelling the stale attempt",
                );
                stale.handle.abort();
                if let Some(sid) = stale.sandbox_id {
                    let backend = self.backend.clone();
                    tokio::spawn(async move {
                        if let Err(e) = backend.destroy(sid).await {
                            tracing::debug!(sandbox_id = %sid, error = %e,
                                "destroy of a stale (reassigned-away) capture VM failed (best-effort)");
                        }
                    });
                }
            }
            // Reserve the slot with a placeholder immediately (before the
            // real task exists) so a concurrent heartbeat tick's
            // `should_claim` sees this epoch as already owned.
            let placeholder = tokio::spawn(async {});
            running.insert(
                job_id,
                RunningJob {
                    epoch,
                    sandbox_id: None,
                    handle: placeholder,
                },
            );
        }
        let executor = self.strong_self();
        let handle = tokio::spawn(async move {
            executor.run_one(job_id, epoch, spec).await;
        });
        // Replace the placeholder handle with the real one. The task
        // above may already be racing ahead (updating `sandbox_id` via
        // `report_progress`), but the `epoch` key is unchanged so that's
        // race-free.
        if let Some(entry) = self.running.lock().unwrap().get_mut(&job_id) {
            entry.handle = handle;
        }
    }

    async fn run_one(self: Arc<Self>, job_id: CaptureJobId, epoch: i64, job_spec: CaptureJobSpec) {
        let mut record = CaptureJobRecord {
            job_id,
            epoch,
            stage: CaptureJobStage::Booting,
            sandbox_id: None,
            terminal: None,
        };
        if let Err(e) = record.persist(&self.records_dir).await {
            tracing::warn!(%job_id, error = %e, "capture job: failed to persist initial record");
        }
        self.reports.insert(job_id, report_from_record(&record));

        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<CaptureProgress>(64);
        let backend = self.backend.clone();
        let capture_task = tokio::spawn(async move {
            backend
                .build_base_snapshot(
                    job_spec.spec,
                    job_spec.warm,
                    job_spec.resolved_env,
                    job_spec.capture_egress,
                    progress_tx,
                )
                .await
        });

        while let Some(progress) = progress_rx.recv().await {
            if let Some(sid) = progress.sandbox_id {
                record.sandbox_id = Some(sid);
                if let Some(entry) = self.running.lock().unwrap().get_mut(&job_id) {
                    if entry.epoch == epoch {
                        entry.sandbox_id = Some(sid);
                    }
                }
            }
            record.stage = stage_from_phase(progress.phase);
            if let Err(e) = record.persist(&self.records_dir).await {
                tracing::warn!(%job_id, error = %e, "capture job: failed to persist progress record");
            }
            self.reports.insert(
                job_id,
                CaptureJobReport {
                    job_id,
                    epoch,
                    stage: record.stage,
                    progress: Some(CaptureJobProgress {
                        detail: progress.detail.clone(),
                        log_tail: Some(progress.output_tail.clone()),
                    }),
                    fc_snapshot_version: None,
                    terminal: None,
                },
            );
        }

        let terminal = match capture_task.await {
            Ok(Ok(meta)) => match bincode::serialize(&meta) {
                Ok(bytes) => CaptureTerminalReport::Done {
                    result_bincode: bytes,
                },
                Err(e) => CaptureTerminalReport::Failed {
                    error: format!("failed to encode capture result: {e}"),
                    error_stage: record.stage.as_str().to_string(),
                    retryable: false,
                },
            },
            Ok(Err(sandbox_err)) => {
                let (retryable, error_stage) = classify_sandbox_error(&sandbox_err, &record.stage);
                CaptureTerminalReport::Failed {
                    error: sandbox_err.to_string(),
                    error_stage,
                    retryable,
                }
            }
            Err(join_err) => CaptureTerminalReport::Failed {
                error: format!("capture executor task panicked or was aborted: {join_err}"),
                error_stage: record.stage.as_str().to_string(),
                retryable: true,
            },
        };
        record.stage = match &terminal {
            CaptureTerminalReport::Done { .. } => CaptureJobStage::Done,
            CaptureTerminalReport::Failed { .. } => CaptureJobStage::Failed,
        };
        record.terminal = Some(terminal.clone());
        if let Err(e) = record.persist(&self.records_dir).await {
            tracing::warn!(%job_id, error = %e, "capture job: failed to persist terminal record");
        }
        self.reports.insert(
            job_id,
            CaptureJobReport {
                job_id,
                epoch,
                stage: record.stage,
                progress: None,
                fc_snapshot_version: None,
                terminal: Some(terminal),
            },
        );
        // Done: clear the live-sandbox exemption + running-registry entry
        // (but only if we're still the epoch of record — a reassignment
        // may have already replaced this entry, in which case its
        // teardown is that newer attempt's `start` call's job, not ours).
        let mut running = self.running.lock().unwrap();
        if matches!(running.get(&job_id), Some(r) if r.epoch == epoch) {
            running.remove(&job_id);
        }
    }
}

/// Best-effort classification of a non-`CaptureFailed` `SandboxError`
/// (e.g. `create()`/`destroy()` failing outside the structured capture-
/// failure taxonomy) — treated as retryable (a transient host issue)
/// unless the backend reports a structured `CaptureFailure`, whose own
/// `CaptureFailureKind::is_retryable()` is authoritative.
fn classify_sandbox_error(
    e: &engram_core::SandboxError,
    current_stage: &CaptureJobStage,
) -> (bool, String) {
    match e {
        engram_core::SandboxError::CaptureFailed(failure) => (
            failure.kind.is_retryable(),
            failure
                .stage
                .clone()
                .unwrap_or_else(|| current_stage.as_str().to_string()),
        ),
        _ => (true, current_stage.as_str().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::image::WarmConfig;
    use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::SandboxError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn live_spec(image: &str) -> SandboxSpec {
        SandboxSpec {
            image: image.into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 64 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    /// A mock `SandboxBackend` whose `build_base_snapshot` drives a
    /// caller-supplied progress+result script — enough to exercise the
    /// executor's record persistence + report snapshotting + the
    /// live-sandbox reaper-exemption predicate without a real VM.
    struct MockBackend {
        create_calls: AtomicUsize,
        destroy_calls: AtomicUsize,
        last_destroyed: Mutex<Option<SandboxId>>,
    }

    #[async_trait]
    impl SandboxBackend for MockBackend {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            Ok(SandboxId::new())
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
            PathBuf::new()
        }
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            Err(SandboxError::InvalidSpec("unused".into()))
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroy_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_destroyed.lock().unwrap() = Some(id);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn build_base_snapshot(
            &self,
            _spec: SandboxSpec,
            _warm: Option<WarmConfig>,
            _capture_env: std::collections::HashMap<String, String>,
            _capture_egress: Option<SessionEgressPolicy>,
            progress: tokio::sync::mpsc::Sender<CaptureProgress>,
        ) -> Result<SnapshotMetadata, SandboxError> {
            let id = self.create(live_spec("mock")).await?;
            let _ = progress
                .send(CaptureProgress {
                    phase: engram_core::types::CapturePhase::Boot,
                    sandbox_id: Some(id),
                    warm_stage: None,
                    detail: None,
                    output_tail: String::new(),
                    warm_stages: Vec::new(),
                })
                .await;
            // Hold the "VM" open long enough for the test to observe the
            // live-sandbox exemption before completing.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.destroy(id).await?;
            Ok(SnapshotMetadata {
                id: engram_core::SnapshotId::new(),
                size_bytes: 1,
                created_at: chrono::Utc::now(),
                image_version: "mock:1".into(),
                disk_manifest: None,
                memory_manifest: None,
                base_memory_manifest: None,
                migration_source: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: vec![],
                paused_at: None,
            })
        }
    }

    fn spec(image: &str) -> CaptureJobSpec {
        CaptureJobSpec {
            spec: live_spec(image),
            warm: None,
            resolved_env: Default::default(),
            capture_egress: None,
        }
    }

    /// Regression (ported from the retired `PooledBackend::
    /// is_base_capture` test): a capture VM must be exempt from the
    /// teardown reconcile for its whole lifetime, and the exemption must
    /// be cleared once the job finishes.
    #[tokio::test]
    async fn capture_vm_is_exempt_from_reconcile_then_cleared() {
        let backend = Arc::new(MockBackend {
            create_calls: AtomicUsize::new(0),
            destroy_calls: AtomicUsize::new(0),
            last_destroyed: Mutex::new(None),
        });
        let tmp = tempfile::tempdir().unwrap();
        let executor = CaptureJobExecutor::new(backend.clone(), tmp.path().join("capture-jobs"));

        let job_id = CaptureJobId::new();
        assert!(executor.should_claim(job_id, 1));
        executor.start(job_id, 1, spec("exempt-test"));

        // Wait for the sandbox_id to show up in a report (first progress
        // frame landed), then assert it's exempt.
        let sandbox_id = wait_for_sandbox_id(&executor, job_id).await;
        assert!(
            executor.is_live_sandbox(sandbox_id),
            "capture VM must be exempt (is_live_sandbox) while the job runs",
        );

        wait_for_terminal(&executor, job_id).await;
        assert!(
            !executor.is_live_sandbox(sandbox_id),
            "the exemption must be cleared once the job finishes",
        );
        assert_eq!(backend.destroy_calls.load(Ordering::SeqCst), 1);
    }

    /// Stale-epoch cancel: an assignment at a HIGHER epoch than what's
    /// running must destroy the stale attempt's VM before claiming anew.
    #[tokio::test]
    async fn higher_epoch_cancels_and_destroys_stale_attempt() {
        let backend = Arc::new(MockBackend {
            create_calls: AtomicUsize::new(0),
            destroy_calls: AtomicUsize::new(0),
            last_destroyed: Mutex::new(None),
        });
        let tmp = tempfile::tempdir().unwrap();
        let executor = CaptureJobExecutor::new(backend.clone(), tmp.path().join("capture-jobs"));

        let job_id = CaptureJobId::new();
        executor.start(job_id, 1, spec("epoch-1"));
        let stale_sandbox = wait_for_sandbox_id(&executor, job_id).await;

        assert!(
            !executor.should_claim(job_id, 1),
            "same epoch already running"
        );
        assert!(
            executor.should_claim(job_id, 2),
            "higher epoch must be claimable"
        );

        executor.start(job_id, 2, spec("epoch-2"));
        // Give the abort + destroy a moment to land.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(
            *backend.last_destroyed.lock().unwrap(),
            Some(stale_sandbox),
            "the stale (epoch 1) attempt's VM must be destroyed on reassignment",
        );

        assert!(
            !executor.should_claim(job_id, 1),
            "epoch 1 is now stale twice over"
        );
    }

    /// A lower-epoch assignment than what's running must be ignored.
    #[tokio::test]
    async fn lower_epoch_assignment_is_ignored() {
        let backend = Arc::new(MockBackend {
            create_calls: AtomicUsize::new(0),
            destroy_calls: AtomicUsize::new(0),
            last_destroyed: Mutex::new(None),
        });
        let tmp = tempfile::tempdir().unwrap();
        let executor = CaptureJobExecutor::new(backend, tmp.path().join("capture-jobs"));
        let job_id = CaptureJobId::new();
        executor.start(job_id, 5, spec("epoch-5"));
        assert!(
            !executor.should_claim(job_id, 3),
            "a lower epoch must never be claimable"
        );
    }

    /// Restart rehydrate: a non-terminal record must destroy any
    /// surviving sandbox and rewrite itself terminal-Failed-retryable.
    #[tokio::test]
    async fn rehydrate_fails_non_terminal_record_and_destroys_survivor() {
        let backend = Arc::new(MockBackend {
            create_calls: AtomicUsize::new(0),
            destroy_calls: AtomicUsize::new(0),
            last_destroyed: Mutex::new(None),
        });
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("capture-jobs");
        let job_id = CaptureJobId::new();
        let sandbox_id = SandboxId::new();
        let orphaned = CaptureJobRecord {
            job_id,
            epoch: 3,
            stage: CaptureJobStage::Warming,
            sandbox_id: Some(sandbox_id),
            terminal: None,
        };
        orphaned.persist(&dir).await.unwrap();

        let executor = CaptureJobExecutor::new(backend.clone(), dir.clone());
        executor.rehydrate().await;

        assert_eq!(
            *backend.last_destroyed.lock().unwrap(),
            Some(sandbox_id),
            "rehydrate must destroy the orphaned survivor VM",
        );
        let reports = executor.current_reports();
        assert_eq!(reports.len(), 1);
        match &reports[0].terminal {
            Some(CaptureTerminalReport::Failed {
                retryable,
                error_stage,
                ..
            }) => {
                assert!(*retryable);
                assert_eq!(error_stage, "warming");
            }
            other => panic!("expected a rewound Failed terminal, got {other:?}"),
        }

        // The on-disk record must ALSO be rewound (not just the in-memory
        // report) — a second rehydrate (e.g. a crash loop) must not
        // re-destroy an already-gone sandbox as if it were still live.
        let reloaded = CaptureJobRecord::load_all(&dir).await;
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].stage, CaptureJobStage::Failed);
    }

    /// `ack` must drop both the in-memory report and the durable file.
    #[tokio::test]
    async fn ack_clears_report_and_durable_record() {
        let backend = Arc::new(MockBackend {
            create_calls: AtomicUsize::new(0),
            destroy_calls: AtomicUsize::new(0),
            last_destroyed: Mutex::new(None),
        });
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("capture-jobs");
        let executor = CaptureJobExecutor::new(backend, dir.clone());
        let job_id = CaptureJobId::new();
        executor.start(job_id, 1, spec("ack-test"));
        wait_for_terminal(&executor, job_id).await;
        assert_eq!(executor.current_reports().len(), 1);

        executor.ack(&[job_id]).await;
        assert!(executor.current_reports().is_empty());
        assert!(CaptureJobRecord::load_all(&dir).await.is_empty());
    }

    async fn wait_for_sandbox_id(
        executor: &Arc<CaptureJobExecutor>,
        job_id: CaptureJobId,
    ) -> SandboxId {
        for _ in 0..200 {
            // The report itself doesn't carry sandbox_id (that's an
            // executor-internal detail); poll `is_live_sandbox` via the
            // running-registry through a report/stage check instead: once
            // the report exists at all, the sandbox has very likely been
            // assigned (the very first progress frame carries it) — but
            // to avoid a race, poll the executor's internal state via a
            // short sleep loop and re-derive from `running`.
            if let Some(id) = executor
                .running
                .lock()
                .unwrap()
                .get(&job_id)
                .and_then(|r| r.sandbox_id)
            {
                return id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for sandbox_id to be assigned for {job_id}");
    }

    async fn wait_for_terminal(executor: &Arc<CaptureJobExecutor>, job_id: CaptureJobId) {
        for _ in 0..200 {
            if executor
                .reports
                .get(&job_id)
                .map(|r| r.terminal.is_some())
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for job {job_id} to reach a terminal report");
    }
}
