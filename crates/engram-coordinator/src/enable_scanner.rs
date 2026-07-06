//! ADR 0036 — async image-enable scanner.
//!
//! Background task that drives `enable_jobs` rows through
//! `pending → materializing → capturing → ready | failed`. Sibling
//! to [`crate::evac_resumer`]: same polling shape, same shared-state
//! surface, distinct table.
//!
//! ## Flow
//!
//! 1. Tick: atomically claim up to `claim_limit` non-terminal jobs
//!    whose lease is free or expired
//!    ([`engram_core::traits::MetadataStore::claim_enable_jobs`]).
//! 2. Per job, run the enable pipeline from the top — every step
//!    fast-forwards, so a job resumed after a coordinator crash
//!    re-runs cheaply:
//!    - [`crate::api::enabled_images::fetch_and_seal_manifest`] —
//!      KB-sized metadata pull, parses the manifest.
//!    - `materializing`: [`crate::api::enabled_images::materialize_disk_chunks`]
//!      with a shared progress counter; already-present chunks
//!      content-address-skip (the resume high-water mark is free). A
//!      side task checkpoints `chunks_done` to PG every couple of
//!      seconds — that's the operator's progress bar AND the claim
//!      renewal that stops a peer from stealing a long materialize.
//!    - `capturing`: [`crate::api::enabled_images::capture_and_record_base_snapshot`]
//!      boots the capture VM on a host (idempotent via the
//!      digest-keyed reuse check).
//!    - upsert the `enabled_images` row → `ready`.
//! 3. On any pipeline error: record the failure (bumps `attempts`,
//!    stores `error`, releases the claim) and leave the job in its
//!    current state for the next tick. After `max_attempts` the job
//!    flips to `failed` — `POST /api/v1/enable-jobs/:id/retry`
//!    re-queues it.
//!
//! ## Why this pattern
//!
//! Per `[async_via_state_machine]` — the enable pipeline does minutes
//! of I/O (10 GB-class images) plus a VM boot. Synchronous
//! orchestration inside the POST handler meant the external LB's
//! ~30 s timeout killed every real enable (hence the port-forward +
//! curl ritual this scanner retires). Multi-coordinator safety comes
//! from the lease claim: one pod owns a job at a time; a crashed
//! pod's claim expires and a peer re-claims, resuming idempotently.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engram_core::types::{EnableJob, EnableJobState};
use engram_core::MetaError;

use crate::api::enabled_images::{
    capture_and_record_base_snapshot, fetch_and_seal_manifest, materialize_disk_chunks,
};
use crate::state::SharedState;

#[derive(Clone, Debug)]
pub struct EnableScannerConfig {
    /// Sweep cadence. Enables are operator-interactive (someone is
    /// watching a progress bar), so this is snappier than the 10s
    /// fleet scanners.
    pub poll_interval: Duration,
    /// Claim lease. Must comfortably exceed the longest gap between
    /// claim renewals — both the materialize AND the capture step run a
    /// ticker that renews every `progress_interval`. (Until that ticker
    /// covered the capture step, a long `[warm]`-hook capture — tens of
    /// minutes of warm boot + snapshot upload — would expire this lease
    /// mid-capture, and a peer would re-claim and spawn a duplicate
    /// concurrent capture that fought the first for host resources.)
    pub lease_secs: u32,
    /// Jobs claimed per tick. Enables are heavyweight (registry +
    /// GCS I/O, then a capture VM per job); a small bound keeps one
    /// coord pod from absorbing the whole queue.
    pub claim_limit: u32,
    /// Pipeline failures per job before flipping to `failed`.
    pub max_attempts: u32,
    /// How often the materialize progress counter is checkpointed to
    /// PG (progress bar + claim renewal).
    pub progress_interval: Duration,
}

impl Default for EnableScannerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(3),
            // Capture on a nested-KVM dev-vm has been observed at
            // ~150 s; 300 s keeps a healthy margin before a peer
            // declares the claim stale.
            lease_secs: 300,
            claim_limit: 2,
            max_attempts: 5,
            progress_interval: Duration::from_secs(2),
        }
    }
}

/// Spawn the scanner as a background task. Caller holds the
/// JoinHandle for the process lifetime; dropping aborts the loop.
/// Mirrors [`crate::evac_resumer::spawn`].
pub fn spawn(cfg: EnableScannerConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the first immediate tick — coord just started, give
        // hosts a beat to heartbeat in before capture-host picking.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "enable-scanner tick failed; will retry");
            }
        }
    })
}

/// Single scanner tick. `pub(crate)` so live-PG tests can drive the
/// scanner deterministically without `tokio::spawn`-ing the loop.
pub(crate) async fn run_once(
    cfg: &EnableScannerConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pod identity for the claim audit trail: the k8s pod name in
    // prod (HOSTNAME), the machine hostname in dev. Purely
    // observability — claim correctness comes from the atomic UPDATE.
    let claimant = std::env::var("HOSTNAME").unwrap_or_else(|_| "coord".into());
    let jobs = state
        .services
        .meta
        .claim_enable_jobs(&claimant, cfg.lease_secs, cfg.claim_limit)
        .await?;
    if jobs.is_empty() {
        return Ok(());
    }
    tracing::debug!(count = jobs.len(), "enable-scanner claimed jobs");
    for job in jobs {
        let job_id = job.id;
        match advance_one(cfg, state, &claimant, job).await {
            Ok(()) => {}
            Err(AdvanceError::LeaseLost(msg)) => {
                // #232: our lease expired and a peer re-claimed the job
                // mid-flight. Abandon immediately — every state write is
                // fenced, so we hold no authority over the row anymore.
                // Recording a failure here would clear the new
                // claimant's lease and stamp `error` on a job it is
                // actively completing. Do nothing; the peer drives it.
                tracing::warn!(%job_id, reason = %msg, "enable job lease lost; abandoning to peer");
            }
            Err(e @ (AdvanceError::Pipeline(_) | AdvanceError::NonRetryable(_))) => {
                // Per-job failure: ONE atomic, fenced write bumps attempts,
                // stores the error, releases the claim, AND flips to `failed`
                // if the budget is spent (transient) or the failure is
                // non-retryable (deterministic — bail fast). Keep sweeping;
                // one wedged job must not stall the queue. A Conflict means
                // the lease went away between the failure and now, so we drop.
                let force_terminal = matches!(e, AdvanceError::NonRetryable(_));
                let msg = match &e {
                    AdvanceError::Pipeline(inner) | AdvanceError::NonRetryable(inner) => {
                        inner.to_string()
                    }
                    AdvanceError::LeaseLost(_) => unreachable!("guarded by the outer pattern"),
                };
                tracing::warn!(%job_id, error = %msg, non_retryable = force_terminal, "enable job pipeline failed");
                match state
                    .services
                    .meta
                    .record_enable_job_failure(
                        job_id,
                        &claimant,
                        &msg,
                        cfg.max_attempts,
                        force_terminal,
                    )
                    .await
                {
                    Ok((attempts, EnableJobState::Failed)) => {
                        tracing::warn!(
                            %job_id,
                            attempts,
                            max_attempts = cfg.max_attempts,
                            non_retryable = force_terminal,
                            "enable job marked failed",
                        );
                    }
                    Ok(_) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable job lease lost while recording failure; abandoning to peer");
                    }
                    Err(e2) => {
                        tracing::warn!(%job_id, error = %e2, "failed to record enable job failure");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Outcome of [`advance_one`] when it doesn't complete the pipeline.
enum AdvanceError {
    /// A fenced state write returned [`MetaError::Conflict`]: the lease
    /// expired and a peer re-claimed the job. The worker must abandon
    /// the job WITHOUT any further state writes (#232).
    LeaseLost(String),
    /// A transient pipeline error (registry/GCS, or a `NoCapacity` from
    /// the capture host picker). Eligible for the attempts-budget retry
    /// path — it may clear on its own next tick.
    Pipeline(Box<dyn std::error::Error + Send + Sync>),
    /// A DETERMINISTIC failure that retrying cannot fix — most importantly
    /// a `[warm]` hook that exits non-zero. Bail fast: fail the job on the
    /// first occurrence instead of re-loading + re-capturing the image
    /// `max_attempts` times for nothing. The operator can `RetryEnableJob`
    /// after fixing the image.
    NonRetryable(Box<dyn std::error::Error + Send + Sync>),
}

impl From<MetaError> for AdvanceError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Conflict(msg) => AdvanceError::LeaseLost(msg),
            other => AdvanceError::Pipeline(Box::new(other)),
        }
    }
}

/// Classify a base-snapshot capture failure. `ApiError::Unavailable` is the
/// capture-host picker's `NoCapacity` — transient, retry.
///
/// Issue #539: `ApiError::CaptureFailed` carries a `CaptureFailureKind` —
/// only `WarmExecTransport` (the exec stream died mid-run, e.g. a vsock/
/// gRPC connection loss) is retryable via the attempts budget; every other
/// kind (`WarmExitNonZero`/`WarmStall`/`WarmStageDeadline`/
/// `WarmGlobalTimeout`/`SnapshotFailed`) is a deterministic outcome that
/// retrying can't fix — bail fast on the first occurrence, same as the old
/// blanket `ApiError::Internal` treatment (a `[warm]` hook non-zero exit,
/// a snapshot that failed HEAD-verify, …).
fn classify_capture_error(e: crate::error::ApiError) -> AdvanceError {
    match e {
        crate::error::ApiError::Unavailable(_) => AdvanceError::Pipeline(Box::new(e)),
        crate::error::ApiError::CaptureFailed { kind, .. } if kind.is_retryable() => {
            AdvanceError::Pipeline(Box::new(e))
        }
        other => AdvanceError::NonRetryable(Box::new(other)),
    }
}

/// Run the enable pipeline for one claimed job. Every step is
/// idempotent, so this always starts from the top and fast-forwards:
/// metadata pull is KBs, materialize skips present chunks, capture
/// reuses a digest-matched base snapshot.
async fn advance_one(
    cfg: &EnableScannerConfig,
    state: &SharedState,
    claimant: &str,
    job: EnableJob,
) -> Result<(), AdvanceError> {
    let job_id = job.id;
    let image_uri = job.image_uri.clone();
    tracing::info!(
        %job_id,
        %image_uri,
        from_state = job.state.as_str(),
        attempt = job.attempts + 1,
        "enable-scanner: driving job",
    );

    // Pipeline I/O (registry/GCS/capture) is a `Pipeline` error; fenced
    // store writes (`?` on `MetaError`) become `LeaseLost` on Conflict
    // via `From<MetaError>` and bubble all the way out, abandoning the
    // job without further writes.
    let (mut row, manifest, artifacts) = fetch_and_seal_manifest(state, &image_uri)
        .await
        .map_err(|e| AdvanceError::Pipeline(Box::new(e)))?;
    // `fetch_and_seal_manifest` builds the row from the registry manifest,
    // which carries no secrets (ADR 0057). The capture-time env rides the
    // job (set on enable, inherited on refresh); stamp it onto the row so
    // `capture_and_record_base_snapshot` can resolve + inject it into the
    // `[warm]` hook, and the upsert persists it for the dashboard's edit form.
    row.capture_env = job.capture_env.clone();

    // ---- materializing ----
    state
        .services
        .meta
        .set_enable_job_state(job_id, claimant, EnableJobState::Materializing)
        .await?;
    // chunks_total from the bootstrap (None for harness-only images).
    let chunks_total = artifacts
        .disk_bootstrap_json
        .as_deref()
        .and_then(|b| serde_json::from_slice::<engram_chunk_store::Bootstrap>(b).ok())
        .map(|bs| bs.entries.len() as u32);
    let counter = Arc::new(AtomicU32::new(0));
    state
        .services
        .meta
        .update_enable_job_progress(job_id, claimant, 0, chunks_total)
        .await?;

    // Checkpoint task: persists the counter every couple of seconds.
    // Doubles as the (now fenced) claim renewal during a long
    // materialize. If a checkpoint hits a Conflict our lease is gone —
    // stop ticking so we don't keep hammering a row a peer owns; the
    // next state write in the main path surfaces the LeaseLost.
    let ticker = {
        let meta = state.services.meta.clone();
        let counter = counter.clone();
        let interval = cfg.progress_interval;
        let claimant = claimant.to_string();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await;
            loop {
                tick.tick().await;
                let done = counter.load(Ordering::Relaxed);
                match meta
                    .update_enable_job_progress(job_id, &claimant, done, None)
                    .await
                {
                    Ok(()) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable progress checkpoint lost the lease; stopping ticker");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%job_id, error = %e, "enable progress checkpoint failed");
                    }
                }
            }
        })
    };
    let materialize_result =
        materialize_disk_chunks(state, &image_uri, &artifacts, Some(counter.clone())).await;
    ticker.abort();
    row.disk_manifest = materialize_result.map_err(|e| AdvanceError::Pipeline(Box::new(e)))?;
    // Final progress write so the bar lands on 100% even if the last
    // ticker tick raced the abort.
    let done = counter.load(Ordering::Relaxed);
    state
        .services
        .meta
        .update_enable_job_progress(job_id, claimant, done, None)
        .await?;

    // ---- capturing ----
    state
        .services
        .meta
        .set_enable_job_state(job_id, claimant, EnableJobState::Capturing)
        .await?;
    // Issue #539: `build_base_snapshot` now streams `CaptureProgress` at
    // least every 30s (host keepalive) for the whole capture, so THIS
    // replaces the old blind lease-renewal ticker (deleted — it used to
    // re-write the static `done` count purely to keep the claim alive):
    // every progress write doubles as the renewal
    // (`update_enable_job_capture_progress` bumps `claimed_at`), so a
    // `[warm]`-hook capture that runs tens of minutes past `lease_secs`
    // still holds its claim — and renewals stop exactly when the stream
    // dies (a transport failure), letting a peer legitimately re-claim
    // instead of racing a still-healthy owner.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<engram_core::types::CaptureProgress>(64);
    let progress_consumer = {
        let meta = state.services.meta.clone();
        let claimant = claimant.to_string();
        tokio::spawn(async move {
            while let Some(event) = progress_rx.recv().await {
                match meta
                    .update_enable_job_capture_progress(job_id, &claimant, &event)
                    .await
                {
                    Ok(()) => {}
                    Err(MetaError::Conflict(msg)) => {
                        tracing::warn!(%job_id, reason = %msg, "enable capture progress write lost the lease; abandoning to peer");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%job_id, error = %e, "enable capture progress write failed");
                    }
                }
            }
        })
    };
    let capture_result =
        capture_and_record_base_snapshot(state, &row, &manifest, progress_tx).await;
    // `capture_and_record_base_snapshot` returning means every `Sender`
    // clone it (or the host RPC underneath it) held has been dropped —
    // awaiting the consumer here guarantees every progress event,
    // INCLUDING the very last one written right before a failure, is
    // persisted before we act on `capture_result`. This is load-bearing
    // for the "failing stage + tail survive a WarmExecTransport kill"
    // acceptance criterion: `record_enable_job_failure` (below) never
    // touches these columns itself — it relies on this write having
    // already landed.
    let _ = progress_consumer.await;
    let (base_snapshot_id, base_snapshot_disk_manifest, base_snapshot_memory_manifest) =
        capture_result.map_err(classify_capture_error)?;
    row.base_snapshot_id = Some(base_snapshot_id);
    row.base_snapshot_disk_manifest = Some(base_snapshot_disk_manifest);
    // `None` for cold-boot backends (VZ) — no memory snapshot to stamp.
    row.base_snapshot_memory_manifest = base_snapshot_memory_manifest;

    // ---- ready ----
    state
        .services
        .meta
        .upsert_enabled_image(row)
        .await
        .map_err(AdvanceError::from)?;
    state
        .services
        .meta
        .set_enable_job_state(job_id, claimant, EnableJobState::Ready)
        .await?;
    tracing::info!(%job_id, %image_uri, "enable job ready; image enabled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The scanner's full loop exercises Postgres + a registry + a
    // capture host; deterministic end-to-end coverage lives in the
    // live-PG integration test `enable_jobs_live_pg` (claim/lease
    // semantics, re-POST dedup, failure budget, plus the #232
    // two-claimant fencing test) and the FC e2e suite (bake → push →
    // POST 202 → poll → boot). The per-step plumbing
    // (`materialize_chunk_blob`, capture reuse) is unit-tested in
    // `api::enabled_images`.

    // #232: the worker's abandon-vs-record decision hinges entirely on
    // how a store error maps into `AdvanceError`. A fenced write that
    // lost the lease returns `MetaError::Conflict`; that MUST become
    // `LeaseLost` so `run_once` abandons the job WITHOUT calling
    // `record_enable_job_failure` (which would clear the new
    // claimant's lease + burn its attempts budget). Every other
    // `MetaError`, and every pipeline error, must stay a `Pipeline`
    // error so the budget path still runs.
    #[test]
    fn lease_conflict_maps_to_lease_lost_not_pipeline() {
        match AdvanceError::from(MetaError::Conflict("lease lost: held by pod-b".into())) {
            AdvanceError::LeaseLost(msg) => assert!(msg.contains("pod-b")),
            AdvanceError::Pipeline(_) | AdvanceError::NonRetryable(_) => {
                panic!("a lost-lease Conflict must NOT enter the failure-budget path")
            }
        }
    }

    #[test]
    fn other_meta_errors_map_to_pipeline() {
        for e in [
            MetaError::NotFound,
            MetaError::Db("connection reset".into()),
            MetaError::Migration("schema drift".into()),
        ] {
            match AdvanceError::from(e) {
                AdvanceError::Pipeline(_) => {}
                AdvanceError::LeaseLost(_) | AdvanceError::NonRetryable(_) => {
                    panic!("only a Conflict should abandon; other errors retry via the budget")
                }
            }
        }
    }

    // 1b: a capture failure must be classified so a transient NoCapacity
    // retries but a deterministic warm-hook/capture failure bails fast.
    #[test]
    fn capture_no_capacity_is_retryable_but_internal_is_not() {
        // `ApiError::Unavailable` == the capture-host picker's NoCapacity.
        match classify_capture_error(crate::error::ApiError::Unavailable(
            "no host available (NoCapacity)".into(),
        )) {
            AdvanceError::Pipeline(_) => {}
            _ => panic!("NoCapacity is transient — it must retry via the budget"),
        }
        // `ApiError::Internal` == a [warm] hook non-zero exit / verify-fail.
        match classify_capture_error(crate::error::ApiError::Internal(
            "[warm] hook exited with status Some(1)".into(),
        )) {
            AdvanceError::NonRetryable(_) => {}
            _ => panic!("a deterministic capture failure must bail fast, not retry"),
        }
    }
}
