//! Orphaned base-snapshot reaper.
//!
//! Each enabled-image re-bake/refresh captures a fresh per-image base
//! snapshot and swaps `enabled_images.base_snapshot_id` to it
//! (`upsert_enabled_image`'s `ON CONFLICT`). The PRIOR base row is left
//! dangling — `session_id IS NULL`, referenced by no `enabled_images`
//! row — and nothing else deletes it: `prune_session_snapshots`
//! (checkpoint retention) is `session_id IS NOT NULL` only, and an orphan
//! base keeps pinning its own disk+memory chunks via pin-set sources #3/#4
//! (`engram-postgres`'s `list_recoverable_snapshot_{disk,memory}_manifests`,
//! which filter on `recoverable = TRUE` with NO `session_id` predicate).
//! So without this sweeper every image refresh permanently leaks one base
//! snapshot's worth of chunks (20–32 GB for the heavy dogfood images).
//!
//! Policy: delete `session_id IS NULL` snapshots older than
//! `ENGRAM_BASE_SNAPSHOT_RETENTION_HOURS` (default 24) that are referenced
//! by NO `enabled_images.base_snapshot_id` — live OR soft-deleted (a
//! soft-deleted image's base is still chunk-lineage-pinned, ADR 0021 P1.8).
//! The grace window guards a refresh-in-progress and gives operators a
//! rollback gap. Deletion bumps `chunk_generation`; the existing chunk-GC
//! (ADR 0016 Phase C) and snapshot-blob-GC (ADR 0028 addendum) sweeps then
//! reclaim the now-unpinned chunks and portable `snapshots/<id>/` blobs —
//! this sweeper deletes nothing in BlobStorage directly.
//!
//! Single-coord-pod safe like the sibling sweepers: the DELETE is one
//! idempotent statement; two pods racing just means one deletes zero rows.

use std::time::Duration;

use crate::state::SharedState;

#[derive(Clone, Debug)]
pub struct BaseSnapshotRetentionConfig {
    /// Sweep cadence. Coarse — base snapshots only change on image refresh.
    pub poll_interval: Duration,
    /// How long a superseded base snapshot lingers before it is reaped.
    pub grace: Duration,
}

impl Default for BaseSnapshotRetentionConfig {
    fn default() -> Self {
        let hours = std::env::var("ENGRAM_BASE_SNAPSHOT_RETENTION_HOURS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(24);
        Self {
            poll_interval: Duration::from_secs(3600),
            grace: Duration::from_secs(hours * 3600),
        }
    }
}

/// Spawn the sweeper. Mirrors [`crate::checkpoint_retention::spawn`].
pub fn spawn(cfg: BaseSnapshotRetentionConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "base-snapshot retention sweep failed; will retry");
            }
        }
    })
}

/// Run one retention sweep. Production calls this from [`spawn`]; the
/// deterministic simulator calls it directly without a timer loop.
pub async fn run_once(
    cfg: &BaseSnapshotRetentionConfig,
    state: &SharedState,
) -> Result<usize, engram_core::MetaError> {
    let deleted = state
        .services
        .meta
        .prune_orphan_base_snapshots(
            chrono::Duration::from_std(cfg.grace).unwrap_or_else(|_| chrono::Duration::hours(24)),
        )
        .await?;
    if !deleted.is_empty() {
        // chunk-GC + snapshot-blob-GC reclaim the freed chunks and
        // portable blobs on their next ticks; this only deletes PG rows.
        tracing::info!(
            deleted = deleted.len(),
            grace_secs = cfg.grace.as_secs(),
            "base-snapshot retention sweep reaped orphaned image base snapshots",
        );
    }
    Ok(deleted.len())
}
