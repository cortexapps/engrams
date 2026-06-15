//! `GET /sessions/:id/log` — conversation log handler.
//!
//! ADR 0005 retired the git-shaped surfaces (`?kind=workspace`,
//! `/diff`, `/fork`, `/checkpoint`) along with the `git_workdir`
//! coord-side bare-clone manager that backed them. The remaining log
//! handler reads `session_events` straight from Postgres.
//!
//! `?kind=` is kept as a query parameter solely so callers that hit
//! `?kind=conversation` (the only valid value, also the default) keep
//! working without a 400. Any other value returns 400 — the workspace
//! variant is gone.

use chrono::Utc;
use engram_core::types::SessionState;
use engram_core::SessionId;
use serde::Serialize;

use crate::cow_state::{fetch_for_host, CowStateView};
use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Serialize)]
pub struct ConversationEntry {
    pub idx: i64,
    pub kind: String,
    pub at: chrono::DateTime<Utc>,
    pub payload: serde_json::Value,
}

/// ADR 0028 A.log: one checkpoint in a session's chain — the data
/// behind the durability timeline + the (future) fork-point picker.
#[derive(Serialize)]
pub struct CheckpointSummary {
    pub snapshot_id: String,
    pub created_at: chrono::DateTime<Utc>,
    pub size_bytes: u64,
    /// The (memory, disk, event-log) coherence triple's third leg —
    /// the transcript cursor a rung-1 rewind / fork would cut at.
    pub events_cursor: Option<i64>,
    /// HEAD-verified durable in BlobStorage (rung-1-eligible).
    pub recoverable: bool,
    /// True for the newest checkpoint — the always-pinned rung-1
    /// recovery anchor.
    pub is_latest: bool,
}

// ----------------------------------------------------------------
// ADR 0051: transport-agnostic `_core` entry points for the app-gRPC
// SessionService. Same logic as the (now-unrouted) axum handlers above,
// reshaped to return the plain bodies the gRPC converters consume.
// ----------------------------------------------------------------

/// gRPC `GetLog` core. `kind` defaults to `conversation`; anything else is a
/// 400. `limit` defaults to 200, hard-capped at 1000.
pub(crate) async fn get_log_core(
    state: &SharedState,
    id: SessionId,
    kind: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<ConversationEntry>, ApiError> {
    let _session = state.services.meta.get_session(id).await?;
    let limit = limit.unwrap_or(200).clamp(1, 1000);
    match kind.as_deref().unwrap_or("conversation") {
        "conversation" => {
            let rows = state
                .services
                .meta
                .list_session_events_since(id, -1, limit)
                .await?;
            Ok(rows
                .into_iter()
                .map(|e| ConversationEntry {
                    idx: e.idx,
                    kind: e.kind,
                    at: e.created_at,
                    payload: e.payload,
                })
                .collect())
        }
        other => Err(ApiError::BadRequest(format!(
            "unknown kind `{other}` — only `conversation` is supported"
        ))),
    }
}

/// gRPC `GetCowState` core. `None` when the session has no live sandbox
/// (Idle / HostLost / terminal / Pending) or the host doesn't know it.
pub(crate) async fn cow_state_core(
    state: &SharedState,
    id: SessionId,
) -> Result<Option<CowStateView>, ApiError> {
    let session = state.services.meta.get_session(id).await?;
    let (host_id, sandbox_id) = match (session.host_id, session.sandbox_id, session.status) {
        (
            Some(h),
            Some(sb),
            SessionState::Active | SessionState::Created | SessionState::GuestReady,
        ) => (h, sb),
        _ => return Ok(None),
    };
    let backend = state.host_registry.backend_of(host_id).ok_or_else(|| {
        ApiError::HostLost(format!(
            "session {id} bound to host {host_id} which is no longer registered"
        ))
    })?;
    let records = fetch_for_host(&state.cow_state_cache, host_id, backend)
        .await
        .map_err(ApiError::from)?;
    let Some(record) = records.into_iter().find(|r| r.sandbox_id == sandbox_id) else {
        return Ok(None);
    };
    let (memory_manifest, last_snapshot_at) =
        match state.services.meta.latest_snapshot_for_session(id).await {
            Ok(Some(rec)) => (rec.memory_manifest, Some(rec.created_at)),
            Ok(None) => (None, None),
            Err(_) => (None, None),
        };
    Ok(Some(CowStateView::from_record(
        &record,
        Some(id),
        memory_manifest,
        last_snapshot_at,
    )))
}

/// gRPC `ListCheckpoints` core. Newest-first checkpoint chain.
pub(crate) async fn checkpoints_core(
    state: &SharedState,
    id: SessionId,
) -> Result<Vec<CheckpointSummary>, ApiError> {
    state.services.meta.get_session(id).await?;
    let rows = state.services.meta.list_snapshots_for_session(id).await?;
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| CheckpointSummary {
            snapshot_id: r.id.to_string(),
            created_at: r.created_at,
            size_bytes: r.size_bytes,
            events_cursor: r.events_cursor,
            recoverable: r.recoverable,
            is_latest: i == 0,
        })
        .collect())
}
