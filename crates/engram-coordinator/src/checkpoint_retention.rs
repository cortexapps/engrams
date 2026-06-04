//! ADR 0028 Fix A: checkpoint retention sweeper.
//!
//! Periodic checkpoints add one `snapshots` row per session per
//! cadence interval (~60/hour). At rest the chunk store dedups, so
//! the marginal storage is just inter-checkpoint divergence — but the
//! rows themselves (and the chunks unique to aged-out checkpoints)
//! need a retention policy or they grow without bound.
//!
//! Policy (also the ADR 0022 forkable-history window):
//! - the **latest checkpoint per session is always kept** — it's the
//!   rung-1 recovery anchor, never collectible while the row exists;
//! - older checkpoints inside `ENGRAM_CHECKPOINT_RETENTION_HOURS`
//!   (default 24) are kept — restorable / forkable history;
//! - older-than-window rows are deleted; chunks they exclusively
//!   referenced become GC candidates under the existing pin-set
//!   machinery (ADR 0016 Phase C) and its 24 h grace.
//! - template snapshots (`session_id IS NULL`, the enabled-image base
//!   captures) are exempt — their lifecycle belongs to
//!   `enabled_images`.
//!
//! Single-coord-pod safe the same way the other sweepers are: the
//! DELETE is one idempotent statement; two pods racing just means one
//! deletes zero rows.

use std::time::Duration;

use crate::state::SharedState;

#[derive(Clone, Debug)]
pub struct CheckpointRetentionConfig {
    /// Sweep cadence. Coarse — retention is measured in hours.
    pub poll_interval: Duration,
    /// How much per-session checkpoint history to keep (beyond the
    /// always-kept latest).
    pub retention: Duration,
}

impl Default for CheckpointRetentionConfig {
    fn default() -> Self {
        let hours = std::env::var("ENGRAM_CHECKPOINT_RETENTION_HOURS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(24);
        Self {
            poll_interval: Duration::from_secs(600),
            retention: Duration::from_secs(hours * 3600),
        }
    }
}

/// Spawn the sweeper. Mirrors [`crate::evac_resumer::spawn`].
pub fn spawn(cfg: CheckpointRetentionConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            match state
                .services
                .meta
                .prune_session_snapshots(
                    chrono::Duration::from_std(cfg.retention)
                        .unwrap_or_else(|_| chrono::Duration::hours(24)),
                )
                .await
            {
                Ok(deleted) if deleted.is_empty() => {}
                Ok(deleted) => {
                    // The generation bump above makes the chunks/manifests
                    // these rows exclusively referenced GC candidates, but
                    // the per-snapshot portable blobs (`snapshots/<id>/`
                    // state.bin + sidecar) live outside the chunk-GC
                    // namespace and would otherwise orphan forever. Delete
                    // them here, now that the rows (and thus any resume
                    // that could reference them) are gone. Best-effort: a
                    // missing object (memory-less snapshot never uploaded
                    // one, or abort already removed it) is a harmless 404.
                    let mut blobs_deleted = 0usize;
                    for sid in &deleted {
                        for key in [
                            engram_chunk_store::snapshot_blob::state_blob_key(*sid),
                            engram_chunk_store::snapshot_blob::sidecar_blob_key(*sid),
                        ] {
                            match state.services.blob.delete(&key).await {
                                Ok(()) => blobs_deleted += 1,
                                Err(e) => tracing::debug!(
                                    snapshot_id = %sid,
                                    key = %key,
                                    error = %e,
                                    "checkpoint retention: portable blob delete failed \
                                     (best-effort; object may not exist)",
                                ),
                            }
                        }
                    }
                    tracing::info!(
                        deleted = deleted.len(),
                        blobs_deleted,
                        retention_secs = cfg.retention.as_secs(),
                        "checkpoint retention sweep pruned aged-out session snapshots + portable blobs",
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "checkpoint retention sweep failed; will retry");
                }
            }
        }
    })
}
