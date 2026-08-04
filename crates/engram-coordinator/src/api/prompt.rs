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

    // ADR 0073: gate on terminal state only. NOT on Idle — an idle
    // session is exactly what the delivery driver resumes behind this
    // enqueue. Dead/Completed/Failed can never consume the prompt, so
    // reject now rather than letting a row rot. Gating BEFORE the
    // accept write also means a rejected SendPrompt leaves no orphan
    // receipt row (the pre-idempotency behavior PR #556 finding #3
    // documented).
    let session = state.services.meta.get_session(id).await?;
    if session.status.is_terminal() {
        return Err(ApiError::Gone(format!(
            "session is {} — fork it to continue from its last snapshot",
            session.status.as_str()
        )));
    }

    let prompt_text = text;

    // The accept-time events, appended atomically WITH the outbox row
    // and idempotently on `prompt_id` (a client/orchestrator retry
    // appends nothing — the duplicate-echo fix, PR #556 finding #3):
    //
    // - `prompt_received` (issue #527): the durable "the user asked at
    //   time T" receipt, written before the auto-resume below can
    //   spend tens of seconds on a cold restore.
    // - the user echo: the harness adapter never echoes the prompt
    //   back through the assistant stream, so without this row the
    //   user's turn is invisible to subscribers. The web dedupes its
    //   optimistic bubble against the `prompt_id` tag (ADR 0052).
    // - `harness_mode_changed` (ADR 0107), when a mode rides along.
    //
    // ORDER MATTERS: delivery makes the harness emit
    // `run_started{prompt_id}` into this same log, and the web
    // transcript holds the echo until that arrives. The single
    // transaction makes the echo's idx precede its `run_started` BY
    // CONSTRUCTION — the outbox row and the echo become visible at
    // the same commit, so delivery cannot begin first. (Pre-0067 this
    // leaned on the synchronous forward; prod session 68c70a65.)
    let mut events = vec![
        SessionEvent::PromptReceived {
            prompt_id: prompt_id.clone(),
            at: now,
        },
        SessionEvent::HarnessAgentMessage {
            run_id: String::new(),
            message_id: format!("user-{}", state.services.entropy.uuid()),
            role: AgentRole::User,
            text: prompt_text.clone(),
            prompt_id: Some(prompt_id.clone()),
            at: now,
        },
    ];
    if let Some(mode) = &harness_mode {
        events.push(SessionEvent::HarnessModeChanged {
            mode: mode.clone(),
            at: now,
        });
    }
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
    // The durable accept. From here the command cannot be lost: the
    // delivery driver forwards it (resuming the session first if
    // needed) and redelivers until the harness's confirming event acks
    // the row.
    if state
        .emit_prompt_accept(id, events, &row)
        .await
        .map_err(|e| ApiError::Internal(format!("accept prompt: {e}")))?
        .is_none()
    {
        tracing::debug!(
            session_id = %id,
            prompt_id,
            "duplicate SendPrompt (same prompt_id); accept is idempotent — nudging delivery only",
        );
    }
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
    async fn prompt_received_is_recorded_before_any_auto_resume_runs() {
        // The idempotent accept writes the receipt + echo atomically at
        // accept time, BEFORE the delivery driver's auto-resume can
        // spend tens of seconds on a cold restore (issue #527). The
        // delivery machinery never runs in this unit harness — the
        // receipt must exist anyway.
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));

        send_prompt_core(&state, id, String::new(), "hello".into(), None)
            .await
            .expect("an idle session accepts the prompt");

        let events = mini.events.lock();
        assert_eq!(
            events.len(),
            2,
            "the accept records the receipt + the user echo, nothing else",
        );
        assert_eq!(events[0].kind, "prompt_received");
        assert_eq!(events[1].kind, "agent_message");
        let recorded_prompt_id = events[0].payload["prompt_id"]
            .as_str()
            .expect("prompt_id string field");
        assert!(
            !recorded_prompt_id.is_empty(),
            "an empty caller prompt_id must be minted before the receipt is written",
        );
    }

    /// A rejected SendPrompt (terminal session → Gone) leaves NO durable
    /// trace: no receipt, no echo, no outbox row. (The terminal gate
    /// precedes the accept write — the pre-idempotency behavior that
    /// left orphan receipt rows for rejected prompts is gone.)
    #[tokio::test]
    async fn rejected_prompt_leaves_no_receipt() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(dead_session(id));

        let err = send_prompt_core(&state, id, String::new(), "hello".into(), None)
            .await
            .expect_err("a Dead session cannot accept a prompt");
        assert!(
            matches!(err, ApiError::Gone(_)),
            "expected the Dead-session Gone mapping, got {err:?}",
        );
        assert!(
            mini.events.lock().is_empty(),
            "a rejected prompt writes nothing",
        );
    }

    /// A caller-supplied `prompt_id` (the web's client-minted id) is carried
    /// verbatim into the receipt — not re-minted — so it joins cleanly
    /// against the same id's `run_started` event.
    #[tokio::test]
    async fn prompt_received_carries_the_caller_supplied_prompt_id() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));

        let _ = send_prompt_core(&state, id, "client-pid-42".into(), "hello".into(), None).await;

        let events = mini.events.lock();
        assert_eq!(events.len(), 2, "receipt + echo");
        assert_eq!(events[0].kind, "prompt_received");
        assert_eq!(events[0].payload["prompt_id"], "client-pid-42");
        assert_eq!(events[1].payload["prompt_id"], "client-pid-42");
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
