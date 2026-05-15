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

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use engram_core::types::host::HostStatus;
use engram_core::HostId;
use serde::Serialize;

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
        Self {
            id: row.id,
            hostname: row.hostname,
            status: row.status.as_str(),
            capacity_total_mib,
            capacity_used_mib,
            running_sandboxes,
            local_snapshots: live.local_snapshots.len(),
            last_heartbeat_at: row.last_heartbeat_at,
        }
    }
}
