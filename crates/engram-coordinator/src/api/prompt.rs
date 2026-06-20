//! `POST /sessions/:id/prompt` — push a prompt to a running agent.
//!
//! Auto-resumes Idle sessions via `ensure_active`, then forwards
//! the text via `harness_hub.send_prompt`. The hub atomically
//! clears `last_idle_at` on send so the soft idle-eviction TTL
//! doesn't fire while the adapter is starting its next run.
//!
//! Dead sessions return 410 Gone — the only affordance there is
//! `engram session fork <id>`.

use engram_core::SessionId;
use engram_harness_proto::AgentRole;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Shared delivery preamble for prompts and answers: auto-resume an
/// Idle/evicted session, HOLD through an in-flight move, and resolve the
/// live sandbox. Both a prompt and an answer drive an idle session back to
/// life, so both need the identical hardened sequence.
async fn ensure_active_and_resolve(
    state: &SharedState,
    id: SessionId,
) -> Result<engram_core::SandboxId, ApiError> {
    // Auto-resume Idle sessions via the FC snapshot path. Dead
    // sessions surface 410 Gone here (ensure_active → resume_session
    // → ApiError::Gone for missing snapshots). Active sessions are a
    // no-op.
    crate::api::snapshot::ensure_active(state, id).await?;

    // HOLD delivery while the session is mid-move/mid-resume.
    // Forwarding into the freeze window writes into a frozen sandbox's
    // vsock buffer, which the move's commit then destroys with the source:
    // the message vanishes and the UI hangs "working…" (prod session
    // 284d72e3).
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
            "delivery held during an in-flight session move, delivering now",
        );
    }

    state.resolve_sandbox(id).await.ok_or_else(|| {
        // After ensure_active, an Active session must have a sandbox
        // bound. If not, we hit a state we don't have a clean
        // affordance for — surface a 409.
        ApiError::Conflict(
            "session has no live sandbox after auto-resume; \
             try `engram session resume <id>` and retry"
                .into(),
        )
    })
}

/// ADR 0051: transport-agnostic prompt core (gRPC `SendPrompt`). Holds the
/// SAME hardened auto-resume + mid-move HOLD logic as the axum `prompt`
/// handler; only the I/O shape changed (request fields → params,
/// `Json<PromptResponse>` → the `&'static str` note).
pub(crate) async fn send_prompt_core(
    state: &SharedState,
    id: SessionId,
    prompt_id: String,
    text: String,
) -> Result<&'static str, ApiError> {
    if text.is_empty() {
        return Err(ApiError::BadRequest("`text` is required".into()));
    }
    // Phase 1b: the client (web) mints `prompt_id` so it can correlate its
    // optimistic bubble with the server echo + `RunStarted{prompt_id}`
    // (this is what dedupes the double-render). Mint one if a non-web
    // caller left it empty, so the wire is always uniform.
    let prompt_id = if prompt_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        prompt_id
    };

    // Auto-resume (Idle → FC snapshot), HOLD through any in-flight move,
    // and resolve the live sandbox. Shared verbatim with `answer_question_core`.
    let sandbox_id = ensure_active_and_resolve(state, id).await?;

    let prompt_text = text;
    state
        .services
        .host
        .send_prompt(sandbox_id, prompt_id.clone(), prompt_text.clone())
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
                // Phase 1b: tag the user-echo with the client prompt_id so
                // the web dedupes its optimistic bubble against this event
                // (the double-render fix) instead of rendering both.
                prompt_id: Some(prompt_id),
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "emit user prompt event failed");
    }

    Ok("prompt forwarded")
}

/// ADR 0054: transport-agnostic answer core (gRPC `AnswerQuestion`).
/// Answering a deferred `UserQuestion` resumes the session exactly as a
/// prompt does — so it reuses the identical auto-resume + HOLD + resolve
/// preamble — then forwards `HarnessCommand::AnswerQuestion`. Unlike a
/// prompt it emits **no user-echo**: the harness's own `QuestionAnswered`
/// event is the durable "answered" record, and a synthetic user turn would
/// pollute the transcript. Idempotent end to end (the deferred tool yields
/// exactly one tool_result), so a duplicate answer is at worst a no-op
/// resume.
pub(crate) async fn answer_question_core(
    state: &SharedState,
    id: SessionId,
    tool_call_id: String,
    answers: engram_harness_proto::Answers,
) -> Result<&'static str, ApiError> {
    if tool_call_id.is_empty() {
        return Err(ApiError::BadRequest("`tool_call_id` is required".into()));
    }
    let sandbox_id = ensure_active_and_resolve(state, id).await?;
    state
        .services
        .host
        .answer_question(sandbox_id, tool_call_id, answers)
        .await
        .map_err(|e| ApiError::Internal(format!("forward answer to harness: {e}")))?;
    Ok("answer forwarded")
}

/// Phase 1b: edit a still-queued type-ahead prompt by its `prompt_id`,
/// before the harness consumes it. The hold/auto-resume logic of
/// `send_prompt_core` is unnecessary here — the prompt was only queued
/// while a run is in flight, so the session is Active and attached.
pub(crate) async fn edit_queued_prompt_core(
    state: &SharedState,
    id: SessionId,
    prompt_id: String,
    text: String,
) -> Result<&'static str, ApiError> {
    if prompt_id.is_empty() {
        return Err(ApiError::BadRequest("`prompt_id` is required".into()));
    }
    if text.is_empty() {
        return Err(ApiError::BadRequest("`text` is required".into()));
    }
    let sandbox_id = state
        .resolve_sandbox(id)
        .await
        .ok_or_else(|| ApiError::Conflict("session has no live sandbox".into()))?;
    state
        .services
        .host
        .edit_queued_prompt(sandbox_id, prompt_id, text)
        .await
        .map_err(|e| ApiError::Internal(format!("edit queued prompt: {e}")))?;
    Ok("queued prompt edited")
}

/// Phase 1b: remove a still-queued type-ahead prompt by its `prompt_id`
/// (the user pulled it back to the composer or cancelled it).
pub(crate) async fn dequeue_queued_prompt_core(
    state: &SharedState,
    id: SessionId,
    prompt_id: String,
) -> Result<&'static str, ApiError> {
    if prompt_id.is_empty() {
        return Err(ApiError::BadRequest("`prompt_id` is required".into()));
    }
    let sandbox_id = state
        .resolve_sandbox(id)
        .await
        .ok_or_else(|| ApiError::Conflict("session has no live sandbox".into()))?;
    state
        .services
        .host
        .dequeue_queued_prompt(sandbox_id, prompt_id)
        .await
        .map_err(|e| ApiError::Internal(format!("dequeue queued prompt: {e}")))?;
    Ok("queued prompt dequeued")
}
