//! ADR 0029 — the read-side surface behind the web app's **Storage**
//! page (copy-on-write durability + chunk-store rollups).
//!
//! `GET /api/storage/summary` aggregates, in one call:
//!
//! - the per-sandbox COW ledger across **every** registered host
//!   (dirty chunks, unflushed bytes, base-chunk locality, last flush),
//! - fleet-wide rollups derived from that ledger, and
//! - cheap Postgres counts (snapshot count + bytes, GC-pending).
//!
//! It reuses [`crate::cow_state::CowStateCache`] (1s TTL), so the
//! Storage page's slow poll never fans a host RPC per request. It does
//! **not** list the content-addressed blob store — "chunks stored" and
//! "dedup ratio" would each require an O(objects) walk, which would put
//! diagnostic load on the workload's hot path. Those two cells are
//! intentionally absent until a maintained counter exists (see the ADR).
//!
//! A host that fails its COW RPC is skipped (logged at debug), not
//! fatal — a single flaky host must not 500 the whole page.

use std::collections::HashMap;

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use engram_core::types::cow_state::unix_ms_to_dt;
use engram_core::{HostId, SandboxId, SessionId};
use serde::Serialize;

use crate::cow_state::fetch_for_host;
use crate::error::ApiError;
use crate::state::SharedState;

/// One per-sandbox row of the durability ledger.
#[derive(Serialize)]
pub struct DurabilityRow {
    pub sandbox_id: SandboxId,
    pub session_id: Option<SessionId>,
    pub host_id: HostId,
    /// Dirty chunks resident in host RAM, not yet flushed to BlobStorage.
    pub dirty_chunks: u32,
    pub dirty_bytes: u64,
    /// Total chunks the live disk manifest references.
    pub base_chunks: u32,
    /// Of `base_chunks`, how many are resident on the host's NVMe cache.
    pub base_chunks_local: u32,
    /// ISO-8601 of the last successful flush; `null` = never flushed.
    pub last_flush_at: Option<DateTime<Utc>>,
}

/// `GET /api/storage/summary` response.
#[derive(Serialize)]
pub struct StorageSummaryResponse {
    // ---- fleet-wide rollups ----
    /// `snapshots` row count (Postgres).
    pub snapshots: u64,
    /// Sum of `snapshots.size_bytes` (Postgres).
    pub snapshot_bytes: u64,
    /// Chunks parked in `chunk_gc_candidates` awaiting their grace window.
    pub gc_pending: u64,
    /// Number of chunk-tracked sandboxes across the fleet (ledger length).
    pub tracked_sandboxes: u64,
    /// Sum of dirty chunks across the ledger.
    pub dirty_chunks: u64,
    /// Sum of dirty (unflushed) bytes across the ledger.
    pub unflushed_bytes: u64,
    /// Mean base-chunk locality (%) over sandboxes that reference any
    /// base chunks. `0` when nothing is tracked yet.
    pub avg_locality_pct: u32,
    // ---- per-sandbox detail ----
    pub rows: Vec<DurabilityRow>,
}

/// Transport-agnostic core for GetStorageSummary.
pub(crate) async fn storage_summary_core(
    state: &crate::state::SharedState,
) -> Result<StorageSummaryResponse, ApiError> {
    let hosts = state.services.meta.list_active_hosts().await?;

    let mut rows: Vec<DurabilityRow> = Vec::new();
    let mut dirty_chunks: u64 = 0;
    let mut unflushed_bytes: u64 = 0;
    let mut locality_sum: f64 = 0.0;
    let mut locality_n: u64 = 0;

    for host in hosts {
        let host_id = host.id;
        // A host with no live backend (registered in PG but not in the
        // in-memory registry on this replica) contributes no live COW.
        let Some(backend) = state.host_registry.backend_of(host_id) else {
            continue;
        };
        let records = match fetch_for_host(&state.cow_state_cache, host_id, backend).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(%host_id, error = %e, "storage summary: host COW RPC failed; skipping");
                continue;
            }
        };

        // sandbox → session map for this host (indexed PG query, bounded
        // by sandboxes/host). A failure here just drops the join.
        let session_for: HashMap<SandboxId, SessionId> = state
            .services
            .meta
            .list_active_sandbox_assignments_on_host(host_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(sid, sb)| (sb, sid))
            .collect();

        for record in records {
            let s = &record.state;
            dirty_chunks += s.dirty_chunks as u64;
            unflushed_bytes += s.dirty_bytes;
            if s.base_chunks > 0 {
                locality_sum += (s.base_chunks_local as f64 / s.base_chunks as f64) * 100.0;
                locality_n += 1;
            }
            rows.push(DurabilityRow {
                sandbox_id: record.sandbox_id,
                session_id: session_for.get(&record.sandbox_id).copied(),
                host_id,
                dirty_chunks: s.dirty_chunks,
                dirty_bytes: s.dirty_bytes,
                base_chunks: s.base_chunks,
                base_chunks_local: s.base_chunks_local,
                last_flush_at: unix_ms_to_dt(s.last_flush_unix_ms),
            });
        }
    }

    let avg_locality_pct = if locality_n > 0 {
        (locality_sum / locality_n as f64).round() as u32
    } else {
        0
    };

    let totals = state.services.meta.snapshot_totals().await?;
    let gc_pending = state.services.meta.count_gc_candidates().await?;

    Ok(StorageSummaryResponse {
        snapshots: totals.count,
        snapshot_bytes: totals.total_bytes,
        gc_pending,
        tracked_sandboxes: rows.len() as u64,
        dirty_chunks,
        unflushed_bytes,
        avg_locality_pct,
        rows,
    })
}

pub async fn summary(
    State(state): State<SharedState>,
) -> Result<Json<StorageSummaryResponse>, ApiError> {
    Ok(Json(storage_summary_core(&state).await?))
}
