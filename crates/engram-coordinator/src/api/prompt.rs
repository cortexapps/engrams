//! `POST /sessions/:id/prompt` — push a prompt to a running agent.
//!
//! Auto-resumes Idle sessions via `ensure_active`, then forwards
//! the text via `harness_hub.send_prompt`. The hub atomically
//! clears `last_idle_at` on send so the soft idle-eviction TTL
//! doesn't fire while the adapter is starting its next run.
//!
//! Dead sessions return 410 Gone — the only affordance there is
//! `engram session fork <id>`.

use axum::extract::{Path, State};
use axum::Json;
use engram_core::SessionId;
use engram_harness_proto::AgentRole;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

#[derive(Deserialize)]
pub struct PromptRequest {
    pub text: String,
}

#[derive(Serialize)]
pub struct PromptResponse {
    pub session_id: SessionId,
    pub note: &'static str,
}

pub async fn prompt(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Json(req): Json<PromptRequest>,
) -> Result<Json<PromptResponse>, ApiError> {
    if req.text.is_empty() {
        return Err(ApiError::BadRequest("`text` is required".into()));
    }

    // Auto-resume Idle sessions via the FC snapshot path. Dead
    // sessions surface 410 Gone here (ensure_active → resume_session
    // → ApiError::Gone for missing snapshots). Active sessions are a
    // no-op.
    crate::api::snapshot::ensure_active(&state, id).await?;

    // HOLD delivery while the session is mid-move/mid-resume.
    // Forwarding into the freeze window writes the prompt into a
    // frozen sandbox's vsock buffer, which the move's commit then
    // destroys with the source: the message vanishes and the UI hangs
    // "working…" (prod session 284d72e3).
    //
    // The hold condition is `lease held AND status != Active` — NOT
    // the lease alone. ADR 0045 C2's finalize task holds the lease
    // long past the move (drain + source commit + the Full-checkpoint
    // durability catch-up, ~30s), but the session walks back to
    // Active exactly at harness-ready — from that moment delivery
    // targets the LIVE dest and is safe (prod session 962011bf: the
    // lease-only condition swallowed a prompt for 23.7s until the
    // checkpoint row landed). The C2 flow transitions to Evacuating
    // BEFORE the capture pause, so the status check covers the whole
    // freeze.
    let hold_budget = std::time::Duration::from_secs(60);
    let hold_start = std::time::Instant::now();
    let mut held = false;
    loop {
        let lease_held = state
            .services
            .meta
            .session_lease_held(id)
            .await
            .unwrap_or(false);
        if !lease_held {
            break;
        }
        let status = state
            .services
            .meta
            .get_session(id)
            .await
            .map(|s| s.status)
            .unwrap_or(engram_core::types::SessionState::Active);
        if status == engram_core::types::SessionState::Active {
            // Post-rebind, harness-ready: the lease is the finalize's
            // durability bookkeeping, not a freeze.
            break;
        }
        if hold_start.elapsed() > hold_budget {
            return Err(ApiError::Conflict(
                "session is mid-move (lease held for over 60s); retry shortly".into(),
            ));
        }
        held = true;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if held {
        tracing::info!(
            session_id = %id,
            held_ms = hold_start.elapsed().as_millis() as u64,
            "prompt held during an in-flight session move, delivering now",
        );
    }

    let sandbox_id = state.resolve_sandbox(id).await.ok_or_else(|| {
        // After ensure_active, an Active session must have a sandbox
        // bound. If not, we hit a state we don't have a clean
        // affordance for — surface a 409.
        ApiError::Conflict(
            "session has no live sandbox after auto-resume; \
             try `engram session resume <id>` and retry"
                .into(),
        )
    })?;

    let prompt_text = req.text;
    state
        .services
        .host
        .send_prompt(sandbox_id, prompt_text.clone())
        .await
        .map_err(|e| ApiError::Internal(format!("forward prompt to harness: {e}")))?;

    // Record the user's prompt in the session event log so transcripts
    // can reconstruct the conversation. The harness adapter never
    // echoes the prompt back through Claude's stream-json output —
    // it only translates the *assistant* response — so without this
    // entry the user's turn is invisible to subscribers. Best-effort:
    // if the emit fails the harness already has the prompt and will
    // run it, so we'd rather log and return success than 500 the
    // caller after a successful forward.
    if let Err(e) = state
        .emit(
            id,
            SessionEvent::HarnessAgentMessage {
                run_id: String::new(),
                message_id: format!("user-{}", uuid::Uuid::new_v4()),
                role: AgentRole::User,
                text: prompt_text,
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "emit user prompt event failed");
    }

    Ok(Json(PromptResponse {
        session_id: id,
        note: "prompt forwarded",
    }))
}
