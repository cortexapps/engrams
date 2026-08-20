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

/// ADR 0108 A6: how an accepted prompt reaches the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptDelivery {
    /// Enqueue a Deliver op now — every follow-up prompt. The op-log
    /// ladder owns ordering, auto-resume, and redelivery.
    Enqueue,
    /// The create boot carries the prompt in the spawn env:
    /// `boot_on_reserved_host` peeks the row, stamps
    /// `ENGRAM_INITIAL_PROMPT*`, and marks it delivered after
    /// `start_agent` returns Ok — the harness's `run_started{prompt_id}`
    /// acks it. No Deliver op is minted, and the row's `not_before` is
    /// pushed one `ACK_TIMEOUT` out, so neither the executor nor the
    /// outbox shim has anything to chase during a normal boot; a boot
    /// that dies before the stamp leaves the row to come due and the
    /// ordinary rail recovers it.
    RidesBoot,
}

/// The hard API-level prompt size cap. There was NO bound before
/// (only non-empty) — the app-gRPC surface accepts 80 MiB frames, so an
/// unbounded prompt was a latent jsonb/outbox DoS. Generous: two orders
/// of magnitude above the 24 KiB spawn-env eligibility cutoff; a prompt
/// above THAT silently rides the outbox rail instead of the env.
const MAX_PROMPT_BYTES: usize = 1024 * 1024;

/// ADR 0051: transport-agnostic prompt core (gRPC `SendPrompt`).
/// ADR 0079: enqueue-only — the durable receipts + one outbox row (+ a
/// Deliver op, per `delivery`), then return. Delivery ordering (behind
/// an in-flight resume/evict), the auto-resume, and the mid-move wait
/// all ride the deliver op's position in the session op log; the 60s
/// mid-move HOLD this handler used to poll is gone.
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
    delivery: PromptDelivery,
) -> Result<&'static str, ApiError> {
    if text.is_empty() {
        return Err(ApiError::BadRequest("`text` is required".into()));
    }
    if text.len() > MAX_PROMPT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "`text` is {} bytes; the maximum is {MAX_PROMPT_BYTES}",
            text.len(),
        )));
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
        // ADR 0108 A6: a boot-carried prompt is not DUE — the boot is
        // its delivery vehicle, and a due row would have the shim
        // minting Deliver ops to chase a prompt that is already riding
        // the spawn. One ACK_TIMEOUT of grace covers the boot with wide
        // margin; a boot that dies first leaves the row to come due and
        // the ordinary rail recovers it.
        not_before: match delivery {
            PromptDelivery::Enqueue => now,
            PromptDelivery::RidesBoot => now + crate::session_verbs::ACK_TIMEOUT,
        },
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
    if matches!(delivery, PromptDelivery::Enqueue) {
        // ADR 0079: enqueue the Deliver op directly (no wake hop for
        // first delivery); the shim's NOTIFY/poll loop owns redelivery.
        crate::outbox_delivery::enqueue_deliver_op(state, id).await;
        state.outbox_wake.notify_one();
    }

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
    // ADR 0052 (2026-08-20 correction): the durable row stays UNACKED
    // through a running turn (`prompt_queued` no longer acks), so an
    // edit must land in BOTH copies — the PG row (what a redelivery
    // reads after a harness death) and, when the prompt already reached
    // the harness, its in-memory queue (what the turn boundary
    // consumes). The harness leg is a no-op for ids it doesn't hold
    // (an undelivered row has no queue copy), so forwarding is always
    // safe; it is REQUIRED to succeed only when PG had nothing (the
    // prompt lives solely in the queue — pre-correction rows).
    let row_updated = state
        .services
        .meta
        .outbox_update_prompt_text(&prompt_id, &text)
        .await
        .map_err(|e| ApiError::Internal(format!("edit queued prompt: {e}")))?;
    match state.resolve_sandbox(id).await {
        Some(sandbox_id) => {
            if let Err(e) = state
                .services
                .host
                .edit_queued_prompt(sandbox_id, prompt_id.clone(), text)
                .await
            {
                if !row_updated {
                    return Err(ApiError::Internal(format!("edit queued prompt: {e}")));
                }
                tracing::debug!(session_id = %id, prompt_id, error = %e,
                    "harness-side edit failed; the durable row carries the edit and a \
                     redelivery replays it");
            }
        }
        None if !row_updated => {
            return Err(ApiError::Conflict("session has no live sandbox".into()));
        }
        None => {}
    }
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
    // ADR 0052 (2026-08-20 correction): kill BOTH copies. The durable
    // row (unacked through a running turn) dies first, so the withdrawn
    // prompt can never redeliver — then the harness's in-memory copy is
    // dropped by the forwarded DequeueQueued, whose `PromptDequeued`
    // echo is also a terminal ack (belt-and-suspenders for the copy
    // that raced this delete). The web's transcript reconciles via the
    // PromptDequeued event on the harness path; for the PG-only path
    // the held echo is simply never consumed (same render outcome as
    // pre-0067's failed-forward shape).
    let row_deleted = state
        .services
        .meta
        .outbox_delete_unacked(&prompt_id)
        .await
        .map_err(|e| ApiError::Internal(format!("dequeue queued prompt: {e}")))?;
    match state.resolve_sandbox(id).await {
        Some(sandbox_id) => {
            if let Err(e) = state
                .services
                .host
                .dequeue_queued_prompt(sandbox_id, prompt_id.clone())
                .await
            {
                if !row_deleted {
                    return Err(ApiError::Internal(format!("dequeue queued prompt: {e}")));
                }
                tracing::debug!(session_id = %id, prompt_id, error = %e,
                    "harness-side dequeue failed; the durable row is gone so the prompt \
                     cannot redeliver (the queue copy dies with the harness)");
            }
        }
        None if !row_deleted => {
            return Err(ApiError::Conflict("session has no live sandbox".into()));
        }
        None => {}
    }
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

        send_prompt_core(
            &state,
            id,
            String::new(),
            "hello".into(),
            None,
            PromptDelivery::Enqueue,
        )
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

    /// ADR 0108 A6: `RidesBoot` writes the same durable accept (receipt
    /// + echo + row) but mints NO Deliver op and defers the row one
    /// ACK_TIMEOUT — the boot is the delivery vehicle, and nothing may
    /// chase a prompt that is already riding the spawn. The row stays
    /// the at-least-once backstop: it comes due on its own if the boot
    /// dies before the stamp.
    #[tokio::test]
    async fn rides_boot_accepts_durably_without_minting_a_deliver_op() {
        use engram_core::types::session_op::OpKind;
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));

        send_prompt_core(
            &state,
            id,
            format!("create:{id}"),
            "boot-carried".into(),
            None,
            PromptDelivery::RidesBoot,
        )
        .await
        .expect("accept");

        assert_eq!(mini.events.lock().len(), 2, "receipt + echo, as always");
        // Block-scoped: clippy's await_holding_lock is scope-based and
        // does not credit an explicit drop().
        {
            let outbox = mini.outbox.lock();
            assert_eq!(outbox.len(), 1, "the durable backstop row exists");
            assert!(
                outbox[0].not_before > outbox[0].created_at,
                "the row is deferred — not due while it rides the boot",
            );
        }
        assert!(
            !state
                .services
                .meta
                .op_pending_exists(id, OpKind::Deliver)
                .await
                .expect("op probe"),
            "RidesBoot mints no Deliver op",
        );
    }

    /// The API-level size cap (there was none): an oversize prompt is a
    /// BadRequest with no durable trace.
    #[tokio::test]
    async fn oversize_prompt_is_rejected_with_no_side_effects() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(idle_session(id));
        let err = send_prompt_core(
            &state,
            id,
            "pid".into(),
            "x".repeat(MAX_PROMPT_BYTES + 1),
            None,
            PromptDelivery::Enqueue,
        )
        .await
        .expect_err("oversize must be rejected");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
        assert!(mini.events.lock().is_empty(), "no durable writes");
        assert!(mini.outbox.lock().is_empty(), "no outbox row");
    }

    /// A rejected SendPrompt (terminal session → Gone) leaves NO durable
    /// trace: no receipt, no echo, no outbox row. (The terminal gate
    /// precedes the accept write — the pre-idempotency behavior that
    /// left orphan receipt rows for rejected prompts is gone.)
    #[tokio::test]
    async fn rejected_prompt_leaves_no_receipt() {
        let id = SessionId::new();
        let (state, mini, _local) = build_state_for_session(dead_session(id));

        let err = send_prompt_core(
            &state,
            id,
            String::new(),
            "hello".into(),
            None,
            PromptDelivery::Enqueue,
        )
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

        let _ = send_prompt_core(
            &state,
            id,
            "client-pid-42".into(),
            "hello".into(),
            None,
            PromptDelivery::Enqueue,
        )
        .await;

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
            PromptDelivery::Enqueue,
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

        let err = send_prompt_core(
            &state,
            id,
            "pid".into(),
            "hi".into(),
            Some("plan".into()),
            PromptDelivery::Enqueue,
        )
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
            PromptDelivery::Enqueue,
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

        send_prompt_core(
            &state,
            id,
            "pid".into(),
            "hello".into(),
            None,
            PromptDelivery::Enqueue,
        )
        .await
        .expect("plain prompt");

        let events = mini.events.lock();
        assert!(events.iter().all(|e| e.kind != "harness_mode_changed"));
        let outbox = mini.outbox.lock();
        assert_eq!(outbox.len(), 1);
        assert!(outbox[0].payload.get("mode").is_none());
    }
}
