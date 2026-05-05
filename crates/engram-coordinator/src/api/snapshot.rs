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
    // Pre-flight: if the snapshot dir or manifest disappeared on
    // disk (host wiped /var, operator rm'd, FC's own writes failed
    // halfway), the restore call below would surface as a generic
    // 500. Treat the missing-file case the same as a null
    // local_path in metadata — terminal-Dead, 410 Gone — so the
    // caller's affordance ("fork the workspace") is the same.
    let manifest_path = local_path.join("manifest.json");
    if !tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
        tracing::warn!(
            session_id = %id,
            path = %manifest_path.display(),
            "snapshot manifest missing on disk; marking session Dead",
        );
        let _ = state
            .services
            .meta
            .set_session_status(id, SessionStatus::Dead)
            .await;
        return Err(ApiError::Gone(
            "snapshot_invalidated: session can't be revived; \
             use `engram session fork <id>` to continue from the workspace"
                .into(),
        ));
    }
    let session_for_ctx = session.clone();
    // ScheduleContext keys warm-pool affinity by `(repo, tag)`. With
    // raw-URI image refs we split here for the affinity hint —
    // `repo` becomes `host[:port]/repo[/path]`, `image_version`
    // becomes the tag.
    let (image_repo, image_tag) =
        engram_core::types::session::split_image_ref(&session_for_ctx.image);
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        prefer_snapshot_id: Some(record.id),
        memory_mib: None,
    };
    let (host_id, new_sandbox_id) = state
        .host_registry
        .restore_for_session(&ctx, local_path)
        .await?;
    bind_resumed_session(&state, id, host_id, new_sandbox_id).await;
    // Re-launch the per-session agent so the in-VM bootstrap
    // supervisor kill+respawns the harness for the restored sandbox.
    // Without this, the post-resume VM has the pre-snapshot adapter
    // still running with a half-open vsock — the host can't reach
    // it (its UDS died with the original sandbox), and the adapter
    // can't notice (vsock reads on a half-open connection block
    // forever). A fresh BootstrapLaunch is the in-VM signal to
    // start clean. Resume omits ENGRAM_INITIAL_PROMPT so the
    // adapter goes straight to Idle and waits for the next user
    // prompt instead of replaying the original kickoff.
    //
    // The base env starts from the per-request `secrets` map sealed
    // at create time (e.g. CLAUDE_CODE_OAUTH_TOKEN); if the row
    // doesn't exist (no overrides were submitted, or the session
    // pre-dates session-secret persistence) we fall back to an
    // empty map and the harness boots without that env. The
    // ENGRAM_SESSION_HARNESS_NAME hint that the host-agent reads
    // for substrate naming is recomputed downstream by
    // `resolve_harness`.
    //
    // TODO(secrets-on-resume-manifest): also re-resolve the image's
    // manifest-declared `[secrets.*]` schema here via SecretStore.
    // Today only the per-request override map round-trips.
    let resume_base_env = match crate::api::sessions::load_session_secrets(&state, id).await {
        Ok(Some(map)) => map,
        Ok(None) => std::collections::HashMap::new(),
        Err(e) => {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "load_session_secrets failed; resume continues with empty secret env",
            );
            std::collections::HashMap::new()
        }
    };
    let agent_opt =
        crate::api::sessions::resolve_harness(&state, &session.harness, id, None, &resume_base_env)
            .ok()
            .flatten();
    if let Some(agent) = agent_opt {
        if let Err(e) = state
            .services
            .sandbox
            .start_agent(new_sandbox_id, agent)
            .await
        {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %new_sandbox_id,
                error = %e,
                "post-resume start_agent failed; harness may not reattach",
            );
        }
    }
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
    // Critical for post-resume harness reconnect: the harness hub's
    // session_to_sandbox map keys the FC vsock accept path. Without
    // this, an in-VM adapter that re-dials after FC restore would
    // hit `accept_via_session_lookup` → "no sandbox bound to this
    // session_id" and bounce. The original `bind_session` from
    // `create_session` pointed at the now-destroyed sandbox.
    state.harness_hub.bind_session(id, sandbox_id);
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
        if let Some(proxy) = state.services.egress_proxy.as_ref() {
            proxy.registry.unregister(id);
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
