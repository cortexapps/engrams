//! `POST /sessions/:id/prompt` — durably enqueue a prompt for a session.
//!
//! ADR 0073 phase 2: `SendPrompt` no longer delivers
//! synchronously. The core emits the durable receipts (prompt_received
//! and the user echo), INSERTs one `session_outbox` row, and returns
//! 202-shaped success immediately — the outbox delivery driver
//! (`crate::outbox_delivery`) resumes the session BEHIND the enqueue,
//! forwards, self-heals the harness-unbound desync, and redelivers
//! until the confirming harness event acks the row. A caller therefore
//! never waits on a 12-89s resume, and a connection bounce can no
//! longer eat the command (the e35ed1fa class): the row survives until
//! `run_started{prompt_id}` lands.
//!
//! Dead sessions return 410 Gone — the only affordance there is
//! `engram session fork <id>`.

use engram_core::SessionId;
use engram_harness_proto::AgentRole;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// ADR 0051: transport-agnostic prompt core (gRPC `SendPrompt`).
/// ADR 0079: enqueue-only — the durable receipts + one outbox row + a
/// Deliver op, then return. Delivery ordering (behind an in-flight
/// resume/evict), the auto-resume, and the mid-move wait all ride the
/// deliver op's position in the session op log; the 60s mid-move HOLD
/// this handler used to poll is gone.
pub(crate) async fn send_prompt_core(
    state: &SharedState,
    id: SessionId,
    prompt_id: String,
    text: String,
    // ADR 0107: optional session-mode directive riding this prompt (e.g.
    // `plan`). Validated against the session harness's declared descriptor
    // modes BEFORE any durable write it causes; rides the outbox payload and
    // `HarnessCommand::Prompt.mode`.
    harness_mode: Option<String>,
) -> Result<&'static str, ApiError> {
    if text.is_empty() {
        return Err(ApiError::BadRequest("`text` is required".into()));
    }
    let harness_mode = harness_mode.filter(|m| !m.is_empty());
    if let Some(mode) = &harness_mode {
        let harness = state
            .services
            .meta
            .get_session_harness(id)
            .await
            .map_err(|e| ApiError::Internal(format!("session harness lookup: {e}")))?
            .ok_or_else(|| {
                ApiError::BadRequest(
                    "harness_mode is only valid on an agent-mode session with a harness".into(),
                )
            })?;
        let descriptor = crate::api::sessions::resolve_descriptor(state, &harness).await?;
        crate::api::sessions::validate_harness_mode(&descriptor, &harness, Some(mode))?;
    }
    // Phase 1b: the client (web) mints `prompt_id` so it can correlate its
    // optimistic bubble with the server echo + `RunStarted{prompt_id}`
    // (this is what dedupes the double-render). Mint one if a non-web
    // caller left it empty, so the wire is always uniform.
    let prompt_id = if prompt_id.is_empty() {
        state.services.entropy.uuid().to_string()
    } else {
        prompt_id
    };
    let now = state.services.clock.now_utc();

    // Issue #527 Phase 1: the durable "the user asked at time T" receipt —
    // the FIRST PG write of this function, before the auto-resume below.
    // The auto-resume can take tens to hundreds of seconds (a cold FC
    // restore); without a receipt written before it starts, the earliest
    // durable trace of "the user asked for something" post-dates the
    // resume, and every prompt→first-token latency number becomes a lower
    // bound reconstructed from the `idle→created` transition. This event
    // is coordinator-authoritative and stays true across a guest-state
    // rewind (the user genuinely did send the prompt), so
    // `rewind_session_to_cursor` excludes `prompt_received` from its
    // tombstone UPDATE. Best-effort like the user-echo emit below: an emit
    // failure logs + proceeds — we never 500 the caller over telemetry.
    //
    // PR #556 review finding #3: this write lands BEFORE any request
    // validation below (session state, mid-move HOLD), so a subsequently
    // rejected `SendPrompt` still leaves a permanent receipt row, and a
    // client retry of a *retryable* rejection (e.g. the mid-move HOLD's
    // documented Conflict) that reuses the same `prompt_id` writes a
    // second one. This is spec-inherited from issue #527's emit-before-
    // resume + `DESC LIMIT 1` design, not a regression introduced here —
    // `prompt_received_seconds_ago`'s `ORDER BY idx DESC LIMIT 1` anchors
    // on the LAST attempt, undercounting latency for the retried case.
    // Deduplicating retries (or switching to `ASC LIMIT 1` to anchor on
    // the user-perceived first ask) is left to a follow-up — out of scope
    // for Phase 1, which only needed *a* durable receipt to exist.
    if let Err(e) = state
        .emit(
            id,
            SessionEvent::PromptReceived {
                prompt_id: prompt_id.clone(),
                at: now,
            },
        )
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "emit prompt_received event failed");
    }

    // ADR 0073: gate on terminal state only. NOT on Idle — an idle
    // session is exactly what the delivery driver resumes behind this
    // enqueue. Dead/Completed/Failed can never consume the prompt, so
    // reject now rather than letting a row rot.
    let session = state.services.meta.get_session(id).await?;
    if session.status.is_terminal() {
        return Err(ApiError::Gone(format!(
            "session is {} — fork it to continue from its last snapshot",
            session.status.as_str()
        )));
    }

    let prompt_text = text;

    // Record the user's prompt in the session event log BEFORE the outbox
    // row exists. The harness adapter never echoes the prompt back through
    // Claude's stream-json output — it only translates the *assistant*
    // response — so without this entry the user's turn is invisible to
    // subscribers.
    //
    // ORDER MATTERS: delivery makes the harness start the run and emit
    // `run_started`, appended to this same log. The web transcript
    // (buildMessages) HOLDS a `prompt_id` user echo and only renders it when
    // it reaches the consuming `run_started{prompt_id}` (ADR 0052 type-ahead).
    // Emitting before the enqueue guarantees the echo's `idx` precedes its
    // `run_started` BY CONSTRUCTION — delivery cannot begin until the row
    // exists, and the row is inserted after this emit. (Pre-0067 this
    // ordering leaned on the synchronous forward; prod session 68c70a65.)
    //
    // Best-effort: an emit failure logs + proceeds (the prompt still
    // delivers); we never 500 the caller over a missing echo.
    if let Err(e) = state
        .emit(
            id,
            SessionEvent::HarnessAgentMessage {
                run_id: String::new(),
                message_id: format!("user-{}", state.services.entropy.uuid()),
                role: AgentRole::User,
                text: prompt_text.clone(),
                // Phase 1b: tag the user-echo with the client prompt_id so
                // the web dedupes its optimistic bubble against this event
                // (the double-render fix) instead of rendering both.
                prompt_id: Some(prompt_id.clone()),
                at: now,
            },
        )
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "emit user prompt event failed");
    }

    // ADR 0107: the durable "the user selected mode M" fact — emitted only
    // after validation, before the outbox row, so the mode marker's idx
    // precedes the run it applies to. Coordinator-authoritative (excluded
    // from rewind tombstoning). Best-effort like the receipts above.
    if let Some(mode) = &harness_mode {
        if let Err(e) = state
            .emit(
                id,
                SessionEvent::HarnessModeChanged {
                    mode: mode.clone(),
                    at: now,
                },
            )
            .await
        {
            tracing::warn!(session_id = %id, error = %e, "emit harness_mode_changed failed");
        }
    }

    // The durable enqueue. From here the command cannot be lost: the
    // delivery driver forwards it (resuming the session first if needed)
    // and redelivers until the harness's confirming event acks the row.
    let payload = match &harness_mode {
        Some(mode) => serde_json::json!({ "text": prompt_text, "mode": mode }),
        None => serde_json::json!({ "text": prompt_text }),
    };
    let row = engram_core::types::outbox::OutboxRow {
        prompt_id: prompt_id.clone(),
        session_id: id,
        kind: engram_core::types::outbox::OutboxKind::Prompt,
        payload,
        created_at: now,
        attempts: 0,
        not_before: now,
        delivered_at: None,
        acked_at: None,
    };
    state
        .services
        .meta
        .outbox_enqueue(&row)
        .await
        .map_err(|e| ApiError::Internal(format!("enqueue prompt: {e}")))?;
    // ADR 0079: enqueue the Deliver op directly (no wake hop for first
    // delivery); the shim's NOTIFY/poll loop owns redelivery.
    crate::outbox_delivery::enqueue_deliver_op(state, id).await;
    state.outbox_wake.notify_one();

    Ok("prompt queued")
}

/// ADR 0089: accept an opaque result for an orchestrator-registered tool.
/// The submitted event and durable outbox row commit atomically so surfaces
/// never lock a pending call that has no delivery obligation.
pub(crate) async fn complete_tool_call_core(
    state: &SharedState,
    id: SessionId,
    tool_call_id: String,
    result_json: String,
) -> Result<&'static str, ApiError> {
    if tool_call_id.is_empty() {
        return Err(ApiError::BadRequest("`tool_call_id` is required".into()));
    }
    let session = state.services.meta.get_session(id).await?;
    if session.status.is_terminal() {
        return Err(ApiError::Gone(format!(
            "session is {} — fork it to continue from its last snapshot",
            session.status.as_str()
        )));
    }

    let now = state.services.clock.now_utc();
    let row = engram_core::types::outbox::OutboxRow {
        prompt_id: engram_core::types::outbox::tool_result_outbox_id(id, &tool_call_id),
        session_id: id,
        kind: engram_core::types::outbox::OutboxKind::ToolResult,
        payload: serde_json::json!({
            "tool_call_id": tool_call_id.clone(),
            "result_json": result_json.clone(),
        }),
        created_at: now,
        attempts: 0,
        not_before: now,
        delivered_at: None,
        acked_at: None,
    };
    state
        .emit_with_outbox(
            id,
            SessionEvent::ToolResultSubmitted {
                tool_call_id,
                result_json,
                at: now,
            },
            &row,
        )
        .await?;
    crate::outbox_delivery::enqueue_deliver_op(state, id).await;
    state.outbox_wake.notify_one();
    Ok("tool result queued")
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
    // ADR 0073: the prompt may still be an UNDELIVERED outbox row (the
    // driver hasn't handed it to the relay yet) — edit it in place in
    // PG. Otherwise it lives in the harness's type-ahead queue: edit it
    // there, exactly as before.
    if state
        .services
        .meta
        .outbox_update_prompt_text(&prompt_id, &text)
        .await
        .map_err(|e| ApiError::Internal(format!("edit queued prompt: {e}")))?
    {
        return Ok("queued prompt edited");
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
    // ADR 0073: an undelivered outbox row is dequeued by deleting it —
    // it never reached the harness. The web's transcript reconciles via
    // the PromptDequeued event, which the HOST path emits; for the
    // PG-only path the held echo is simply never consumed (same render
    // outcome as pre-0067's failed-forward shape).
    if state
        .services
        .meta
        .outbox_delete_undelivered(&prompt_id)
        .await
        .map_err(|e| ApiError::Internal(format!("dequeue queued prompt: {e}")))?
    {
        return Ok("queued prompt dequeued");
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
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    //! ADR 0073: the pre-outbox `deliver_with_reattach` unit tests are
    //! retired WITH the helper — that coverage lives in the outbox
    //! delivery driver now (`outbox_delivery`: the NotFound→reattach
    //! arm) and in the store-contract live-PG tests (`outbox_live_pg`).
    use super::*;

    // -- Issue #527 Phase 1: prompt_received emit ordering --------------

    /// `AppState` fixture backed by `MiniMeta`, so assertions can inspect
    /// the exact `session_events` order `send_prompt_core` produced.
    /// Shared with `api::snapshot`'s `evicting_gate_tests` via
    /// `state::tests::build_state_for_session` (PR #556 review finding #4 —
    /// same-crate unit test modules share `pub(crate)` fns fine).
    use crate::state::tests::build_state_for_session;

    fn dead_session(id: SessionId) -> engram_core::types::Session {
        engram_core::types::Session {
            id,
            status: engram_core::types::SessionState::Dead,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:prompt-receipt".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            last_event_at: None,
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    /// The structural invariant this issue exists to create: `prompt_received`
    /// is written as the FIRST PG side-effect of `send_prompt_core`, before
    /// the terminal-state gate (and, post-ADR-0079, before the outbox row +
    /// Deliver op). A `Dead` session fails the gate immediately with no
    /// further side effects (no enqueue, no user-echo) — so if the receipt
    /// survives as the sole recorded event, it proves the emit happens
    /// unconditionally up front rather than being contingent on a successful
    /// delivery.
    #[tokio::test]
    async fn prompt_received_is_recorded_even_when_auto_resume_fails_outright() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(dead_session(id));

        let err = send_prompt_core(&state, id, String::new(), "hello".into(), None)
            .await
            .expect_err("a Dead session cannot auto-resume");
        assert!(
            matches!(err, ApiError::Gone(_)),
            "expected the Dead-session Gone mapping, got {err:?}",
        );

        let events = mini.events.lock();
        assert_eq!(
            events.len(),
            1,
            "prompt_received must be recorded even though auto-resume (and \
             therefore the user-echo + delivery) never ran",
        );
        assert_eq!(events[0].kind, "prompt_received");
        let recorded_prompt_id = events[0].payload["prompt_id"]
            .as_str()
            .expect("prompt_id string field");
        assert!(
            !recorded_prompt_id.is_empty(),
            "an empty caller prompt_id must be minted before the receipt is written",
        );
    }

    /// A caller-supplied `prompt_id` (the web's client-minted id) is carried
    /// verbatim into the receipt — not re-minted — so it joins cleanly
    /// against the same id's `run_started` event.
    #[tokio::test]
    async fn prompt_received_carries_the_caller_supplied_prompt_id() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(dead_session(id));

        let _ = send_prompt_core(&state, id, "client-pid-42".into(), "hello".into(), None).await;

        let events = mini.events.lock();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "prompt_received");
        assert_eq!(events[0].payload["prompt_id"], "client-pid-42");
    }

    // -- ADR 0107: harness_mode validation + event + payload ------------

    fn idle_session(id: SessionId) -> engram_core::types::Session {
        engram_core::types::Session {
            status: engram_core::types::SessionState::Idle,
            ..dead_session(id)
        }
    }

    /// An unknown mode is rejected BEFORE any durable write — no receipt,
    /// no event, no outbox row. (The mode gate deliberately precedes the
    /// unconditional `prompt_received` emit: a typo'd mode is a caller
    /// error, not a user ask.)
    #[tokio::test]
    async fn unknown_harness_mode_is_rejected_with_no_side_effects() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));
        *mini.harness.lock() = Some("claude".into());

        let err = send_prompt_core(
            &state,
            id,
            "pid".into(),
            "hello".into(),
            Some("bogus".into()),
        )
        .await
        .expect_err("unknown mode must be rejected");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
        assert!(mini.events.lock().is_empty(), "no durable writes");
        assert!(mini.outbox.lock().is_empty(), "no outbox row");
    }

    /// A mode on a harness-less session (dev_vm) is a BadRequest, not a
    /// silent drop.
    #[tokio::test]
    async fn harness_mode_without_a_harness_is_rejected() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));
        assert!(mini.harness.lock().is_none());

        let err = send_prompt_core(&state, id, "pid".into(), "hi".into(), Some("plan".into()))
            .await
            .expect_err("mode without a harness must be rejected");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
    }

    /// The happy path: a declared mode emits `harness_mode_changed` (after
    /// the receipt + user echo, before the enqueue) and rides the outbox
    /// Prompt payload so the deliver verb can forward it on
    /// `HarnessCommand::Prompt.mode`.
    #[tokio::test]
    async fn declared_harness_mode_emits_the_event_and_rides_the_outbox_payload() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));
        *mini.harness.lock() = Some("claude".into());

        send_prompt_core(
            &state,
            id,
            "pid".into(),
            "plan it".into(),
            Some("plan".into()),
        )
        .await
        .expect("declared mode is accepted");

        let events = mini.events.lock();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["prompt_received", "agent_message", "harness_mode_changed"]
        );
        assert_eq!(events[2].payload["mode"], "plan");

        let outbox = mini.outbox.lock();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].payload["text"], "plan it");
        assert_eq!(outbox[0].payload["mode"], "plan");
    }

    /// No mode → no `harness_mode_changed` event and no `mode` key in the
    /// payload (the wire field stays `None`, meaning "no change").
    #[tokio::test]
    async fn absent_harness_mode_leaves_no_mode_trace() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));

        send_prompt_core(&state, id, "pid".into(), "hello".into(), None)
            .await
            .expect("plain prompt");

        let events = mini.events.lock();
        assert!(events.iter().all(|e| e.kind != "harness_mode_changed"));
        let outbox = mini.outbox.lock();
        assert_eq!(outbox.len(), 1);
        assert!(outbox[0].payload.get("mode").is_none());
    }
}
