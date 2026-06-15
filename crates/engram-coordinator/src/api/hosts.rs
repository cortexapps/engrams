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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use engram_core::HostId;
use serde::Serialize;

use crate::cow_state::{fetch_for_host, CowStateView};
use crate::error::ApiError;
use crate::state::SharedState;

/// `GET /api/hosts` — list all hosts. ADR 0047: rendered from the
/// Postgres rows alone (heartbeats persist the full scheduling state),
/// so every coordinator replica serves the same view.
pub async fn list(State(state): State<SharedState>) -> Result<Json<ListHostsResponse>, ApiError> {
    let rows = state.services.meta.list_active_hosts().await?;
    // ADR 0048: the reserved aggregate (both budget dims) so the operator's
    // wave planner reads each host's free RAM/vCPU + load for victim-picking.
    let reserved = state
        .services
        .meta
        .per_host_reserved()
        .await
        .unwrap_or_default();
    let hosts = rows
        .into_iter()
        .map(|row| {
            let r = reserved.get(&row.id).copied().unwrap_or_default();
            HostView::from_row(row, r)
        })
        .collect();
    Ok(Json(ListHostsResponse { hosts }))
}

/// `GET /api/hosts/:id`. NotFound if the row isn't in Postgres.
pub async fn get(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
) -> Result<Json<HostView>, ApiError> {
    let rows = state.services.meta.list_active_hosts().await?;
    let row = rows
        .into_iter()
        .find(|r| r.id == host_id)
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let reserved = state
        .services
        .meta
        .per_host_reserved()
        .await
        .unwrap_or_default();
    let r = reserved.get(&host_id).copied().unwrap_or_default();
    Ok(Json(HostView::from_row(row, r)))
}

/// `GET /api/hosts/:id/cow-state`. ADR 0016 Phase A. Returns one
/// row per chunk-tracked sandbox on the host — each enriched with
/// its session_id (joined from `sessions.host_for_sandbox`) so the
/// web app can pivot the same payload by host or by session.
/// Backed by [`crate::cow_state::CowStateCache`] (1s TTL) so the
/// web app's 1-2s polling doesn't fan out to the host on every
/// request.
pub async fn cow_state(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
) -> Result<Json<HostCowStateResponse>, ApiError> {
    let backend = state
        .host_registry
        .backend_of(host_id)
        .ok_or_else(|| ApiError::NotFound("host not registered".into()))?;
    let records = fetch_for_host(&state.cow_state_cache, host_id, backend)
        .await
        .map_err(ApiError::from)?;

    // Build a `sandbox_id → session_id` map for this host once via
    // the M3-era `list_active_sandbox_assignments_on_host` query
    // (already indexed on `(host_id, status)` per its docstring),
    // then iterate the host's per-sandbox records. One PG round
    // trip + one PG snapshot lookup per session — bounded by N
    // sandboxes/host (tens, typical), not N total sessions.
    let assignments = state
        .services
        .meta
        .list_active_sandbox_assignments_on_host(host_id)
        .await
        .map_err(ApiError::from)?;
    let session_for: std::collections::HashMap<engram_core::SandboxId, engram_core::SessionId> =
        assignments.into_iter().map(|(sid, sb)| (sb, sid)).collect();

    let mut sessions = Vec::with_capacity(records.len());
    for record in records {
        let session_id = session_for.get(&record.sandbox_id).copied();
        // Memory-tier enrichment: project the session's latest
        // snapshot row into `memory_manifest` + `last_snapshot_at`.
        // No snapshot → `CowStateView` falls back to the host's
        // `last_snapshot_unix_ms` (often also `0`, rendered as
        // "never" by the consumer).
        let (memory_manifest, last_snapshot_at) = match session_id {
            Some(sid) => enrichment_for_session(&state, sid).await,
            None => (None, None),
        };
        sessions.push(CowStateView::from_record(
            &record,
            session_id,
            memory_manifest,
            last_snapshot_at,
        ));
    }
    Ok(Json(HostCowStateResponse { host_id, sessions }))
}

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

/// `POST /api/hosts/:id/drain`. ADR 0047: a durable coordinator-side
/// cordon (`hosts.cordoned`) — heartbeats can't clobber it back to
/// schedulable; placement reads the bit from PG on every pick. New
/// sessions won't be assigned to this host; in-flight sessions stay.
pub async fn drain(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
) -> Result<StatusCode, ApiError> {
    state.services.meta.set_host_cordoned(host_id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct ListHostsResponse {
    pub hosts: Vec<HostView>,
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
    /// ADR 0046/0047: host-measured allocatable RAM — what placement
    /// budgets against.
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
            allocatable_mib,
            reserved_mib,
            free_mib: allocatable_mib.saturating_sub(reserved_mib),
            total_vcpus: row.total_vcpus,
            cpu_budget_vcpus: cpu_budget,
            reserved_vcpus,
            free_vcpus: cpu_budget.saturating_sub(reserved_vcpus),
            last_heartbeat_at: row.last_heartbeat_at,
        }
    }
}

/// `GET /api/hosts/:id/cow-state` response shape (ADR 0016 Phase A).
#[derive(Serialize)]
pub struct HostCowStateResponse {
    pub host_id: HostId,
    /// One entry per chunk-tracked sandbox on this host. Sandboxes
    /// without a chunk view (Process backend, VZ-without-NBD) are
    /// omitted by the host-agent — they don't appear here either.
    pub sessions: Vec<CowStateView>,
}
