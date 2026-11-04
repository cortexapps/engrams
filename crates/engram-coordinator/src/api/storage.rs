//! The read-side surface behind the web app's Storage page: snapshot
//! totals and the chunk-GC backlog, both cheap Postgres counts.

use crate::error::ApiError;
use crate::state::SharedState;

/// `GetStorageSummary` response.
pub struct StorageSummaryResponse {
    /// `snapshots` row count.
    pub snapshots: u64,
    /// Sum of `snapshots.size_bytes`.
    pub snapshot_bytes: u64,
    /// Chunks parked in `chunk_gc_candidates` awaiting their grace window.
    pub gc_pending: u64,
    /// False when `gc_pending` is an estimate: the table passed the
    /// exact-count cap (`GC_CANDIDATE_EXACT_CAP`).
    pub gc_pending_exact: bool,
}

pub(crate) async fn storage_summary_core(
    state: &SharedState,
) -> Result<StorageSummaryResponse, ApiError> {
    let totals = state.services.meta.snapshot_totals().await?;
    let backlog = state
        .services
        .meta
        .count_gc_candidates(engram_core::traits::GC_CANDIDATE_EXACT_CAP)
        .await?;
    Ok(StorageSummaryResponse {
        snapshots: totals.count,
        snapshot_bytes: totals.total_bytes,
        gc_pending: backlog.count,
        gc_pending_exact: backlog.exact,
    })
}
