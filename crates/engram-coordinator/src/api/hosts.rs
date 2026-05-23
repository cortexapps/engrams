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
use engram_core::types::host::HostStatus;
use engram_core::HostId;
use serde::Serialize;

use crate::cow_state::{fetch_for_host, CowStateView};
use crate::error::ApiError;
use crate::state::SharedState;

/// `GET /api/hosts` — list all hosts the coordinator knows about,
/// merging persisted Postgres rows with live in-memory scheduler
/// state (capacity, local snapshots, draining).
pub async fn list(State(state): State<SharedState>) -> Result<Json<ListHostsResponse>, ApiError> {
    let rows = state.services.meta.list_active_hosts().await?;
    let hosts = rows
        .into_iter()
        .map(|row| {
            let live = state.host_registry.snapshot_state(row.id);
            HostView::from_row_and_live(row, live)
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
    let live = state.host_registry.snapshot_state(host_id);
    Ok(Json(HostView::from_row_and_live(row, live)))
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
async fn enrichment_for_session(
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

/// `POST /api/hosts/:id/drain`. Flips both the Postgres row and the
/// in-memory scheduler view to Draining; new sessions won't be
/// assigned to this host. In-flight sessions stay put.
pub async fn drain(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
) -> Result<StatusCode, ApiError> {
    state
        .services
        .meta
        .set_host_status(host_id, HostStatus::Draining)
        .await?;
    if let Some(mut s) = state.host_registry.snapshot_state(host_id) {
        s.draining = true;
        state.host_registry.update_state(host_id, s);
    }
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
    pub last_heartbeat_at: DateTime<Utc>,
}

impl HostView {
    fn from_row_and_live(
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
        let (capacity_total_mib, capacity_used_mib, running_sandboxes) =
            if row.capacity.total_mib > 0 {
                (
                    row.capacity.total_mib,
                    row.capacity.used_mib,
                    row.capacity.running_sandboxes,
                )
            } else {
                (
                    live.capacity.total_mib,
                    live.capacity.used_mib,
                    live.capacity.running_sandboxes,
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
