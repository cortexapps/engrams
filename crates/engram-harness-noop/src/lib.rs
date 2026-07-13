//! First-party test harness: emits a configurable cadence of
//! [`HarnessEvent`]s over the harness channel.
//!
//! Library-shaped (no binary) so callers — primarily Track B/D
//! integration tests — can spawn it in-process over an
//! `tokio::io::duplex` pair without standing up a separate VM. A
//! standalone binary wrapper that connects via vsock from inside a
//! Firecracker guest will land alongside Track B/D when those wire
//! up the in-guest path.

use std::time::Duration;

use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessEvent,
    HarnessFrame,
};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};

/// Configuration for one noop-harness run. All fields have sensible
/// defaults; tests typically override only the bits they care about.
#[derive(Clone, Debug, Deserialize)]
pub struct NoopConfig {
    pub session_id: SessionId,
    /// Emitted as the `harness_version` in [`HarnessAttach`]. Useful
    /// for log filtering when multiple noop harnesses run side by side.
    pub harness_version: String,
    /// Number of tool calls to emit before falling silent. After the
    /// last tool call the harness emits a single [`HarnessEvent::Idle`]
    /// and stops emitting (it stays connected so the host can still
    /// send Shutdown).
    pub tool_calls: u32,
    /// Wall-clock between successive ToolCallStarted events. Each
    /// tool call's Completed event fires `tool_call_duration_ms` after
    /// its Started, so the cadence and per-call duration are
    /// independent knobs.
    pub interval: Duration,
    /// Synthetic per-tool-call duration reported on `ToolCallCompleted`.
    pub tool_call_duration_ms: u64,
    /// Synthetic `result_summary` text the noop emits on each
    /// `ToolCallCompleted`. Slack/UI consumers see this; tests
    /// typically pass small unique strings so they can assert on
    /// order in event-stream tests.
    pub result_summary_template: String,
    /// Synthetic assistant text emitted via `AgentMessage` between
    /// runs. Set to a non-empty string to exercise the new chat-
    /// shaped wire path.
    pub agent_message_template: String,
    /// Whether to send `RunCompleted` at the end. Off by default so
    /// the harness stays at "between runs" when the test wants to
    /// exercise idle eviction.
    pub send_run_completed: bool,
    /// ADR 0067 attach token (sandbox half). Tests bind the hub with
    /// the same pair so the handshake validates.
    pub sandbox_id: SandboxId,
    /// ADR 0067 attach token (generation half).
    pub binding_epoch: u64,
}

impl NoopConfig {
    pub fn for_session(session_id: SessionId) -> Self {
        Self {
            session_id,
            harness_version: "engram-harness-noop/0.1.0".into(),
            tool_calls: 3,
            interval: Duration::from_millis(50),
            tool_call_duration_ms: 10,
            result_summary_template: "ok (noop)".into(),
            agent_message_template: "noop assistant message".into(),
            send_run_completed: false,
            sandbox_id: SandboxId::new(),
            binding_epoch: 1,
        }
    }
}

/// Reasons a noop run can finish. Tests assert on these to lock down
/// the protocol contract (e.g. "shutdown command must terminate the
/// run cleanly").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoopOutcome {
    /// Emitted all `tool_calls` plus an Idle event, then peer closed
    /// the connection (or the test dropped its end).
    EmittedAndPeerClosed,
    /// Host sent `HarnessCommand::Shutdown { .. }`. The harness
    /// returns Ok early without emitting any further events.
    Shutdown,
    /// Host rejected the attach handshake (`HarnessAttachAck { ok: false }`).
    /// Tests use this to verify rejection paths.
    AttachRejected,
}

/// Run one noop-harness session against `stream`. Returns when the
/// connection closes or the host sends Shutdown.
pub async fn run<S>(stream: S, cfg: NoopConfig) -> Result<NoopOutcome, NoopError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);

    write_msg(
        &mut writer,
        &HarnessAttach {
            session_id: cfg.session_id,
            sandbox_id: cfg.sandbox_id,
            binding_epoch: cfg.binding_epoch,
            harness_version: cfg.harness_version.clone(),
        },
    )
    .await
    .map_err(NoopError::Io)?;

    let mut reader = reader;
    let ack: HarnessAttachAck = read_msg(&mut reader).await.map_err(NoopError::Io)?;
    if !ack.ok {
        tracing::warn!(message = ?ack.message, "noop harness attach rejected");
        return Ok(NoopOutcome::AttachRejected);
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    write_msg(
        &mut writer,
        &HarnessFrame::Event(HarnessEvent::RunStarted {
            run_id: run_id.clone(),
            prompt_summary: Some("noop run".into()),
            prompt_id: None,
        }),
    )
    .await
    .map_err(NoopError::Io)?;

    // Background task: consume host-sent commands. We watch for
    // Shutdown so we can short-circuit. Using a tokio::sync::watch
    // keeps the main loop's cancellation cheap.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let mut reader_only = reader;
    let reader_task = tokio::spawn(async move {
        loop {
            match read_msg::<_, HarnessFrame>(&mut reader_only).await {
                Ok(HarnessFrame::Command(HarnessCommand::Shutdown { .. })) => {
                    let _ = shutdown_tx.send(true);
                    return;
                }
                Ok(HarnessFrame::Command(HarnessCommand::Checkpoint { .. })) => {
                    // Noop has no transcript to flush; ack via the
                    // writer channel implicitly.
                }
                Ok(HarnessFrame::Command(HarnessCommand::Prompt { .. })) => {
                    // Noop ignores prompts — its run shape is fixed
                    // by the config. A real adapter would queue the
                    // prompt and start a new run after the current
                    // Idle. Tests for the prompt path use
                    // engram-harness-claude or a fixture noop.
                }
                Ok(HarnessFrame::Command(HarnessCommand::Interrupt)) => {
                    // Noop has no in-flight child to SIGINT — nothing
                    // to interrupt. A real adapter stops its current
                    // run and emits RunInterrupted + Idle.
                }
                Ok(HarnessFrame::Command(HarnessCommand::EditQueued { .. }))
                | Ok(HarnessFrame::Command(HarnessCommand::DequeueQueued { .. })) => {
                    // Phase 1b queue mutations. Noop has a fixed run shape
                    // and never queues, so there's nothing to edit/cancel.
                }
                Ok(HarnessFrame::Command(HarnessCommand::AnswerQuestion { .. })) => {
                    // ADR 0054: noop never calls AskUserQuestion, so it never
                    // receives an answer to feed back. Nothing to do.
                }
                Ok(HarnessFrame::Command(HarnessCommand::ToolResult { .. })) => {
                    // ADR 0089: noop never requests a registered tool, so it
                    // has no pending call that could consume this result.
                }
                Ok(HarnessFrame::Event(_)) => {
                    // Host shouldn't send Events; ignore.
                }
                Err(_) => return,
            }
        }
    });

    for i in 0..cfg.tool_calls {
        if *shutdown_rx.borrow() {
            reader_task.abort();
            return Ok(NoopOutcome::Shutdown);
        }
        let tool_call_id = format!("noop-{i}");
        let started = HarnessFrame::Event(HarnessEvent::ToolCallStarted {
            run_id: run_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: "Noop".into(),
            args_summary: None,
        });
        if write_msg(&mut writer, &started).await.is_err() {
            // Peer closed — clean exit.
            reader_task.abort();
            return Ok(NoopOutcome::EmittedAndPeerClosed);
        }
        // Allow Shutdown to interrupt during the per-call delay.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(cfg.tool_call_duration_ms)) => {},
            _ = shutdown_rx.changed() => {
                reader_task.abort();
                return Ok(NoopOutcome::Shutdown);
            }
        }
        let completed = HarnessFrame::Event(HarnessEvent::ToolCallCompleted {
            run_id: run_id.clone(),
            tool_call_id,
            tool_name: "Noop".into(),
            ok: true,
            duration_ms: cfg.tool_call_duration_ms,
            result_summary: Some(cfg.result_summary_template.clone()),
        });
        if write_msg(&mut writer, &completed).await.is_err() {
            reader_task.abort();
            return Ok(NoopOutcome::EmittedAndPeerClosed);
        }
        // Track B exercise: emit one synthetic AgentMessage after
        // each tool call so the SSE flow exercises the chat-shaped
        // wire path. Skip if the template is empty (legacy tests).
        if !cfg.agent_message_template.is_empty() {
            let msg = HarnessFrame::Event(HarnessEvent::AgentMessage {
                run_id: run_id.clone(),
                message_id: format!("noop-msg-{i}"),
                role: engram_harness_proto::AgentRole::Assistant,
                text: cfg.agent_message_template.clone(),
            });
            if write_msg(&mut writer, &msg).await.is_err() {
                reader_task.abort();
                return Ok(NoopOutcome::EmittedAndPeerClosed);
            }
        }
        // Idle gap between tool calls.
        if i + 1 < cfg.tool_calls {
            tokio::select! {
                _ = tokio::time::sleep(cfg.interval) => {},
                _ = shutdown_rx.changed() => {
                    reader_task.abort();
                    return Ok(NoopOutcome::Shutdown);
                }
            }
        }
    }

    if cfg.send_run_completed {
        let _ = write_msg(
            &mut writer,
            &HarnessFrame::Event(HarnessEvent::RunCompleted { run_id, ok: true }),
        )
        .await;
    }

    let _ = write_msg(&mut writer, &HarnessFrame::Event(HarnessEvent::Idle)).await;

    // Stay connected so the host can issue commands until peer
    // disconnect. This matches the production contract: a real
    // adapter doesn't exit just because it ran out of work — it
    // sticks around in case the host wants it to suspend or shut
    // down. For tests the parent typically drops its end after
    // assertions; the reader_task notices EOF and we observe Shutdown
    // through the watch channel.
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }
    reader_task.abort();
    if *shutdown_rx.borrow() {
        Ok(NoopOutcome::Shutdown)
    } else {
        Ok(NoopOutcome::EmittedAndPeerClosed)
    }
}

#[derive(Debug)]
pub enum NoopError {
    Io(std::io::Error),
}

impl std::fmt::Display for NoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "noop harness io: {e}"),
        }
    }
}

impl std::error::Error for NoopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_harness_proto::{HarnessAttach, HarnessAttachAck};

    #[tokio::test]
    async fn noop_emits_attach_then_run_started_then_tool_calls_then_idle() {
        let (host_side, harness_side) = tokio::io::duplex(1 << 16);
        let session_id = SessionId::new();
        let mut cfg = NoopConfig::for_session(session_id);
        cfg.tool_calls = 2;
        cfg.interval = Duration::from_millis(1);
        cfg.tool_call_duration_ms = 1;

        let host_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(host_side);
            // Read attach, ack OK.
            let attach: HarnessAttach = read_msg(&mut hr).await.unwrap();
            assert_eq!(attach.session_id, session_id);
            write_msg(
                &mut hw,
                &HarnessAttachAck {
                    ok: true,
                    reject: None,
                    message: None,
                },
            )
            .await
            .unwrap();

            let mut events = Vec::new();
            // Read all frames until peer EOF (after Idle, the harness
            // waits for shutdown — we drop our writer to release).
            while let Ok(frame) = read_msg::<_, HarnessFrame>(&mut hr).await {
                if let HarnessFrame::Event(ev) = frame {
                    let is_idle = matches!(ev, HarnessEvent::Idle);
                    events.push(ev);
                    if is_idle {
                        break;
                    }
                }
            }
            // After Idle we drop our writer to close the connection.
            drop(hw);
            events
        });

        let outcome = run(harness_side, cfg).await.unwrap();
        // The harness blocks waiting for shutdown after Idle; the
        // host above drops its writer which causes EOF on the harness
        // reader_task, but the writer half is still alive (we wrote
        // Idle). Outcome is "peer closed" since reader_task exits.
        // Either Shutdown or EmittedAndPeerClosed is acceptable here
        // depending on which closes first.
        assert!(matches!(
            outcome,
            NoopOutcome::EmittedAndPeerClosed | NoopOutcome::Shutdown
        ));

        let events = host_task.await.unwrap();
        // Expected stream after Track B's wire reshape: per tool call,
        // the noop emits ToolCallStarted → ToolCallCompleted →
        // AgentMessage (a synthetic assistant text). With 2 tool
        // calls plus the bracketing RunStarted + Idle, that's 8 events.
        assert_eq!(events.len(), 8, "got: {events:?}");
        assert!(matches!(events[0], HarnessEvent::RunStarted { .. }));
        assert!(matches!(events[1], HarnessEvent::ToolCallStarted { .. }));
        assert!(matches!(events[2], HarnessEvent::ToolCallCompleted { .. }));
        assert!(matches!(events[3], HarnessEvent::AgentMessage { .. }));
        assert!(matches!(events[4], HarnessEvent::ToolCallStarted { .. }));
        assert!(matches!(events[5], HarnessEvent::ToolCallCompleted { .. }));
        assert!(matches!(events[6], HarnessEvent::AgentMessage { .. }));
        assert!(matches!(events[7], HarnessEvent::Idle));
    }

    #[tokio::test]
    async fn noop_returns_attach_rejected_when_host_acks_false() {
        let (host_side, harness_side) = tokio::io::duplex(1 << 16);
        let cfg = NoopConfig::for_session(SessionId::new());

        let host_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(host_side);
            let _: HarnessAttach = read_msg(&mut hr).await.unwrap();
            write_msg(
                &mut hw,
                &HarnessAttachAck {
                    ok: false,
                    reject: Some(engram_harness_proto::AttachReject::UnknownBinding),
                    message: Some("nope".into()),
                },
            )
            .await
            .unwrap();
        });

        let outcome = run(harness_side, cfg).await.unwrap();
        assert_eq!(outcome, NoopOutcome::AttachRejected);
        host_task.await.unwrap();
    }

    #[tokio::test]
    async fn noop_short_circuits_on_shutdown_command() {
        let (host_side, harness_side) = tokio::io::duplex(1 << 16);
        let mut cfg = NoopConfig::for_session(SessionId::new());
        cfg.tool_calls = 100;
        cfg.interval = Duration::from_millis(50);

        let host_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(host_side);
            let _: HarnessAttach = read_msg(&mut hr).await.unwrap();
            write_msg(
                &mut hw,
                &HarnessAttachAck {
                    ok: true,
                    reject: None,
                    message: None,
                },
            )
            .await
            .unwrap();

            // Wait until we've seen at least one ToolCallStarted, then
            // tell the harness to shut down. Asserts that Shutdown
            // interrupts the per-call interval cleanly.
            loop {
                let frame: HarnessFrame = read_msg(&mut hr).await.unwrap();
                if matches!(
                    frame,
                    HarnessFrame::Event(HarnessEvent::ToolCallStarted { .. })
                ) {
                    break;
                }
            }
            write_msg(
                &mut hw,
                &HarnessFrame::Command(HarnessCommand::Shutdown { grace_secs: 0 }),
            )
            .await
            .unwrap();
        });

        let outcome = run(harness_side, cfg).await.unwrap();
        assert_eq!(outcome, NoopOutcome::Shutdown);
        host_task.await.unwrap();
    }
}
