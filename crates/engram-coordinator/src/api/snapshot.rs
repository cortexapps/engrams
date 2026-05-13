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
use engram_core::{SandboxError, SandboxId, SessionId};
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

    // ADR 0007 Phase 6: backend owns its staging dir. Coord no
    // longer pre-allocates a path — the backend's
    // `snapshot_path_for(metadata.id)` is the canonical reference
    // for the on-disk location. Cross-host durability flows
    // through the chunked manifests on `SnapshotMetadata`, not
    // through the local path.
    let metadata = state.services.sandbox.snapshot(sandbox_id).await?;

    let now = Utc::now();
    // Record the host that wrote this snapshot to its local disk so
    // the resume path's snapshot-affinity scheduler can route back to
    // it (zero-cost hot-tier hit). ADR 0007: durability lives in the
    // chunk store (`disk_manifest` / `memory_manifest`); the local dir
    // is a per-host cache the same-host fast-resume reads from.
    let host_id = state.host_registry.host_of(sandbox_id);
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: id,
        host_id,
        image_version: metadata.image_version,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
        // ADR 0007: chunked manifests are the durability primitive.
        // FC backends produce both fields via the PooledBackend wrap;
        // VZ produces disk_manifest only; Process produces neither.
        disk_manifest: metadata.disk_manifest,
        memory_manifest: metadata.memory_manifest,
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
    // the chunked manifests are gone (or never were), the session
    // can't be brought back.
    Ok(())
}

async fn resume_session(state: SharedState, id: SessionId) -> Result<SnapshotResponse, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    // ADR 0007: single-tier dispatcher.
    //   Idle  → restore from the snapshot's chunked manifests
    //           (snapshot-affinity-scheduled to the host that
    //           captured it; cross-host materialization is a
    //           follow-up).
    //   Dead  → 410 Gone (no recoverable manifests).
    //   any other status → 409.
    match session.status {
        SessionStatus::Idle => resume_from_idle(state, session).await,
        SessionStatus::Dead => Err(ApiError::Gone(
            "snapshot_invalidated: session is terminal; chunked manifests are gone or never existed".into(),
        )),
        other => Err(ApiError::Conflict(format!(
            "session is {} — only Idle sessions can be resumed",
            other.as_str()
        ))),
    }
}

async fn resume_from_idle(
    state: SharedState,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;
    let record = state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .ok_or_else(|| {
            tracing::warn!(
                session_id = %id,
                "resume requested but no snapshot row found — marking Dead",
            );
            ApiError::Gone(
                "snapshot_invalidated: session can't be revived; \
                 use `engram session fork <id>` to continue"
                    .into(),
            )
        })?;
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
        .is_some()
    {
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

/// Same-host resume path: the snapshot's chunked manifests are
/// durable in `BlobStorage`, and the host that captured the snapshot
/// still holds the dir under
/// `<cfg.local_path>/snapshots/<session>/<snapshot_id>`. The scheduler
/// routes restore back to that host via snapshot-affinity.
///
/// Cross-host materialization (where the picked host doesn't have a
/// local copy and reconstructs from chunks) is a follow-up task —
/// the chunked manifests are the portable form, but FC's restore
/// API still wants a directory on local disk today.
async fn resume_from_fc_snapshot(
    state: SharedState,
    session: Session,
    record: SnapshotRecord,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;
    // ADR 0007: reconstruct the snapshot dir from
    // `(snapshot_dir, session_id, snapshot_id)`. The snapshot path
    // wrote the directory at this exact location; the chunked
    // manifests on the row carry the durable copy.
    // ADR 0007 Phase 6: coord no longer pre-resolves a local path.
    // The backend's `snapshot_path_for(record.id)` is the canonical
    // host-local reference; PooledBackend.restore wraps to
    // materialise memory.bin from chunks if it's missing. The
    // "snapshot manifest gone on disk" pre-flight that lived here
    // is now the backend's responsibility — restore() returns a
    // typed SandboxError::Snapshot on missing artifacts, which
    // we map to 410 Gone below.
    let session_for_ctx = session.clone();
    // ScheduleContext carries `(repo, tag)` for telemetry / future
    // affinity hooks; the live scheduler currently keys only on
    // snapshot affinity + capacity. With raw-URI image refs we split
    // here — `repo` becomes `host[:port]/repo[/path]`, `image_version`
    // becomes the tag.
    let (image_repo, image_tag) =
        engram_core::types::session::split_image_ref(&session_for_ctx.image);
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        prefer_snapshot_id: Some(record.id),
        memory_mib: None,
    };
    // Build the SnapshotMetadata the trait now takes. The record
    // carries every field we need; we just round-trip it back into
    // the engine type the backend expects.
    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: record.id,
        size_bytes: record.size_bytes,
        created_at: record.created_at,
        image_version: record.image_version.clone(),
        disk_manifest: record.disk_manifest,
        memory_manifest: record.memory_manifest,
    };
    let (host_id, new_sandbox_id) = match state
        .host_registry
        .restore_for_session(&ctx, restore_metadata)
        .await
    {
        Ok(v) => v,
        Err(SandboxError::Snapshot(msg)) => {
            // Lost local artifacts on every viable host — chunked
            // restore couldn't rehydrate from the manifest either.
            // Mark Dead + surface the same affordance the missing-
            // manifest preflight used to return.
            tracing::warn!(
                session_id = %id,
                error = %msg,
                "snapshot restore failed; marking session Dead",
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
        Err(e) => return Err(ApiError::from(e)),
    };
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
    // Build the resume base env in two layers, mirroring create:
    //
    //   1. manifest.env + manifest.[secrets.*] resolved fresh from
    //      the deployment SecretStore. Re-resolving (vs. snapshotting
    //      at create) means an operator-driven secret rotation lands
    //      automatically on the next resume.
    //   2. per-request `secrets` overrides (CLAUDE_CODE_OAUTH_TOKEN,
    //      ANTHROPIC_API_KEY, etc.) decrypted from session_secrets.
    //      These shadow any same-key value from layer 1 — matches
    //      "the user explicitly typed a value at create" semantics.
    //
    // Both layers fail soft: a missing enabled_images row, transient
    // SecretStore hiccup, or pre-fix session with no sealed row
    // resume with a thinner env (the pre-fix status quo) instead of
    // failing the whole resume.
    let mut resume_base_env =
        match crate::api::sessions::resolve_manifest_secrets(&state, &session).await {
            Ok(env) => env,
            Err(e) => {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "resolve_manifest_secrets failed; resume continues without manifest env",
                );
                std::collections::HashMap::new()
            }
        };
    match crate::api::sessions::load_session_secrets(&state, id).await {
        Ok(Some(overrides)) => {
            for (k, v) in overrides {
                resume_base_env.insert(k, v);
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "load_session_secrets failed; resume continues without per-request overrides",
            );
        }
    }
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
        // ADR 0006: the host-agent unregisters its local proxy
        // entry as part of `destroy`. No coordinator-side cleanup.
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
