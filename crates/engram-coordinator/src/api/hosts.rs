//! Read-only host listing endpoints. ADR 0013 retired the
//! bincode-over-WebSocket `connect` handler — host registration +
//! heartbeats + auth + harness events all live in
//! [`super::host_http`] now as plain HTTP POSTs that any coord
//! pod can serve. What remains here is the read-side surface for
//! the SPA / operator CLI:
//!
//! - `GET /api/hosts` — list every host with merged PG-row +
//!   in-memory scheduler state.
//! - `GET /api/hosts/:id` — one host's view.
//! - `POST /api/hosts/:id/drain` — flip the row + scheduler view
//!   to Draining so the scheduler stops picking it.
//! - `GET /api/hosts/:id/cow-state` — ADR 0016 Phase A: per-sandbox
//!   COW diagnostic for every chunk-tracked sandbox on this host.

use chrono::{DateTime, Utc};
use engram_core::HostId;
use serde::Serialize;

use crate::state::SharedState;

/// Memory-tier enrichment: project the session's latest snapshot row
/// into the diagnostic view's `memory_manifest` +
/// `last_snapshot_at` fields. Returns `(None, None)` when the
/// session has never been snapshotted — `CowStateView` falls back to
/// the host's in-memory `last_snapshot_unix_ms` in that case.
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

#[derive(Serialize)]
pub struct HostView {
    pub id: HostId,
    pub hostname: String,
    pub status: &'static str,
    /// ADR 0047: the coordinator-owned cordon bit — non-schedulable
    /// regardless of the host-reported `status`.
    pub cordoned: bool,
    pub capacity_total_mib: u64,
    pub capacity_used_mib: u64,
    pub running_sandboxes: u32,
    pub local_snapshots: usize,
    /// ADR 0015 M5: count of images this host has fully prefetched
    /// and is ready to serve (heartbeat-persisted; ADR 0047).
    pub ready_images: usize,
    /// ADR 0015 M5: manifest digests of the prefetched images.
    /// Exposed so callers (operators, integration tests) can
    /// poll for a specific digest's readiness without guessing
    /// from the count. Sorted for deterministic output.
    pub ready_image_digests: Vec<String>,
    /// Observed disk/mem/cpu utilization from the latest heartbeat
    /// (the fleet view's bars); 0 until the host's first
    /// post-migration heartbeat.
    pub util_disk_total_mib: u64,
    pub util_disk_used_mib: u64,
    pub util_mem_total_mib: u64,
    pub util_mem_used_mib: u64,
    pub util_cpu_pct: f32,
    /// Issue #540 (host RAM ledger attribution): measured base-shm tmpfs
    /// residency, and the running/parked split of guest PSS. `0` until
    /// the host's first post-0078 heartbeat. `base_shm_pending_mib` is
    /// deliberately absent here — it's transient host-local state
    /// already folded into `allocatable_mib`, not durable PG state.
    pub util_base_shm_mib: u64,
    pub util_parked_pss_mib: u64,
    pub util_running_pss_mib: u64,
    /// ADR 0046/0047, amended by issue #540: host-measured allocatable
    /// RAM — what placement budgets against.
    pub allocatable_mib: u64,
    /// ADR 0048: Σ reserved RAM (mem_budget_mib) and the resulting free
    /// (allocatable − reserved) — the operator's wave planner reads these
    /// for victim-picking + the 2D drain guard.
    pub reserved_mib: u64,
    pub free_mib: u64,
    /// ADR 0048: host core count + budget (`total_vcpus × overcommit`),
    /// Σ reserved vCPU, and the resulting free vCPU.
    pub total_vcpus: u32,
    pub cpu_budget_vcpus: u64,
    pub reserved_vcpus: u64,
    pub free_vcpus: u64,
    pub last_heartbeat_at: DateTime<Utc>,
    /// ADR 0068: names of the capability-vector fields that are
    /// currently `Failed` (or `Unknown` on a `schema >= 1` host) —
    /// empty on a healthy host. Kills the "no capacity with free
    /// hosts" mystery mode at the operator's fleet view, not just in
    /// the placement-exclusion log.
    pub failing_capabilities: Vec<String>,
    /// ADR 0068: this host's `firecracker --snapshot-version`
    /// (`None` off FC / not yet probed).
    pub fc_snapshot_version: Option<String>,
    /// ADR 0068: `0` = this host has never reported a capability
    /// vector (pre-0068 row, or mid-roll) — the soft-pass posture.
    /// `>= 1` once it has.
    pub capabilities_schema: u32,
}

impl HostView {
    pub(crate) fn from_row(
        row: engram_core::types::HostRecord,
        reserved: engram_core::types::host::ReservedBudget,
    ) -> Self {
        let mut ready_image_digests = row.ready_images.clone();
        ready_image_digests.sort();
        let allocatable_mib = row.utilization.allocatable_mib;
        let reserved_mib = reserved.mem_mib.max(0) as u64;
        let cpu_budget = engram_core::types::host::host_cpu_budget(row.total_vcpus).max(0) as u64;
        let reserved_vcpus = reserved.vcpus.max(0) as u64;
        let failing_capabilities = row
            .capabilities
            .failing_capabilities()
            .into_iter()
            .map(str::to_string)
            .collect();
        let fc_snapshot_version = row.capabilities.fc_snapshot_version.clone();
        let capabilities_schema = row.capabilities.schema;
        Self {
            id: row.id,
            hostname: row.hostname,
            status: row.status.as_str(),
            cordoned: row.cordoned,
            capacity_total_mib: row.capacity.total_mib,
            capacity_used_mib: row.capacity.used_mib,
            running_sandboxes: row.capacity.running_sandboxes,
            local_snapshots: row.local_snapshots.len(),
            ready_images: ready_image_digests.len(),
            ready_image_digests,
            util_disk_total_mib: row.utilization.disk_total_mib,
            util_disk_used_mib: row.utilization.disk_used_mib,
            util_mem_total_mib: row.utilization.mem_total_mib,
            util_mem_used_mib: row.utilization.mem_used_mib,
            util_cpu_pct: row.utilization.cpu_pct,
            util_base_shm_mib: row.utilization.base_shm_mib,
            util_parked_pss_mib: row.utilization.parked_pss_mib,
            util_running_pss_mib: row.utilization.running_pss_mib,
            allocatable_mib,
            reserved_mib,
            free_mib: allocatable_mib.saturating_sub(reserved_mib),
            total_vcpus: row.total_vcpus,
            cpu_budget_vcpus: cpu_budget,
            reserved_vcpus,
            free_vcpus: cpu_budget.saturating_sub(reserved_vcpus),
            last_heartbeat_at: row.last_heartbeat_at,
            failing_capabilities,
            fc_snapshot_version,
            capabilities_schema,
        }
    }
}
