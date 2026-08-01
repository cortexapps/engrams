//! The `SessionService/Interrupt` RPC — operator stop (ADR 0030).
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

/// ADR 0051: transport-agnostic interrupt — the app-gRPC `Interrupt` RPC
/// (the only remaining surface) delegates here. No auto-resume — a missing
/// live sandbox is a 409. Returns the static note the RPC echoes back.
///
/// ADR 0108: `source` attributes the caller (`InterruptRequest.source`,
/// free-form; empty from old clients → "unattributed"). Every forwarded
/// interrupt is logged and counted so a phantom interrupt is traceable
/// without archaeology.
pub(crate) async fn interrupt_core(
    state: &SharedState,
    id: SessionId,
    source: &str,
) -> Result<&'static str, ApiError> {
    let source = if source.is_empty() {
        "unattributed"
    } else {
        source
    };
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
    tracing::info!(
        session_id = %id,
        sandbox_id = %sandbox_id,
        source,
        "operator interrupt forwarded"
    );
    ::metrics::counter!(
        crate::metrics::INTERRUPTS_TOTAL,
        "source" => crate::metrics::interrupt_source_label(source)
    )
    .increment(1);
    Ok("interrupt forwarded")
}
