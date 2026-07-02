//! `POST /sessions/:id/prompt` — push a prompt to a running agent.
//!
//! Auto-resumes Idle sessions via `ensure_active`, then forwards
//! the text via `harness_hub.send_prompt`. The hub atomically
//! clears `last_idle_at` on send so the soft idle-eviction TTL
//! doesn't fire while the adapter is starting its next run.
//!
//! If the forward hits the harness-unbound desync (an `Active` session
//! whose sandbox is alive — `/exec` works — but no harness is attached
//! for run delivery), `deliver_with_reattach` re-attaches the harness in
//! place and retries, so an interactive prompt/answer self-heals in a beat
//! instead of waiting on the background desync watchdog.
//!
//! Dead sessions return 410 Gone — the only affordance there is
//! `engram session fork <id>`.

use std::time::{Duration, Instant};

use engram_core::{SandboxError, SessionId};
use engram_harness_proto::AgentRole;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Budget for the bounded poll-retry after an in-place harness reattach. The
/// live-harness reattach arm (ADR 0045 C1) only SIGUSR1s the harness to drop +
/// re-dial and returns immediately, so the rebind lands a beat later; an exited
/// harness respawn takes longer. 5s comfortably covers both and stays well under
/// the caller's overall RPC timeout.
const REATTACH_FORWARD_BUDGET: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for the re-dialed harness to re-bind to the hub.
const REATTACH_FORWARD_POLL: Duration = Duration::from_millis(200);

/// Forward a delivery (prompt/answer) to the harness, self-healing the
/// harness-unbound desync.
///
/// `forward` does the actual `send_prompt`/`answer_question`; `reattach`
/// re-establishes the harness IN PLACE on the live sandbox (no teardown —
/// [`crate::api::snapshot::reattach_harness_in_place`]). On the happy path
/// `forward` succeeds immediately with zero added latency. On
/// `SandboxError::NotFound` — the sandbox VM is alive and `/exec` works, but no
/// harness is bound for run delivery (the ADR 0045 C1 / ADR 0034 "prompts
/// 'sandbox not found' while exec works" desync) — we reattach and poll-retry
/// until the re-dialed harness re-binds (bounded by `budget`, since the
/// live-harness arm returns as soon as it SIGUSR1s, not when the re-dial lands).
///
/// This is the synchronous, interactive-latency counterpart to the desync
/// watchdog's ~5-minute background reattach: a Slack follow-up or web prompt
/// landing in the desync window recovers in a beat instead of stranding the user
/// (incident 2026-06-26, session 0332dccf). Safe to retry — prompts dedupe by
/// `prompt_id` (ADR 0052 `seen_prompt_ids`, surviving respawn + snapshot/restore)
/// and answers are no-op-on-duplicate (ADR 0054), so a re-sent forward never
/// double-runs.
async fn deliver_with_reattach<Fwd, FwdFut, Re, ReFut>(
    op: &'static str,
    budget: Duration,
    poll: Duration,
    forward: Fwd,
    reattach: Re,
) -> Result<(), ApiError>
where
    Fwd: Fn() -> FwdFut,
    FwdFut: std::future::Future<Output = Result<(), SandboxError>>,
    Re: FnOnce() -> ReFut,
    ReFut: std::future::Future<Output = Result<bool, ApiError>>,
{
    match forward().await {
        Ok(()) => return Ok(()),
        // Harness-unbound desync — heal below.
        Err(SandboxError::NotFound) => {}
        Err(e) => return Err(ApiError::Internal(format!("forward {op} to harness: {e}"))),
    }

    if !reattach().await? {
        // The session moved off the sandbox (a concurrent evict/resume re-bound
        // it) or has no agent to attach — retryable; the next attempt resolves
        // the fresh binding.
        return Err(ApiError::Conflict(format!(
            "{op} delivery: harness is unbound and could not be re-attached in \
             place (the session may be mid-resume); retry shortly"
        )));
    }

    let deadline = Instant::now() + budget;
    loop {
        match forward().await {
            Ok(()) => return Ok(()),
            Err(SandboxError::NotFound) => {
                if Instant::now() >= deadline {
                    return Err(ApiError::Conflict(format!(
                        "{op} delivery: harness did not re-bind within {}s of an \
                         in-place reattach; retry shortly",
                        budget.as_secs()
                    )));
                }
                tokio::time::sleep(poll).await;
            }
            Err(e) => {
                return Err(ApiError::Internal(format!(
                    "forward {op} to harness after reattach: {e}"
                )))
            }
        }
    }
}

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

/// Issue #535 (d): the echo-then-forward core shared by EVERY prompt
/// delivery — first-or-follow-up, live-request-or-boot-path. Emits the
/// user-echo event FIRST (see the ordering comment below — load-bearing for
/// web rendering), then forwards via `deliver_with_reattach` (self-healing
/// the harness-unbound desync). Factored out of `send_prompt_core` so the
/// create path (`session_boot::boot_on_reserved_host`) can deliver the
/// initial prompt through the EXACT same path a follow-up `SendPrompt`
/// rides — no more separate env-var/synthetic-event spelling for "the
/// first" prompt.
pub(crate) async fn deliver_prompt(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: engram_core::SandboxId,
    prompt_id: String,
    text: String,
) -> Result<(), ApiError> {
    // Record the user's prompt in the session event log BEFORE forwarding to
    // the harness. The harness adapter never echoes the prompt back through
    // Claude's stream-json output — it only translates the *assistant*
    // response — so without this entry the user's turn is invisible to
    // subscribers.
    //
    // ORDER MATTERS: the forward makes the harness start the run and emit
    // `run_started`, which is appended to this same log. The web transcript
    // (buildMessages) HOLDS a `prompt_id` user echo and only renders it when it
    // reaches the consuming `run_started{prompt_id}` (ADR 0052 type-ahead: a
    // queued prompt must render at its consumption position, not echo position).
    // If `run_started` were appended FIRST — as it was when this emit ran AFTER
    // the forward — the held echo isn't there yet at render time, the run draws
    // no user turn, and the echo that lands next is held forever → the user's
    // message vanishes from the transcript on every follow-up `SendPrompt` (prod
    // session 68c70a65). Emitting here guarantees the echo's `idx` precedes its
    // `run_started` — including for the CREATE path now (issue #535 (d)): the
    // initial prompt's `RunStarted.prompt_id` is set from this same consumed
    // queued prompt (`harness-proto` `QueuedPrompt`), so it renders through the
    // identical follow-up path.
    //
    // Best-effort: an emit failure logs + proceeds (the harness still receives +
    // runs the prompt below); we never fail the caller over a missing echo. A
    // forward that ultimately fails leaves the held echo unrendered (no run to
    // consume it) rather than a dangling bubble.
    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::HarnessAgentMessage {
                run_id: String::new(),
                message_id: format!("user-{}", uuid::Uuid::new_v4()),
                role: AgentRole::User,
                text: text.clone(),
                // Phase 1b: tag the user-echo with the client prompt_id so
                // the web dedupes its optimistic bubble against this event
                // (the double-render fix) instead of rendering both.
                prompt_id: Some(prompt_id.clone()),
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session_id = %session_id, error = %e, "emit user prompt event failed");
    }

    deliver_with_reattach(
        "prompt",
        REATTACH_FORWARD_BUDGET,
        REATTACH_FORWARD_POLL,
        || {
            let host = state.services.host.clone();
            let pid = prompt_id.clone();
            let txt = text.clone();
            async move { host.send_prompt(sandbox_id, pid, txt).await }
        },
        || crate::api::snapshot::reattach_harness_in_place(state, session_id, sandbox_id),
    )
    .await
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

    deliver_prompt(state, id, sandbox_id, prompt_id, text).await?;

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
    deliver_with_reattach(
        "answer",
        REATTACH_FORWARD_BUDGET,
        REATTACH_FORWARD_POLL,
        || {
            let host = state.services.host.clone();
            let tcid = tool_call_id.clone();
            let ans = answers.clone();
            async move { host.answer_question(sandbox_id, tcid, ans).await }
        },
        || crate::api::snapshot::reattach_harness_in_place(state, id, sandbox_id),
    )
    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Tiny budget/poll so the timeout path resolves in milliseconds.
    const FAST_BUDGET: Duration = Duration::from_millis(60);
    const FAST_POLL: Duration = Duration::from_millis(5);

    /// Happy path: the first forward binds, so we never reattach (zero added
    /// latency on the hot path).
    #[tokio::test]
    async fn forwards_clean_without_reattach() {
        let reattached = Arc::new(AtomicUsize::new(0));
        let r = reattached.clone();
        let out = deliver_with_reattach(
            "prompt",
            FAST_BUDGET,
            FAST_POLL,
            || async { Ok::<(), SandboxError>(()) },
            || {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Ok::<bool, ApiError>(true)
                }
            },
        )
        .await;
        assert!(out.is_ok());
        assert_eq!(
            reattached.load(Ordering::SeqCst),
            0,
            "a clean forward must not reattach",
        );
    }

    /// The incident shape: the forward hits the harness-unbound `NotFound`, we
    /// reattach in place, and the retry binds.
    #[tokio::test]
    async fn reattaches_and_retries_on_harness_unbound() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reattached = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r = reattached.clone();
        let out = deliver_with_reattach(
            "prompt",
            FAST_BUDGET,
            FAST_POLL,
            || {
                let c = c.clone();
                async move {
                    // Pre-reattach call is unbound; the post-reattach retry binds.
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(SandboxError::NotFound)
                    } else {
                        Ok(())
                    }
                }
            },
            || {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Ok::<bool, ApiError>(true)
                }
            },
        )
        .await;
        assert!(
            out.is_ok(),
            "delivery should succeed after the in-place reattach"
        );
        assert_eq!(reattached.load(Ordering::SeqCst), 1, "exactly one reattach");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "forward attempted twice (pre + post reattach)",
        );
    }

    /// Reattach reports the session moved off the sandbox (concurrent
    /// evict/resume) → a retryable Conflict, never a forward against a stale id.
    #[tokio::test]
    async fn conflict_when_reattach_finds_session_moved() {
        let out = deliver_with_reattach(
            "answer",
            FAST_BUDGET,
            FAST_POLL,
            || async { Err::<(), SandboxError>(SandboxError::NotFound) },
            || async { Ok::<bool, ApiError>(false) },
        )
        .await;
        assert!(matches!(out, Err(ApiError::Conflict(_))));
    }

    /// The harness never re-binds within the budget → retryable Conflict (not a
    /// hard 500), so the orchestrator's onDeliveryError retry path applies rather
    /// than the thread wedging.
    #[tokio::test]
    async fn conflict_when_harness_never_rebinds() {
        let out = deliver_with_reattach(
            "prompt",
            FAST_BUDGET,
            FAST_POLL,
            || async { Err::<(), SandboxError>(SandboxError::NotFound) },
            || async { Ok::<bool, ApiError>(true) },
        )
        .await;
        assert!(matches!(out, Err(ApiError::Conflict(_))));
    }

    /// A non-`NotFound` forward error is a real failure, not a binding desync:
    /// surface it as Internal and do NOT reattach.
    #[tokio::test]
    async fn non_notfound_error_is_not_healed() {
        let reattached = Arc::new(AtomicUsize::new(0));
        let r = reattached.clone();
        let out = deliver_with_reattach(
            "prompt",
            FAST_BUDGET,
            FAST_POLL,
            || async { Err::<(), SandboxError>(SandboxError::Timeout) },
            || {
                let r = r.clone();
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                    Ok::<bool, ApiError>(true)
                }
            },
        )
        .await;
        assert!(matches!(out, Err(ApiError::Internal(_))));
        assert_eq!(
            reattached.load(Ordering::SeqCst),
            0,
            "a non-unbound error must not trigger a reattach",
        );
    }
}
