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
    /// claim renewals — progress checkpoints renew every
    /// `progress_interval` during materialize, and the capture step
    /// renews once at entry but can then run ~3 min on dev-vm-class
    /// hosts without touching the row.
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
        if let Err(e) = advance_one(cfg, state, job).await {
            // Per-job failure: bump attempts + store the error; flip
            // to failed once the budget is spent. Keep sweeping —
            // one wedged job must not stall the queue.
            let msg = e.to_string();
            tracing::warn!(%job_id, error = %msg, "enable job pipeline failed");
            match state
                .services
                .meta
                .record_enable_job_failure(job_id, &msg)
                .await
            {
                Ok(attempts) if attempts >= cfg.max_attempts => {
                    tracing::warn!(
                        %job_id,
                        attempts,
                        max_attempts = cfg.max_attempts,
                        "enable job budget exhausted; marking failed",
                    );
                    if let Err(e2) = state
                        .services
                        .meta
                        .set_enable_job_state(job_id, EnableJobState::Failed)
                        .await
                    {
                        tracing::warn!(%job_id, error = %e2, "failed to mark enable job failed");
                    }
                }
                Ok(_) => {}
                Err(e2) => {
                    tracing::warn!(%job_id, error = %e2, "failed to record enable job failure");
                }
            }
        }
    }
    Ok(())
}

/// Run the enable pipeline for one claimed job. Every step is
/// idempotent, so this always starts from the top and fast-forwards:
/// metadata pull is KBs, materialize skips present chunks, capture
/// reuses a digest-matched base snapshot.
async fn advance_one(
    cfg: &EnableScannerConfig,
    state: &SharedState,
    job: EnableJob,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let job_id = job.id;
    let image_uri = job.image_uri.clone();
    tracing::info!(
        %job_id,
        %image_uri,
        from_state = job.state.as_str(),
        attempt = job.attempts + 1,
        "enable-scanner: driving job",
    );

    let (mut row, manifest, artifacts) = fetch_and_seal_manifest(state, &image_uri).await?;

    // ---- materializing ----
    state
        .services
        .meta
        .set_enable_job_state(job_id, EnableJobState::Materializing)
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
        .update_enable_job_progress(job_id, 0, chunks_total)
        .await?;

    // Checkpoint task: persists the counter every couple of seconds.
    // Doubles as the claim renewal during a long materialize.
    let ticker = {
        let meta = state.services.meta.clone();
        let counter = counter.clone();
        let interval = cfg.progress_interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await;
            loop {
                tick.tick().await;
                let done = counter.load(Ordering::Relaxed);
                if let Err(e) = meta.update_enable_job_progress(job_id, done, None).await {
                    tracing::debug!(%job_id, error = %e, "enable progress checkpoint failed");
                }
            }
        })
    };
    let materialize_result =
        materialize_disk_chunks(state, &image_uri, &artifacts, Some(counter.clone())).await;
    ticker.abort();
    row.disk_manifest = materialize_result?;
    // Final progress write so the bar lands on 100% even if the last
    // ticker tick raced the abort.
    let done = counter.load(Ordering::Relaxed);
    state
        .services
        .meta
        .update_enable_job_progress(job_id, done, None)
        .await?;

    // ---- capturing ----
    state
        .services
        .meta
        .set_enable_job_state(job_id, EnableJobState::Capturing)
        .await?;
    let (base_snapshot_id, base_snapshot_disk_manifest, base_snapshot_memory_manifest) =
        capture_and_record_base_snapshot(state, &row, &manifest).await?;
    row.base_snapshot_id = Some(base_snapshot_id);
    row.base_snapshot_disk_manifest = Some(base_snapshot_disk_manifest);
    // `None` for cold-boot backends (VZ) — no memory snapshot to stamp.
    row.base_snapshot_memory_manifest = base_snapshot_memory_manifest;

    // ---- ready ----
    state.services.meta.upsert_enabled_image(row).await?;
    state
        .services
        .meta
        .set_enable_job_state(job_id, EnableJobState::Ready)
        .await?;
    tracing::info!(%job_id, %image_uri, "enable job ready; image enabled");
    Ok(())
}

#[cfg(test)]
mod tests {
    // The scanner's full loop exercises Postgres + a registry + a
    // capture host; deterministic end-to-end coverage lives in the
    // live-PG integration test `enable_jobs_live_pg` (claim/lease
    // semantics, re-POST dedup, failure budget) and the FC e2e suite
    // (bake → push → POST 202 → poll → boot). The per-step plumbing
    // (`materialize_chunk_blob`, capture reuse) is unit-tested in
    // `api::enabled_images`.
}
