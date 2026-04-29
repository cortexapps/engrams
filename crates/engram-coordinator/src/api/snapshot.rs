//! Snapshot / evict / resume endpoints.
//!
//! State machine:
//!
//! ```text
//!   POST /sessions             -> Active   (sandbox live, registry bound)
//!   POST /sessions/:id/snapshot -> Active   (snapshot taken, sandbox stays live)
//!   DELETE /sessions/:id/local -> Idle     (sandbox destroyed, requires snapshot)
//!   POST /sessions/:id/resume   -> Active   (restored from snapshot, registry rebound)
//!   DELETE /sessions/:id        -> Completed (terminal)
//! ```
//!
//! `snapshot` is intentionally separate from `evict_local`: snapshot is a
//! save-point that keeps the live sandbox running, evict drops it. Most
//! callers that want both should snapshot then evict in two requests, or
//! evict then resume across the lifecycle of a session.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::SessionStatus;
use engram_core::SessionId;
use serde::Serialize;

use crate::error::ApiError;
use crate::host_registry::ScheduleContext;
use crate::state::{SessionEvent, SharedState};

#[derive(Serialize)]
pub struct SnapshotResponse {
    pub session_id: SessionId,
    pub snapshot_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub note: &'static str,
}

pub async fn snapshot(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    state.services.meta.get_session(id).await?;

    let sandbox_id = state.registry.get(id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to snapshot — create or resume first".into(),
        )
    })?;

    // Snapshots live under `<local_path>/snapshots/<session>/<dir>`.
    // The directory name is its own UUID; the canonical SnapshotId is what
    // the backend reports back in SnapshotMetadata.
    let dest = state
        .snapshot_dir()
        .join(id.to_string())
        .join(uuid::Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&dest)
        .await
        .map_err(|e| ApiError::Internal(format!("create snapshot dir: {e}")))?;

    let metadata = state.services.sandbox.snapshot(sandbox_id, &dest).await?;

    let now = Utc::now();
    // Record the host that wrote this snapshot to its local disk so
    // the resume path's snapshot-affinity scheduler can route back to
    // it (zero-cost hot-tier hit).
    let host_id = state.host_registry.host_of(sandbox_id);
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: id,
        host_id,
        local_path: Some(dest),
        image_version: metadata.image_version,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
    };
    state.services.meta.record_snapshot(record).await?;

    state
        .emit(
            id,
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await?;

    // Snapshot does NOT change session state — the live sandbox keeps
    // running. evict_local is the explicit "drop from RAM" action.
    Ok(Json(SnapshotResponse {
        session_id: id,
        snapshot_id: Some(metadata.id.to_string()),
        size_bytes: Some(metadata.size_bytes),
        note: "snapshot recorded; live sandbox still running",
    }))
}

pub async fn resume(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    // Resuming an Active session is a no-op the caller probably
    // didn't mean. Idle (operator-evicted) and PendingReassign
    // (Phase 3d migration) both want to re-spin a sandbox from
    // the latest snapshot — same code path, same scheduling.
    if !matches!(
        session.status,
        SessionStatus::Idle | SessionStatus::PendingReassign
    ) {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Idle / PendingReassign sessions can be resumed",
            session.status.as_str()
        )));
    }

    let record = state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .ok_or_else(|| {
            ApiError::Conflict("no snapshot exists for this session — cannot resume".into())
        })?;

    // Snapshots are local-NVMe-only after Phase 4's blob removal.
    // The scheduler's snapshot-affinity branch routes the restore
    // back to the host that holds the snapshot (sub-second hot resume).
    // If that host is gone the snapshot is gone too — cross-host
    // durability lives in git, not snapshots. Resume from a git
    // checkpoint is the right fallback (Track C).
    let local_path = record.local_path.clone().ok_or_else(|| {
        ApiError::Conflict(
            "snapshot has no local copy — host is gone; resume must reconstruct from git".into(),
        )
    })?;

    // Snapshot affinity: prefer the host that has this snapshot
    // locally. When restoring from cold tier the affinity hint is
    // None — pick_for_session falls through to capacity ranking.
    let session_for_ctx = session.clone();
    let ctx = ScheduleContext {
        repo: &session_for_ctx.repo,
        image_version: &session_for_ctx.image_version,
        prefer_snapshot_id: Some(record.id),
        memory_mib: None,
    };
    let (host_id, new_sandbox_id) = state
        .host_registry
        .restore_for_session(&ctx, local_path)
        .await?;
    if let Err(e) = state
        .services
        .meta
        .assign_session_host(id, Some(host_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            host_id = %host_id,
            error = %e,
            "assign_session_host on restore failed; HostRegistry routing still works",
        );
    }
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(id, Some(new_sandbox_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            sandbox_id = %new_sandbox_id,
            error = %e,
            "assign_session_sandbox on restore failed; live routing still works (in-memory only)",
        );
    }
    state.registry.bind(id, new_sandbox_id);
    state
        .services
        .meta
        .set_session_status(id, SessionStatus::Active)
        .await?;
    let now = Utc::now();
    state
        .emit(
            id,
            SessionEvent::Resumed {
                snapshot_id: record.id,
                at: now,
            },
        )
        .await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: session.status,
                to: SessionStatus::Active,
                at: now,
            },
        )
        .await?;

    Ok(Json(SnapshotResponse {
        session_id: id,
        snapshot_id: Some(record.id.to_string()),
        size_bytes: Some(record.size_bytes),
        note: "resumed from snapshot",
    }))
}

pub async fn evict_local(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    if session.status != SessionStatus::Active {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Active sessions can be evicted",
            session.status.as_str()
        )));
    }

    // Refuse to evict if there's no snapshot to come back to. Without
    // this check the in-flight state would just be lost when the
    // sandbox gets destroyed.
    if state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "no snapshot exists for this session — take a snapshot before evicting".into(),
        ));
    }

    if let Some(sandbox_id) = state.registry.unbind(id) {
        if let Err(e) = state.services.sandbox.destroy(sandbox_id).await {
            // Best-effort: even if destroy fails we drop the binding
            // and mark Idle. The sandbox is the cache, not source of truth.
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during evict_local; continuing",
            );
        }
    }

    // Clear the persisted sandbox_id so a coordinator restart
    // doesn't repopulate routing for a sandbox that no longer
    // exists. host_id stays so resume's snapshot affinity still
    // prefers the same host.
    let _ = state.services.meta.assign_session_sandbox(id, None).await;
    state
        .services
        .meta
        .set_session_status(id, SessionStatus::Idle)
        .await?;
    let now = Utc::now();
    state.emit(id, SessionEvent::Evicted { at: now }).await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: SessionStatus::Active,
                to: SessionStatus::Idle,
                at: now,
            },
        )
        .await?;
    Ok(StatusCode::ACCEPTED)
}
