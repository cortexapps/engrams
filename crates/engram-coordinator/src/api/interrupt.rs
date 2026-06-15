//! `POST /sessions/:id/interrupt` — operator stop (ADR 0030).
//!
//! Stops the in-flight run on the session's harness while keeping the
//! session alive. Forwards `interrupt` to the host hub, which sends
//! `HarnessCommand::Interrupt` to the attached harness; the harness
//! SIGINTs its current `claude` child, emits `RunInterrupted` + `Idle`,
//! and stays attached for the next prompt (which resumes via
//! `--resume`).
//!
//! Unlike `prompt`, this does NOT auto-resume an Idle session: there's
//! no in-flight run to stop on a sleeping session, so a missing live
//! sandbox is a 409 rather than a resume. The `run_interrupted` event
//! the transcript renders flows back up the normal harness-event path
//! (harness → host → `SessionEvent::from_harness` → SSE), not from this
//! handler.

use engram_core::SessionId;

use crate::error::ApiError;
use crate::state::SharedState;

// ADR 0039 Task 32: `interrupt` axum shim removed. See `interrupt_core` for the gRPC entry point.

/// Transport-agnostic core: forward an operator stop to the session's
/// harness (no auto-resume — a missing live sandbox is a 409). No authz —
/// gated by the axum route layer / trusted gRPC caller (ADR 0039 §6).
/// Returns the static diagnostic note both transports echo back.
pub(crate) async fn interrupt_core(
    state: &SharedState,
    id: SessionId,
) -> Result<&'static str, ApiError> {
    let sandbox_id = state.resolve_sandbox(id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to interrupt — it is idle or not yet started".into(),
        )
    })?;

    state
        .services
        .host
        .interrupt(sandbox_id)
        .await
        .map_err(|e| ApiError::Internal(format!("forward interrupt to harness: {e}")))?;

    Ok("interrupt forwarded")
}
