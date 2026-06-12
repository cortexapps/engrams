//! Host types and helpers used by the gRPC fleet service and the
//! internal host-http routes.
//!
//! ADR 0039 Task 32: the axum shims `list`, `get`, `cow_state`, and `drain`
//! are removed. The gRPC FleetService now owns all fleet read/write surface.
//! `HostView`, `enrichment_for_session`, and `from_row_and_live` remain,
//! used by gRPC fleet handlers.

use chrono::{DateTime, Utc};
use engram_core::HostId;
use serde::Serialize;

use crate::state::SharedState;

/// Memory-tier enrichment: project the session's latest snapshot row
/// into the diagnostic view's `memory_manifest` +
/// `last_snapshot_at` fields. Returns `(None, None)` when the
/// session has never been snapshotted — `CowStateView` falls back to
/// the host's in-memory `last_snapshot_unix_ms` in that case.
///
/// `pub(crate)` so `grpc_app/fleet.rs::get_host_cow_state` can reuse
/// it rather than re-inlining the same logic.
pub(crate) async fn enrichment_for_session(
    state: &SharedState,
    session_id: engram_core::SessionId,
) -> (
    Option<engram_core::types::manifest::ManifestRef>,
    Option<chrono::DateTime<chrono::Utc>>,
) {
    match state
        .services
        .meta
        .latest_snapshot_for_session(session_id)
        .await
    {
        Ok(Some(rec)) => (rec.memory_manifest, Some(rec.created_at)),
        Ok(None) => (None, None),
        Err(e) => {
            tracing::debug!(
                %session_id,
                error = %e,
                "cow-state enrichment: snapshot lookup failed; rendering without memory tier",
            );
            (None, None)
        }
    }
}

// ADR 0039 Task 32: `drain` axum shim removed.
// `ListHostsResponse` and `HostCowStateResponse` removed (axum-only types).
// The gRPC FleetService uses proto-generated `app::ListHostsResponse` /
// `app::GetHostCowStateResponse` instead.

#[derive(Serialize)]
pub struct HostView {
    pub id: HostId,
    pub hostname: String,
    pub status: &'static str,
    pub capacity_total_mib: u64,
    pub capacity_used_mib: u64,
    pub running_sandboxes: u32,
    pub local_snapshots: usize,
    /// ADR 0015 M5: count of images this host has fully
    /// prefetched and is ready to serve. Live-only — reads as 0
    /// on a coord replica that hasn't received a heartbeat yet.
    pub ready_images: usize,
    /// ADR 0015 M5: manifest digests of the prefetched images.
    /// Exposed so callers (operators, integration tests) can
    /// poll for a specific digest's readiness without guessing
    /// from the count. Sorted for deterministic output.
    pub ready_image_digests: Vec<String>,
    /// Observed disk/mem/cpu utilization from the latest heartbeat
    /// (the fleet view's bars). Like capacity, these prefer the
    /// persisted `hosts` row so they're consistent across coord
    /// replicas; 0 until the host's first post-migration heartbeat.
    pub util_disk_total_mib: u64,
    pub util_disk_used_mib: u64,
    pub util_mem_total_mib: u64,
    pub util_mem_used_mib: u64,
    pub util_cpu_pct: f32,
    pub last_heartbeat_at: DateTime<Utc>,
}

impl HostView {
    pub(crate) fn from_row_and_live(
        row: engram_core::types::HostRecord,
        live: Option<crate::host_registry::HostState>,
    ) -> Self {
        // Capacity prefers the Postgres row (consistent across coord
        // replicas — persisted on every heartbeat). If `live` is
        // present *and* the row's MiB total is zero (pre-migration
        // row, never had a fresh heartbeat write), fall back to the
        // in-memory value so the operator isn't stuck staring at 0
        // during a single-replica deploy or right after the
        // migration runs.
        //
        // `local_snapshots` stays live-only — the count isn't
        // persisted yet. It'll read as 0 on the heartbeat-non-owning
        // pod, which matches the existing pre-MiB-fields behaviour.
        let live = live.unwrap_or_default();
        // Same row-vs-live preference as capacity: the row is written
        // on every heartbeat and is replica-consistent; fall back to
        // the in-memory value only for a pre-MiB-fields row that has
        // never had a fresh heartbeat write.
        let (capacity_total_mib, capacity_used_mib, running_sandboxes, util) =
            if row.capacity.total_mib > 0 {
                (
                    row.capacity.total_mib,
                    row.capacity.used_mib,
                    row.capacity.running_sandboxes,
                    row.utilization.clone(),
                )
            } else {
                (
                    live.capacity.total_mib,
                    live.capacity.used_mib,
                    live.capacity.running_sandboxes,
                    live.utilization.clone(),
                )
            };
        let ready_images = live.ready_images.len();
        let mut ready_image_digests: Vec<String> = live
            .ready_images
            .iter()
            .map(|d| d.as_str().to_string())
            .collect();
        ready_image_digests.sort();
        Self {
            id: row.id,
            hostname: row.hostname,
            status: row.status.as_str(),
            capacity_total_mib,
            capacity_used_mib,
            running_sandboxes,
            local_snapshots: live.local_snapshots.len(),
            ready_images,
            ready_image_digests,
            util_disk_total_mib: util.disk_total_mib,
            util_disk_used_mib: util.disk_used_mib,
            util_mem_total_mib: util.mem_total_mib,
            util_mem_used_mib: util.mem_used_mib,
            util_cpu_pct: util.cpu_pct,
            last_heartbeat_at: row.last_heartbeat_at,
        }
    }
}

// ADR 0039 Task 32: `HostCowStateResponse` removed (axum-only type).
// The gRPC FleetService uses proto-generated `app::GetHostCowStateResponse`.
