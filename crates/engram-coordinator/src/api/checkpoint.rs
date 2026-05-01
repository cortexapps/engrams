//! `POST /sessions/:id/checkpoint` — manual / operator-driven flush
//! of a session's workspace to its `engram/sessions/<id>` branch.
//!
//! Calls into [`engram_host_agent::checkpoint::checkpoint_session`].
//! Refuses (409 Conflict) when the session isn't `SessionKind::Git`
//! — `Local` and `Readonly` sessions have no checkpoint branch.
//!
//! Track C.9 (auto-checkpoint on `HarnessEvent::Idle` /
//! `HarnessEvent::RunCompleted`) builds on the same primitive but
//! invokes it from the harness EventSink rather than this HTTP
//! endpoint. Per-tool-call cadence was the original Track C.9 design;
//! we settled on idle/run-completed instead — one commit per
//! completed agent run is the right unit of work for `engram session
//! diff/pr` and the noise from per-call empty commits isn't worth it.

use axum::extract::{Path, State};
use axum::Json;
use chrono::Utc;
use engram_core::types::session::SessionKind;
use engram_core::SessionId;
use engram_harness_proto::CheckpointReason;
use engram_host_agent::checkpoint::{
    checkpoint_session, CheckpointConfig, CheckpointError, CheckpointOutcome,
};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Where the in-VM workspace lives. Phase 4 production wires this
/// via Track C.5 (`engram-bootstrap`); for `--mode=all` development
/// against ProcessBackend, the sandbox cwd is itself the workspace
/// (the rootfs_source materialises into it), so `.` is the right
/// relative path.
///
/// We hardcode `.` here rather than `/workspace` because every
/// existing caller and test exercises ProcessBackend. When Track C.5
/// lands the in-VM bootstrap, this becomes per-session metadata.
const DEFAULT_WORKSPACE: &str = ".";

#[derive(Serialize)]
pub struct CheckpointResponse {
    pub session_id: SessionId,
    pub branch: String,
    pub commit_sha: String,
    pub harness_acked: bool,
}

pub async fn checkpoint(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<CheckpointResponse>, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    // Ephemeral / Readonly sessions have no checkpoint branch —
    // refuse explicitly so callers don't silently no-op.
    let branch = match session.session_kind {
        SessionKind::Git => session.checkpoint_branch.clone().ok_or_else(|| {
            ApiError::Internal(
                "git session is missing its checkpoint_branch — schema invariant broken".into(),
            )
        })?,
        SessionKind::Ephemeral | SessionKind::Readonly => {
            return Err(ApiError::Conflict(format!(
                "session is `{}` — only `git` sessions can be checkpointed",
                session.session_kind.as_str()
            )));
        }
    };

    let sandbox_id = state.registry.get(id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to checkpoint — create or resume first".into(),
        )
    })?;

    let backend = state.services.sandbox.clone();
    let cfg = CheckpointConfig::default();

    match checkpoint_session(
        &state.harness_hub,
        &backend,
        sandbox_id,
        DEFAULT_WORKSPACE,
        &branch,
        CheckpointReason::Manual,
        &cfg,
    )
    .await
    {
        Ok(CheckpointOutcome {
            commit_sha,
            harness_acked,
        }) => {
            let commit_sha = commit_sha.unwrap_or_default();
            let _ = state
                .emit(
                    id,
                    SessionEvent::CheckpointPushed {
                        commit_sha: commit_sha.clone(),
                        harness_acked,
                        at: Utc::now(),
                    },
                )
                .await;
            Ok(Json(CheckpointResponse {
                session_id: id,
                branch,
                commit_sha,
                harness_acked,
            }))
        }
        Err(e) => {
            let reason = e.to_string();
            let _ = state
                .emit(
                    id,
                    SessionEvent::CheckpointFailed {
                        reason: reason.clone(),
                        at: Utc::now(),
                    },
                )
                .await;
            Err(map_checkpoint_error(e))
        }
    }
}

/// Map a checkpoint failure to the most useful HTTP status code.
/// Workspace-too-large is a caller bug (operator should fix
/// `.gitignore`); a git push to a broken remote is operational; the
/// rest fold into 500.
fn map_checkpoint_error(e: CheckpointError) -> ApiError {
    match e {
        CheckpointError::WorkspaceTooLarge { .. } => ApiError::Conflict(e.to_string()),
        CheckpointError::GitFailed { .. } => ApiError::Internal(e.to_string()),
        CheckpointError::Sandbox(se) => se.into(),
    }
}
