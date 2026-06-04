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
//!   machinery (ADR 0016 Phase C) and its 24 h grace. Their portable
//!   `snapshots/<id>/` blobs (state.bin / sidecar) likewise become
//!   unpinned and are reaped by the snapshot-blob GC sweep (ADR 0028
//!   addendum) — this sweeper no longer deletes them inline.
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
                    // The generation bump inside `prune_session_snapshots`
                    // re-classifies the chunks/manifests these rows
                    // exclusively referenced. The per-snapshot portable
                    // blobs (`snapshots/<id>/`) are now unpinned (no row)
                    // and reaped by the snapshot-blob GC sweep on its next
                    // tick — we no longer delete them inline here (ADR 0028
                    // addendum: portable blobs are pin-set-governed, so
                    // nothing but the sweep deletes a durable snapshot
                    // blob).
                    tracing::info!(
                        deleted = deleted.len(),
                        retention_secs = cfg.retention.as_secs(),
                        "checkpoint retention sweep pruned aged-out session snapshots",
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "checkpoint retention sweep failed; will retry");
                }
            }
        }
    })
}
