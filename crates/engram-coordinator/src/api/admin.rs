//! Admin endpoints — explicit triggers for primitives whose
//! production driver is implicit (background loops, threshold
//! detectors). ADR 0005 / Stage 5.
//!
//! `flush_session` is the canonical example: in production it fires
//! when the disk-pressure detector (Stage 7) crosses a threshold,
//! but for testability + drain-before-redeploy ops scenarios we
//! expose it explicitly here. Both paths share one code path —
//! `engram_host_agent::flush::flush_session`.
//!
//! All endpoints sit behind the same bearer-auth middleware as the
//! other protected routes.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use chrono::Utc;
use engram_core::types::SessionStatus;
use engram_core::SessionId;
use serde::Serialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Wire shape returned by both admin endpoints. Mirrors
/// `engram_host_agent::flush::FlushOutcome` but keeps the dependency
/// one-way (admin.rs imports from host-agent, not the other way
/// around).
#[derive(Serialize)]
pub struct FlushResult {
    pub session_id: SessionId,
    pub snapshot_id: engram_core::SnapshotId,
    pub blob_size_bytes: u64,
    pub took_ms: u64,
}

impl From<engram_host_agent::flush::FlushOutcome> for FlushResult {
    fn from(o: engram_host_agent::flush::FlushOutcome) -> Self {
        Self {
            session_id: o.session_id,
            snapshot_id: o.snapshot_id,
            blob_size_bytes: o.blob_size_bytes,
            took_ms: o.took_ms,
        }
    }
}

/// `POST /api/admin/sessions/:id/flush`. Forces a cold-tier flush of
/// `:id`'s latest snapshot. Returns 404 if the session is unknown,
/// 409 if it isn't currently `Idle` (no snapshot to flush, or
/// already cold-evicted, or actively running).
pub async fn flush_one(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<FlushResult>, ApiError> {
    let outcome = flush_inner(&state, id).await?;
    Ok(Json(outcome.into()))
}

/// `POST /api/admin/flush-idle`. Enumerate every Idle session, fan
/// out the same primitive in parallel (bounded). Returns one entry
/// per session — success or per-session error. Useful for "drain
/// before redeploy" + Stage 5's integration tests.
pub async fn flush_idle(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state
        .services
        .meta
        .list_idle_sessions()
        .await
        .map_err(|e| ApiError::Internal(format!("list_idle_sessions: {e}")))?;

    // Cap the per-host parallelism to avoid CPU/disk thrash from
    // many concurrent tar+zstd pipelines. 4 is a reasonable default
    // — flushes are CPU-bound on zstd compression; 4 cores' worth
    // is enough headroom on commodity hardware while leaving CPU
    // for the rest of the host's traffic.
    let concurrency = 4;
    let state = state.clone();
    let results = futures::stream::iter(sessions)
        .map(|s| {
            let state = state.clone();
            async move {
                let id = s.id;
                match flush_inner(&state, id).await {
                    Ok(o) => json!({
                        "session_id": id,
                        "ok": true,
                        "outcome": FlushResult::from(o),
                    }),
                    Err(e) => json!({
                        "session_id": id,
                        "ok": false,
                        "error": e.to_string(),
                    }),
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    Ok(Json(json!({ "results": results })))
}

/// Shared core: validate state, look up snapshot, drive the host-
/// agent's flush primitive, emit the SSE event. Returns the typed
/// outcome so both single + bulk endpoints can map it.
async fn flush_inner(
    state: &SharedState,
    id: SessionId,
) -> Result<engram_host_agent::flush::FlushOutcome, ApiError> {
    use engram_host_agent::flush::{flush_session, FlushRequest};

    let session = state.services.meta.get_session(id).await?;
    if session.status != SessionStatus::Idle {
        return Err(ApiError::Conflict(format!(
            "session {id} is not Idle (current: {:?}); flush only applies to Idle snapshots",
            session.status
        )));
    }
    let snap = state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "session {id} has no snapshot to flush (cold-evict already, or never snapshotted)"
            ))
        })?;
    let snapshot_path = snap
        .local_path
        .clone()
        .ok_or_else(|| ApiError::Conflict(format!("snapshot {} has no local_path", snap.id)))?;
    let host_id = snap
        .host_id
        .ok_or_else(|| ApiError::Conflict(format!("snapshot {} has no host_id", snap.id)))?;

    let req = FlushRequest {
        session_id: id,
        snapshot_id: snap.id,
        host_id,
        snapshot_path,
    };

    // Bridge the coord's seal helper into the host-agent's pluggable
    // SealFn. The closure captures a clone of state so the host-
    // agent crate doesn't have to know about SharedState.
    let seal_state = state.clone();
    let seal: engram_host_agent::flush::SealFn = Arc::new(move |url: String| {
        let state = seal_state.clone();
        Box::pin(async move {
            let sealed = crate::blob::seal_blob_ref(&state, &url)
                .await
                .map_err(|e| e.to_string())?;
            Ok(sealed)
        })
    });

    let outcome = flush_session(req, &state.services.blob, &state.services.meta, &seal)
        .await
        .map_err(|e| ApiError::Internal(format!("flush_session: {e}")))?;

    // SSE: surface the cold-evict so dashboard subscribers can
    // re-render the row.
    let _ = state
        .emit(
            id,
            SessionEvent::ColdEvicted {
                snapshot_id: outcome.snapshot_id,
                blob_size_bytes: outcome.blob_size_bytes,
                took_ms: outcome.took_ms,
                at: Utc::now(),
            },
        )
        .await;

    Ok(outcome)
}

// `.buffer_unordered` is on `futures::stream::StreamExt`; the bulk
// endpoint uses it via `futures::stream::iter(...).map(...).buffer_unordered`.
use futures::stream::StreamExt;
