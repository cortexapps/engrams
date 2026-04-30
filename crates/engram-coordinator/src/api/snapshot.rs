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
use engram_core::types::{Session, SessionStatus};
use engram_core::{SandboxId, SessionId};
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
    resume_session(state, id).await.map(Json)
}

/// Auto-resume an `Idle` session if needed, before routing an
/// exec / exec_stream / events request to it. Track B's pack-hosts
/// counterpart: the idle evictor hot-suspends inactive sessions;
/// this helper brings them back transparently on the next request.
///
/// `Active` sessions are a no-op (Ok). `Idle` sessions are
/// resumed via the existing `/resume` flow and the function
/// returns once the session is Active again. Any other status
/// (Pending, Completed, Failed) returns an error — auto-resume
/// only undoes idle-eviction; it doesn't try to reanimate
/// terminal sessions.
pub async fn ensure_active(state: &SharedState, id: SessionId) -> Result<(), ApiError> {
    let session = state.services.meta.get_session(id).await?;
    if session.status == SessionStatus::Idle {
        resume_session(state.clone(), id).await?;
    }
    // Active / Pending / Dead / Completed / Failed all fall through
    // to the downstream handler. Dead in particular is terminal —
    // the snapshot is gone, the session can't be brought back; the
    // caller's only affordance is `engram session fork <id>` to
    // start fresh from the workspace.
    Ok(())
}

async fn resume_session(state: SharedState, id: SessionId) -> Result<SnapshotResponse, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    // Resume is FC-snapshot-only. Active sessions are a no-op the
    // caller likely didn't mean; Dead sessions can't be resumed —
    // their snapshot was invalidated and the only affordance is
    // `engram session fork` to continue from the workspace.
    match session.status {
        SessionStatus::Idle => {} // happy path
        SessionStatus::Dead => {
            return Err(ApiError::Gone(
                "snapshot_invalidated: session can't be revived; \
                 use `engram session fork <id>` to continue from the workspace"
                    .into(),
            ));
        }
        other => {
            return Err(ApiError::Conflict(format!(
                "session is {} — only Idle sessions can be resumed",
                other.as_str()
            )));
        }
    }

    // Hot path: a local FC snapshot exists on some host; the
    // snapshot-affinity scheduler routes the restore back to that
    // host. Sub-second hot resume from FC memory.bin.
    let snapshot = state.services.meta.latest_snapshot_for_session(id).await?;
    let record = snapshot
        .as_ref()
        .filter(|r| r.local_path.is_some())
        .cloned()
        .ok_or_else(|| {
            // No usable snapshot. Mark Dead so subsequent calls hit
            // the 410 path immediately, and surface the same error.
            tracing::warn!(
                session_id = %id,
                "resume requested but no FC snapshot is recoverable — marking Dead",
            );
            ApiError::Gone(
                "snapshot_invalidated: session can't be revived; \
                 use `engram session fork <id>` to continue from the workspace"
                    .into(),
            )
        })?;
    // Mark Dead async — best-effort, ignore failures.
    let _ = transition_to_dead_if_no_snapshot(&state, id).await;
    resume_from_fc_snapshot(state, session, record).await
}

/// Look up the session's snapshot one more time and, if there's
/// truly nothing recoverable, set status = Dead. Best-effort —
/// failure here just means a future request will redo the same
/// check. Called from `resume_session`'s no-snapshot path.
async fn transition_to_dead_if_no_snapshot(
    state: &SharedState,
    id: SessionId,
) -> Result<(), ApiError> {
    if state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .as_ref()
        .filter(|r| r.local_path.is_some())
        .is_some()
    {
        // A snapshot DID land — don't mark Dead.
        return Ok(());
    }
    let _ = state
        .services
        .meta
        .set_session_status(id, SessionStatus::Dead)
        .await;
    let _ = state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: SessionStatus::Idle,
                to: SessionStatus::Dead,
                at: Utc::now(),
            },
        )
        .await;
    Ok(())
}

/// Hot-resume path: a local FC snapshot exists, restore it on the
/// host that holds it (snapshot-affinity-scheduled). Sub-second.
async fn resume_from_fc_snapshot(
    state: SharedState,
    session: Session,
    record: SnapshotRecord,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;
    let local_path = record.local_path.clone().ok_or_else(|| {
        ApiError::Internal("resume_from_fc_snapshot called without local_path".into())
    })?;
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
    bind_resumed_session(&state, id, host_id, new_sandbox_id).await;
    finalize_resume(
        &state,
        id,
        session.status,
        SessionEvent::Resumed {
            snapshot_id: record.id,
            at: Utc::now(),
        },
    )
    .await?;

    Ok(SnapshotResponse {
        session_id: id,
        snapshot_id: Some(record.id.to_string()),
        size_bytes: Some(record.size_bytes),
        note: "resumed from snapshot",
    })
}


async fn bind_resumed_session(
    state: &SharedState,
    id: SessionId,
    host_id: engram_core::HostId,
    sandbox_id: SandboxId,
) {
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
            "assign_session_host on resume failed; HostRegistry routing still works",
        );
    }
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(id, Some(sandbox_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            sandbox_id = %sandbox_id,
            error = %e,
            "assign_session_sandbox on resume failed; live routing still works (in-memory only)",
        );
    }
    state.registry.bind(id, sandbox_id);
}

async fn finalize_resume(
    state: &SharedState,
    id: SessionId,
    from: SessionStatus,
    resume_event: SessionEvent,
) -> Result<(), ApiError> {
    state
        .services
        .meta
        .set_session_status(id, SessionStatus::Active)
        .await?;
    let now = Utc::now();
    state.emit(id, resume_event).await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from,
                to: SessionStatus::Active,
                at: now,
            },
        )
        .await?;
    Ok(())
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

