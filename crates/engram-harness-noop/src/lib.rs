//! Deterministic engine for harness transport and stack tests.
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use engram_core::SessionId;
use engram_harness_proto::attach_token::AttachToken;
use engram_harness_proto::{HarnessCommand, HarnessEvent};
use engram_harness_sdk::{Channels, ConnectionConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Notify};

pub const HARNESS_VERSION: &str = "engram-harness-noop/0.1.0";

#[derive(Clone, Debug)]
pub struct Script {
    pub autorun: bool,
    pub tool_calls: u32,
    pub tool_sleep: Duration,
    pub interval: Duration,
    pub result_summary: String,
    pub agent_message: Option<String>,
    pub send_run_completed: bool,
    pub plan_flow: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            autorun: true,
            tool_calls: 3,
            tool_sleep: Duration::from_millis(10),
            interval: Duration::from_millis(50),
            result_summary: "ok (noop)".into(),
            agent_message: Some("noop assistant message".into()),
            send_run_completed: false,
            plan_flow: false,
        }
    }
}

async fn send_event(tx: &mpsc::Sender<HarnessEvent>, event: HarnessEvent) -> std::io::Result<()> {
    tx.send(event)
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
}

async fn run_turn(
    script: &Script,
    run_id: String,
    prompt_id: Option<String>,
    tx: &mpsc::Sender<HarnessEvent>,
) -> std::io::Result<()> {
    send_event(
        tx,
        HarnessEvent::RunStarted {
            run_id: run_id.clone(),
            prompt_summary: Some("noop run".into()),
            prompt_id,
        },
    )
    .await?;
    for i in 0..script.tool_calls {
        let tool_call_id = format!("{run_id}-{i}");
        send_event(
            tx,
            HarnessEvent::ToolCallStarted {
                run_id: run_id.clone(),
                tool_call_id: tool_call_id.clone(),
                tool_name: "Noop".into(),
                args_summary: None,
            },
        )
        .await?;
        // The engine owns this sleep. A transport reconnect cannot cancel it.
        tokio::time::sleep(script.tool_sleep).await;
        send_event(
            tx,
            HarnessEvent::ToolCallCompleted {
                run_id: run_id.clone(),
                tool_call_id,
                tool_name: "Noop".into(),
                ok: true,
                duration_ms: u64::try_from(script.tool_sleep.as_millis()).unwrap_or(u64::MAX),
                result_summary: Some(script.result_summary.clone()),
            },
        )
        .await?;
        if let Some(text) = &script.agent_message {
            send_event(
                tx,
                HarnessEvent::AgentMessage {
                    run_id: run_id.clone(),
                    message_id: format!("{run_id}-msg-{i}"),
                    role: engram_harness_proto::AgentRole::Assistant,
                    text: text.clone(),
                },
            )
            .await?;
        }
        if i + 1 < script.tool_calls {
            tokio::time::sleep(script.interval).await;
        }
    }
    if script.send_run_completed {
        send_event(tx, HarnessEvent::RunCompleted { run_id, ok: true }).await?;
    }
    send_event(tx, HarnessEvent::Idle).await
}

pub async fn run_engine(
    script: Script,
    mut command_rx: mpsc::Receiver<HarnessCommand>,
    reattach: Arc<Notify>,
    event_tx: mpsc::Sender<HarnessEvent>,
) -> ExitCode {
    let mut pending = std::collections::VecDeque::new();
    let mut autorun = script.autorun;
    let mut plan_seq = 0;
    let mut awaiting_plan_call = None;
    loop {
        let command = if autorun {
            autorun = false;
            None
        } else if let Some(command) = pending.pop_front() {
            Some(command)
        } else {
            tokio::select! {
                command = command_rx.recv() => match command {
                    Some(command) => Some(command),
                    None => return ExitCode::SUCCESS,
                },
                _ = reattach.notified() => {
                    let state = if awaiting_plan_call.is_some() { HarnessEvent::Parked } else { HarnessEvent::Idle };
                    if send_event(&event_tx, state).await.is_err() { return ExitCode::FAILURE; }
                    continue;
                }
            }
        };
        let prompt_id = match command {
            Some(HarnessCommand::Shutdown { .. }) => return ExitCode::SUCCESS,
            Some(command) if script.plan_flow => {
                if plan_flow_step(&event_tx, command, &mut plan_seq, &mut awaiting_plan_call)
                    .await
                    .is_err()
                {
                    return ExitCode::FAILURE;
                }
                continue;
            }
            Some(HarnessCommand::Prompt { prompt_id, .. }) => Some(prompt_id),
            Some(_) => continue,
            None => None,
        };
        let run_id = uuid::Uuid::new_v4().to_string();
        let mut turn_script = script.clone();
        turn_script.send_run_completed |= prompt_id.is_some();
        let turn = run_turn(&turn_script, run_id.clone(), prompt_id, &event_tx);
        tokio::pin!(turn);
        loop {
            tokio::select! {
                biased;
                result = &mut turn => {
                    if result.is_err() { return ExitCode::FAILURE; }
                    break;
                }
                command = command_rx.recv() => match command {
                    None | Some(HarnessCommand::Shutdown { .. }) => {
                        let _ = send_event(&event_tx, HarnessEvent::RunInterrupted { run_id }).await;
                        return ExitCode::SUCCESS;
                    }
                    Some(HarnessCommand::Interrupt) => {
                        let _ = send_event(&event_tx, HarnessEvent::RunInterrupted { run_id }).await;
                        let _ = send_event(&event_tx, HarnessEvent::Idle).await;
                        break;
                    }
                    Some(command) => pending.push_back(command),
                },
                _ = reattach.notified() => {},
            }
        }
    }
}

/// Run the production connection loop against one in-memory connection.
/// After a disconnect it keeps retrying; tests must send Shutdown or abort it.
pub async fn serve_duplex<S>(
    script: Script,
    session_id: SessionId,
    token: AttachToken,
    stream: S,
) -> ExitCode
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let channels = Channels::new();
    let engine = tokio::spawn(run_engine(
        script,
        channels.command_rx,
        channels.reattach.clone(),
        channels.event_tx,
    ));
    let mut stream = Some(stream);
    engram_harness_sdk::serve_with_token(
        ConnectionConfig {
            connect: None,
            port: None,
            session_id,
            harness_version: HARNESS_VERSION.into(),
        },
        move || {
            let stream = stream.take();
            Box::pin(async move {
                stream.map(|stream| {
                    let (r, w) = tokio::io::split(stream);
                    (
                        Box::new(r) as engram_harness_sdk::BoxedReader,
                        Box::new(w) as engram_harness_sdk::BoxedWriter,
                    )
                })
            })
        },
        token,
        engine,
        channels.command_tx,
        channels.event_rx,
        channels.reattach,
    )
    .await
}

async fn plan_flow_step(
    writer: &mpsc::Sender<HarnessEvent>,
    cmd: HarnessCommand,
    plan_seq: &mut u32,
    awaiting_plan_call: &mut Option<String>,
) -> std::io::Result<()> {
    use engram_harness_proto::AgentRole;
    match cmd {
        HarnessCommand::Prompt {
            prompt_id, mode, ..
        } => {
            let run_id = format!("noop-plan-run-{}", *plan_seq);
            send_event(
                writer,
                HarnessEvent::RunStarted {
                    run_id: run_id.clone(),
                    prompt_summary: None,
                    prompt_id: Some(prompt_id),
                },
            )
            .await?;
            if mode.as_deref() == Some("plan") || awaiting_plan_call.is_some() {
                let call_id = format!("noop-plan-{}", *plan_seq);
                *plan_seq += 1;
                *awaiting_plan_call = Some(call_id.clone());
                send_event(
                    writer,
                    HarnessEvent::ToolCallRequested {
                        run_id: run_id.clone(),
                        call_id,
                        name: "exit_plan_mode".into(),
                        args_json: r##"{"plan":"# Noop plan\n\n1. Do the thing."}"##.into(),
                    },
                )
                .await?;
                send_event(writer, HarnessEvent::RunCompleted { run_id, ok: true }).await?;
                send_event(writer, HarnessEvent::Parked).await?;
            } else {
                send_event(
                    writer,
                    HarnessEvent::AgentMessage {
                        run_id: run_id.clone(),
                        message_id: format!("noop-echo-{run_id}"),
                        role: AgentRole::Assistant,
                        text: "noop turn".into(),
                    },
                )
                .await?;
                send_event(writer, HarnessEvent::RunCompleted { run_id, ok: true }).await?;
                send_event(writer, HarnessEvent::Idle).await?;
            }
        }
        HarnessCommand::ToolResult {
            call_id,
            result_json,
        } => {
            if awaiting_plan_call.as_deref() != Some(call_id.as_str()) {
                return Ok(());
            }
            let approved = engram_harness_sdk::plan::parse_plan_decision(&result_json)
                .map(|decision| decision.approved())
                .unwrap_or(false);
            send_event(
                writer,
                HarnessEvent::ToolCallCompleted {
                    run_id: String::new(),
                    tool_call_id: call_id,
                    tool_name: "exit_plan_mode".into(),
                    ok: approved,
                    duration_ms: 0,
                    result_summary: Some(if approved { "approved" } else { "rejected" }.into()),
                },
            )
            .await?;
            *awaiting_plan_call = None;
            if approved {
                let run_id = format!("noop-build-run-{}", *plan_seq);
                send_event(
                    writer,
                    HarnessEvent::RunStarted {
                        run_id: run_id.clone(),
                        prompt_summary: None,
                        prompt_id: None,
                    },
                )
                .await?;
                send_event(
                    writer,
                    HarnessEvent::AgentMessage {
                        run_id: run_id.clone(),
                        message_id: format!("noop-build-{run_id}"),
                        role: AgentRole::Assistant,
                        text: "implementing the approved plan (noop)".into(),
                    },
                )
                .await?;
                send_event(writer, HarnessEvent::RunCompleted { run_id, ok: true }).await?;
                send_event(writer, HarnessEvent::Idle).await?;
            } else {
                // Rejected: a fresh plan park under a new call id (the
                // revision cycle).
                let run_id = format!("noop-plan-run-{}", *plan_seq);
                let call_id = format!("noop-plan-{}", *plan_seq);
                *plan_seq += 1;
                *awaiting_plan_call = Some(call_id.clone());
                send_event(
                    writer,
                    HarnessEvent::RunStarted {
                        run_id: run_id.clone(),
                        prompt_summary: None,
                        prompt_id: None,
                    },
                )
                .await?;
                send_event(writer, HarnessEvent::ToolCallRequested {
                        run_id: run_id.clone(),
                        call_id,
                        name: "exit_plan_mode".into(),
                        args_json: r##"{"plan":"# Noop plan (revised)\n\n1. Do the thing.\n2. Add tests."}"##.into(),
                    })
                .await?;
                send_event(writer, HarnessEvent::RunCompleted { run_id, ok: true }).await?;
                send_event(writer, HarnessEvent::Parked).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn prompt_run_keeps_its_tool_sleep_across_reattach() {
        let channels = Channels::new();
        let mut events = channels.event_rx;
        let engine = tokio::spawn(run_engine(
            Script {
                autorun: false,
                tool_calls: 1,
                tool_sleep: Duration::from_secs(10),
                agent_message: None,
                ..Script::default()
            },
            channels.command_rx,
            channels.reattach.clone(),
            channels.event_tx,
        ));
        channels
            .command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "p1".into(),
                text: "work".into(),
                mode: None,
            })
            .await
            .unwrap();
        let run_id = match events.recv().await.unwrap() {
            HarnessEvent::RunStarted {
                run_id, prompt_id, ..
            } => {
                assert_eq!(prompt_id.as_deref(), Some("p1"));
                run_id
            }
            other => panic!("expected run start: {other:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::ToolCallStarted { .. })
        ));
        tokio::time::advance(Duration::from_secs(4)).await;
        channels.reattach.notify_one();
        tokio::task::yield_now().await;
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(
            matches!(events.recv().await, Some(HarnessEvent::ToolCallCompleted { run_id: id, .. }) if id == run_id)
        );
        assert!(
            matches!(events.recv().await, Some(HarnessEvent::RunCompleted { run_id: id, ok: true }) if id == run_id)
        );
        assert_eq!(events.recv().await, Some(HarnessEvent::Idle));
        channels
            .command_tx
            .send(HarnessCommand::Shutdown { grace_secs: 0 })
            .await
            .unwrap();
        assert_eq!(engine.await.unwrap(), ExitCode::SUCCESS);
    }

    #[tokio::test(start_paused = true)]
    async fn noop_short_circuits_on_shutdown_command() {
        let channels = Channels::new();
        let mut events = channels.event_rx;
        let engine = tokio::spawn(run_engine(
            Script {
                tool_sleep: Duration::from_secs(100),
                ..Script::default()
            },
            channels.command_rx,
            channels.reattach,
            channels.event_tx,
        ));
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::RunStarted { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::ToolCallStarted { .. })
        ));
        channels
            .command_tx
            .send(HarnessCommand::Shutdown { grace_secs: 0 })
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::RunInterrupted { .. })
        ));
        assert_eq!(engine.await.unwrap(), ExitCode::SUCCESS);
        assert_eq!(events.recv().await, None);
    }

    async fn until_parked(events: &mut mpsc::Receiver<HarnessEvent>) -> String {
        let mut call = None;
        while let Some(event) = events.recv().await {
            match event {
                HarnessEvent::ToolCallRequested { call_id, name, .. } => {
                    assert_eq!(name, "exit_plan_mode");
                    call = Some(call_id);
                }
                HarnessEvent::Parked => return call.unwrap(),
                _ => {}
            }
        }
        panic!("engine closed before park");
    }

    #[tokio::test]
    async fn plan_flow_parks_revises_on_reject_and_builds_on_approve() {
        let channels = Channels::new();
        let mut events = channels.event_rx;
        let engine = tokio::spawn(run_engine(
            Script {
                autorun: false,
                plan_flow: true,
                ..Script::default()
            },
            channels.command_rx,
            channels.reattach,
            channels.event_tx,
        ));
        channels
            .command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "plan".into(),
                text: "plan".into(),
                mode: Some("plan".into()),
            })
            .await
            .unwrap();
        let first = until_parked(&mut events).await;
        channels
            .command_tx
            .send(HarnessCommand::ToolResult {
                call_id: first.clone(),
                result_json: r#"{"decision":"reject","feedback":"add tests"}"#.into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::ToolCallCompleted { ok: false, .. })
        ));
        let second = until_parked(&mut events).await;
        assert_ne!(first, second);
        channels
            .command_tx
            .send(HarnessCommand::ToolResult {
                call_id: second,
                result_json: r#"{"decision":"approve"}"#.into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::ToolCallCompleted { ok: true, .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::RunStarted { .. })
        ));
        assert!(
            matches!(events.recv().await, Some(HarnessEvent::AgentMessage { text, .. }) if text.contains("approved plan"))
        );
        assert!(matches!(
            events.recv().await,
            Some(HarnessEvent::RunCompleted { ok: true, .. })
        ));
        assert_eq!(events.recv().await, Some(HarnessEvent::Idle));
        drop(channels.command_tx);
        assert_eq!(engine.await.unwrap(), ExitCode::SUCCESS);
    }
}
