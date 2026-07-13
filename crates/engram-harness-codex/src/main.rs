use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;

use clap::Parser;
use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    AgentRole, FileChange, HarnessCommand, HarnessEvent, Question, QuestionOption,
};
use engram_harness_sdk::{emit, Channels, ConnectionConfig, QueuedPrompt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Notify};

const DEFAULT_CODEX_HOME: &str = "/workspace/.engram/codex";
const THREAD_ID_FILE: &str = "/workspace/.engram/codex-thread-id";
const MAX_SUMMARY: usize = 4096;

#[derive(Parser, Clone, Debug)]
#[command(name = "engram-harness-codex")]
struct Cli {
    #[arg(long, env = "ENGRAM_HARNESS_ADDR", conflicts_with = "port")]
    connect: Option<String>,
    #[arg(long = "port", alias = "vsock-host", env = "ENGRAM_HARNESS_VSOCK_HOST")]
    port: Option<u32>,
    #[arg(long, env = "ENGRAM_SESSION_ID")]
    session_id: SessionId,
    #[arg(long, env = "ENGRAM_SANDBOX_ID")]
    sandbox_id: SandboxId,
    #[arg(long, env = "ENGRAM_BINDING_EPOCH")]
    binding_epoch: u64,
    #[arg(long, env = "ENGRAM_CODEX_BIN")]
    codex_bin: Option<PathBuf>,
    #[arg(long, env = "ENGRAM_CODEX_HOME", default_value = DEFAULT_CODEX_HOME)]
    codex_home: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    if !((cli.connect.is_some()) ^ (cli.port.is_some())) {
        tracing::error!("provide exactly one of --connect or --port");
        return ExitCode::from(2);
    }
    let Channels {
        command_tx,
        command_rx,
        event_tx,
        event_rx,
        reattach,
    } = Channels::new();
    let engine_cli = cli.clone();
    let engine_reattach = reattach.clone();
    let engine =
        tokio::spawn(
            async move { run_engine(engine_cli, command_rx, engine_reattach, event_tx).await },
        );
    engram_harness_sdk::serve(
        ConnectionConfig {
            connect: cli.connect,
            port: cli.port,
            session_id: cli.session_id,
            sandbox_id: cli.sandbox_id,
            binding_epoch: cli.binding_epoch,
            harness_version: format!("engram-harness-codex/{}", env!("CARGO_PKG_VERSION")),
        },
        engine,
        command_tx,
        event_rx,
        reattach,
    )
    .await
}

struct AppServer {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: i64,
    thread_id: String,
    persisted_prompts: HashMap<String, PersistedTurn>,
    buffered: VecDeque<Value>,
    stderr_task: Option<tokio::task::JoinHandle<Vec<String>>>,
}

#[derive(Clone, Debug)]
struct PersistedTurn {
    turn_id: String,
    status: String,
    steered: bool,
}

#[derive(Debug)]
enum Pending {
    Start(QueuedPrompt),
    Steer(QueuedPrompt),
    Interrupt,
}

struct OutstandingQuestion {
    request_id: Value,
    ids_by_text: HashMap<String, String>,
}

async fn run_engine(
    cli: Cli,
    mut commands: mpsc::Receiver<HarnessCommand>,
    reattach: Arc<Notify>,
    events: mpsc::Sender<HarnessEvent>,
) -> ExitCode {
    let mut queued = VecDeque::<QueuedPrompt>::new();
    let mut seen = HashSet::<String>::new();
    let mut failures = 0u32;
    loop {
        match AppServer::spawn(&cli).await {
            Ok(mut server) => {
                failures = 0;
                seen.extend(server.persisted_prompts.keys().cloned());
                let outcome = drive(
                    &mut server,
                    &mut commands,
                    &reattach,
                    &events,
                    &mut queued,
                    &mut seen,
                )
                .await;
                if matches!(
                    outcome,
                    DriveOutcome::Shutdown | DriveOutcome::ChannelClosed
                ) {
                    let _ = server.stdin.shutdown().await;
                    if tokio::time::timeout(std::time::Duration::from_secs(5), server.child.wait())
                        .await
                        .is_err()
                    {
                        let _ = server.child.start_kill();
                        let _ = server.child.wait().await;
                    }
                    return ExitCode::SUCCESS;
                }
                let _ = server.child.start_kill();
                let _ = server.child.wait().await;
            }
            Err(error) => tracing::error!(%error, "failed to start Codex app-server"),
        }
        failures += 1;
        if failures.is_power_of_two() {
            tracing::warn!(
                failures,
                "Codex app-server unavailable; retrying indefinitely"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(1u64 << failures.min(3))).await;
    }
}

impl AppServer {
    async fn spawn(cli: &Cli) -> Result<Self, String> {
        tokio::fs::create_dir_all(&cli.codex_home)
            .await
            .map_err(|e| format!("create CODEX_HOME: {e}"))?;
        ensure_skills_link(&cli.codex_home).await;
        let bin = cli.codex_bin.clone().unwrap_or_else(resolve_codex_bin);
        let mut command = Command::new(&bin);
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", &cli.codex_home)
            .env("CODEX_NON_INTERACTIVE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| format!("spawn {bin:?}: {e}"))?;
        let stdin = child.stdin.take().ok_or("app-server stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("app-server stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("app-server stderr unavailable")?;
        let mut server = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            thread_id: String::new(),
            persisted_prompts: HashMap::new(),
            buffered: VecDeque::new(),
            stderr_task: Some(engram_harness_sdk::spawn_stderr_tail(stderr, 64, true)),
        };
        server
            .request_wait(
                "initialize",
                json!({"clientInfo":{"name":"engrams","title":"Engrams","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false}}),
            )
            .await?;
        server.notify("initialized", json!({})).await?;
        if let Ok(api_key) = std::env::var("CODEX_API_KEY") {
            server
                .request_wait(
                    "account/login/start",
                    json!({"type":"apiKey","apiKey":api_key}),
                )
                .await?;
        }
        // Codex persists API-key login material to CODEX_HOME/auth.json. The
        // app-server keeps the selected credential in memory, so remove the
        // file before any model-controlled command can inspect the workspace.
        // A fresh wrapper re-authenticates from the selected Engram env var.
        match tokio::fs::remove_file(cli.codex_home.join("auth.json")).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(%error, "could not remove Codex credential cache"),
        }
        let prior = tokio::fs::read_to_string(THREAD_ID_FILE)
            .await
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let method = if prior.is_some() {
            "thread/resume"
        } else {
            "thread/start"
        };
        let mut params = thread_params();
        if let Some(thread_id) = prior {
            params["threadId"] = json!(thread_id);
        }
        let response = server.request_wait(method, params).await?;
        server.thread_id = response
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .ok_or("thread response missing result.thread.id")?
            .to_owned();
        server.persisted_prompts = persisted_prompts(&response);
        persist_thread_id(&server.thread_id).await?;
        Ok(server)
    }

    async fn send_request(&mut self, method: &str, params: Value) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    async fn request_wait(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.send_request(method, params).await?;
        loop {
            let value = self.read().await?.ok_or("app-server EOF")?;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(format!("{method}: {error}"));
                }
                return Ok(value);
            }
            self.buffered.push_back(value);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write(json!({"method":method,"params":params})).await
    }

    async fn respond(&mut self, id: Value, result: Value) -> Result<(), String> {
        self.write(json!({"id":id,"result":result})).await
    }

    async fn write(&mut self, value: Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|e| e.to_string())?;
        self.stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn read(&mut self) -> Result<Option<Value>, String> {
        match self.lines.next_line().await.map_err(|e| e.to_string())? {
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| format!("bad app-server JSON: {e}")),
            None => Ok(None),
        }
    }
}

enum DriveOutcome {
    Shutdown,
    ChannelClosed,
    Crashed,
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    server: &mut AppServer,
    commands: &mut mpsc::Receiver<HarnessCommand>,
    reattach: &Notify,
    events: &mpsc::Sender<HarnessEvent>,
    queued: &mut VecDeque<QueuedPrompt>,
    seen: &mut HashSet<String>,
) -> DriveOutcome {
    let mut active: Option<String> = None;
    let mut pending = HashMap::<i64, Pending>::new();
    let mut questions = HashMap::<String, OutstandingQuestion>::new();
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupt_requested = false;
    emit(events, HarnessEvent::Idle).await;
    let mut index = 0;
    while index < queued.len() {
        let prompt_id = queued[index].prompt_id.clone();
        let Some(turn) = server.persisted_prompts.get(&prompt_id) else {
            index += 1;
            continue;
        };
        queued.remove(index);
        if turn.steered {
            emit(events, HarnessEvent::PromptSteered { prompt_id }).await;
            continue;
        }
        emit(
            events,
            HarnessEvent::RunStarted {
                run_id: turn.turn_id.clone(),
                prompt_summary: None,
                prompt_id: Some(prompt_id),
            },
        )
        .await;
        if turn.status == "inProgress" {
            active = Some(turn.turn_id.clone());
        }
        match turn.status.as_str() {
            "interrupted" => {
                emit(
                    events,
                    HarnessEvent::RunInterrupted {
                        run_id: turn.turn_id.clone(),
                    },
                )
                .await
            }
            "completed" | "failed" => {
                emit(
                    events,
                    HarnessEvent::RunCompleted {
                        run_id: turn.turn_id.clone(),
                        ok: turn.status == "completed",
                    },
                )
                .await
            }
            _ => {}
        }
    }
    if active.is_none() {
        if let Some(prompt) = queued.pop_front() {
            match start_turn(server, &prompt).await {
                Ok(id) => {
                    pending.insert(id, Pending::Start(prompt));
                }
                Err(_) => queued.push_front(prompt),
            }
        }
    }
    loop {
        if let Some(value) = server.buffered.pop_front() {
            let completed = value.get("method").and_then(Value::as_str) == Some("turn/completed");
            handle_message(
                server,
                value,
                events,
                &mut active,
                &mut pending,
                queued,
                &mut questions,
            )
            .await;
            if completed {
                interrupt_deadline = None;
                interrupt_requested = false;
            }
            continue;
        }
        tokio::select! {
            command = commands.recv() => match command {
                Some(HarnessCommand::Prompt { prompt_id, text }) => {
                    if !seen.insert(prompt_id.clone()) {
                        if let Some(turn) = server.persisted_prompts.get(&prompt_id) {
                            if turn.steered {
                                emit(events, HarnessEvent::PromptSteered { prompt_id }).await;
                                continue;
                            }
                            emit(events, HarnessEvent::RunStarted {
                                run_id: turn.turn_id.clone(),
                                prompt_summary: None,
                                prompt_id: Some(prompt_id),
                            }).await;
                            match turn.status.as_str() {
                                "interrupted" => emit(events, HarnessEvent::RunInterrupted { run_id: turn.turn_id.clone() }).await,
                                "completed" | "failed" => emit(events, HarnessEvent::RunCompleted { run_id: turn.turn_id.clone(), ok: turn.status == "completed" }).await,
                                _ => {}
                            }
                            if turn.status != "inProgress" {
                                emit(events, HarnessEvent::Idle).await;
                            }
                        }
                        continue;
                    }
                    let prompt = QueuedPrompt { prompt_id, text };
                    if let Some(turn_id) = active.as_deref() {
                        let id = server.send_request("turn/steer", json!({
                            "threadId": server.thread_id,
                            "expectedTurnId": turn_id,
                            "clientUserMessageId": prompt.prompt_id,
                            "input":[{"type":"text","text":prompt.text}],
                        })).await;
                        match id {
                            Ok(id) => { pending.insert(id, Pending::Steer(prompt)); },
                            Err(_) => {
                                emit(events, HarnessEvent::PromptQueued { prompt_id: prompt.prompt_id.clone(), summary: Some(engram_harness_sdk::truncate_utf8(&prompt.text, 1024)) }).await;
                                queued.push_back(prompt);
                            }
                        }
                    } else {
                        match start_turn(server, &prompt).await {
                            Ok(id) => { pending.insert(id, Pending::Start(prompt)); },
                            Err(_) => {
                                emit(events, HarnessEvent::PromptQueued { prompt_id: prompt.prompt_id.clone(), summary: Some(engram_harness_sdk::truncate_utf8(&prompt.text, 1024)) }).await;
                                queued.push_back(prompt);
                            }
                        }
                    }
                }
                Some(HarnessCommand::EditQueued { prompt_id, text }) => {
                    if let Some(q) = queued.iter_mut().find(|q| q.prompt_id == prompt_id) {
                        q.text = text;
                        emit(events, HarnessEvent::PromptEdited { prompt_id, summary: Some(engram_harness_sdk::truncate_utf8(&q.text, 1024)) }).await;
                    }
                }
                Some(HarnessCommand::DequeueQueued { prompt_id }) => {
                    if let Some(i) = queued.iter().position(|q| q.prompt_id == prompt_id) {
                        queued.remove(i);
                        emit(events, HarnessEvent::PromptDequeued { prompt_id }).await;
                    }
                }
                Some(HarnessCommand::Interrupt) => if let Some(turn_id) = active.as_deref() {
                    if let Ok(id) = server.send_request("turn/interrupt", json!({"threadId":server.thread_id,"turnId":turn_id})).await {
                        pending.insert(id, Pending::Interrupt);
                        interrupt_requested = true;
                        interrupt_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                    }
                },
                Some(HarnessCommand::AnswerQuestion { tool_call_id, answers }) => {
                    if let Some(outstanding) = questions.remove(&tool_call_id) {
                        let codex_answers: serde_json::Map<String, Value> = answers.iter().map(|(text,v)| {
                            let id = outstanding.ids_by_text.get(text).cloned().unwrap_or_else(|| text.clone());
                            (id, json!({"answers":v}))
                        }).collect();
                        let _ = server.respond(outstanding.request_id, json!({"answers":codex_answers})).await;
                        if let Some(run_id) = active.clone() {
                            emit(events, HarnessEvent::QuestionAnswered { run_id, tool_call_id, answers }).await;
                        }
                    }
                }
                Some(HarnessCommand::ToolResult { call_id, .. }) => {
                    // ADR 0089 P1: the wire exists but this harness declares
                    // no dynamic tools yet (P3). Surface loudly, never drop.
                    tracing::warn!(%call_id, "ToolResult before ADR 0089 P3: codex harness has no generic tools yet");
                }
                Some(HarnessCommand::Shutdown { .. }) => return DriveOutcome::Shutdown,
                Some(HarnessCommand::Checkpoint { .. }) => {}
                None => return DriveOutcome::ChannelClosed,
            },
            _ = reattach.notified() => if active.is_none() { emit(events, HarnessEvent::Idle).await; },
            _ = async {
                match interrupt_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::warn!("Codex did not confirm interrupt within 10s; terminating app-server");
                let _ = server.child.start_kill();
                interrupt_deadline = None;
            },
            message = server.read() => match message {
                Ok(Some(value)) => {
                    let completed = value.get("method").and_then(Value::as_str) == Some("turn/completed");
                    handle_message(server, value, events, &mut active, &mut pending, queued, &mut questions).await;
                    if completed {
                        interrupt_deadline = None;
                        interrupt_requested = false;
                    }
                },
                Ok(None) | Err(_) => {
                    for request in pending.drain().map(|(_, request)| request) {
                        if let Pending::Start(prompt) | Pending::Steer(prompt) = request {
                            queued.push_back(prompt);
                        }
                    }
                    if let Some(run_id) = active.take() {
                        let _ = server.child.start_kill();
                        let _ = server.child.wait().await;
                        let tail = match server.stderr_task.take() {
                            Some(task) => tokio::time::timeout(std::time::Duration::from_secs(2), task)
                                .await
                                .ok()
                                .and_then(Result::ok)
                                .unwrap_or_default(),
                            None => Vec::new(),
                        };
                        let detail = if tail.is_empty() {
                            "Codex app-server exited unexpectedly; resuming the persisted thread.".to_string()
                        } else {
                            format!("Codex app-server exited unexpectedly; resuming the persisted thread. stderr tail:\n{}", engram_harness_sdk::truncate_utf8(&tail.join("\n"), MAX_SUMMARY))
                        };
                        emit(events, HarnessEvent::AgentMessage { run_id: run_id.clone(), message_id: format!("abnormal-{}", uuid::Uuid::new_v4()), role: AgentRole::System, text: detail }).await;
                        if interrupt_requested {
                            emit(events, HarnessEvent::RunInterrupted { run_id }).await;
                        } else {
                            emit(events, HarnessEvent::RunCompleted { run_id, ok: false }).await;
                        }
                        emit(events, HarnessEvent::Idle).await;
                    }
                    return DriveOutcome::Crashed;
                }
            }
        }
    }
}

async fn start_turn(server: &mut AppServer, prompt: &QueuedPrompt) -> Result<i64, String> {
    let mut params = json!({
        "threadId":server.thread_id,
        "clientUserMessageId":prompt.prompt_id,
        "input":[{"type":"text","text":prompt.text}],
        "approvalPolicy":"never",
        "sandboxPolicy":{"type":"externalSandbox","networkAccess":"enabled"},
    });
    if let Ok(effort) = std::env::var("ENGRAM_CODEX_EFFORT") {
        params["effort"] = json!(effort);
    }
    server.send_request("turn/start", params).await
}

async fn handle_message(
    server: &mut AppServer,
    value: Value,
    events: &mpsc::Sender<HarnessEvent>,
    active: &mut Option<String>,
    pending: &mut HashMap<i64, Pending>,
    queued: &mut VecDeque<QueuedPrompt>,
    questions: &mut HashMap<String, OutstandingQuestion>,
) {
    if value.get("method").and_then(Value::as_str) == Some("item/tool/requestUserInput") {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let run_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .or(active.as_deref())
            .unwrap_or("")
            .to_owned();
        let item_id = params
            .get("itemId")
            .and_then(Value::as_str)
            .unwrap_or("request-user-input")
            .to_owned();
        let qs = parse_questions(params.get("questions"));
        let ids_by_text = params
            .get("questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|q| {
                Some((
                    q.get("question")?.as_str()?.to_owned(),
                    q.get("id")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        if let Some(request_id) = value.get("id").cloned() {
            questions.insert(
                item_id.clone(),
                OutstandingQuestion {
                    request_id,
                    ids_by_text,
                },
            );
        }
        emit(
            events,
            HarnessEvent::UserQuestion {
                run_id,
                tool_call_id: item_id,
                questions: qs,
            },
        )
        .await;
        return;
    }
    if let Some(id) = value.get("id").and_then(Value::as_i64) {
        if let Some(kind) = pending.remove(&id) {
            match kind {
                Pending::Start(prompt) if value.get("error").is_none() => {
                    if let Some(turn_id) = value.pointer("/result/turn/id").and_then(Value::as_str)
                    {
                        *active = Some(turn_id.to_owned());
                        emit(
                            events,
                            HarnessEvent::RunStarted {
                                run_id: turn_id.to_owned(),
                                prompt_summary: None,
                                prompt_id: Some(prompt.prompt_id),
                            },
                        )
                        .await;
                    }
                }
                Pending::Steer(prompt) if value.get("error").is_none() => {
                    emit(
                        events,
                        HarnessEvent::PromptSteered {
                            prompt_id: prompt.prompt_id,
                        },
                    )
                    .await;
                }
                Pending::Steer(prompt) | Pending::Start(prompt) => {
                    emit(
                        events,
                        HarnessEvent::PromptQueued {
                            prompt_id: prompt.prompt_id.clone(),
                            summary: Some(engram_harness_sdk::truncate_utf8(&prompt.text, 1024)),
                        },
                    )
                    .await;
                    queued.push_back(prompt);
                }
                Pending::Interrupt => {}
            }
        }
        return;
    }
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let run_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .or(active.as_deref())
        .unwrap_or("")
        .to_owned();
    match method {
        "item/agentMessage/delta" => {
            if let (Some(item_id), Some(delta)) = (
                params.get("itemId").and_then(Value::as_str),
                params.get("delta").and_then(Value::as_str),
            ) {
                emit(
                    events,
                    HarnessEvent::AgentMessageChunk {
                        run_id,
                        message_id: item_id.into(),
                        chunk: engram_harness_sdk::truncate_utf8(delta, 6 * 1024),
                    },
                )
                .await;
            }
        }
        "item/started" => {
            if let Some(item) = params.get("item") {
                emit_item_started(events, &run_id, item).await;
            }
        }
        "item/completed" => {
            if let Some(item) = params.get("item") {
                emit_item_completed(events, &run_id, item).await;
            }
        }
        "turn/completed" => {
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("failed");
            let completed = params
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .unwrap_or(&run_id)
                .to_owned();
            if status == "interrupted" {
                emit(events, HarnessEvent::RunInterrupted { run_id: completed }).await;
            } else {
                emit(
                    events,
                    HarnessEvent::RunCompleted {
                        run_id: completed,
                        ok: status == "completed",
                    },
                )
                .await;
            }
            *active = None;
            emit(events, HarnessEvent::Idle).await;
            if let Some(prompt) = queued.pop_front() {
                match start_turn(server, &prompt).await {
                    Ok(id) => {
                        pending.insert(id, Pending::Start(prompt));
                    }
                    Err(_) => {
                        queued.push_front(prompt);
                    }
                }
            }
        }
        "thread/name/updated" => {
            if let Some(title) = params.get("threadName").and_then(Value::as_str) {
                emit(
                    events,
                    HarnessEvent::TitleSuggested {
                        title: engram_harness_sdk::truncate_utf8(title, 512),
                    },
                )
                .await;
            }
        }
        _ => {}
    }
}

async fn emit_item_started(events: &mpsc::Sender<HarnessEvent>, run_id: &str, item: &Value) {
    let id = item.get("id").and_then(Value::as_str).unwrap_or("item");
    match item.get("type").and_then(Value::as_str) {
        Some("commandExecution") => {
            emit(
                events,
                HarnessEvent::ToolCallStarted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: "Shell".into(),
                    args_summary: item
                        .get("command")
                        .and_then(Value::as_str)
                        .map(|s| engram_harness_sdk::truncate_utf8(s, 1024)),
                },
            )
            .await
        }
        Some("fileChange") => {
            emit(
                events,
                HarnessEvent::ToolCallStarted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: "FileChange".into(),
                    args_summary: None,
                },
            )
            .await
        }
        Some("mcpToolCall") => {
            emit(
                events,
                HarnessEvent::ToolCallStarted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: item
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("MCP")
                        .into(),
                    args_summary: item
                        .get("arguments")
                        .map(|v| engram_harness_sdk::truncate_utf8(&v.to_string(), 1024)),
                },
            )
            .await
        }
        Some("dynamicToolCall") | Some("collabAgentToolCall") | Some("webSearch") => {
            let tool_name = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("WebSearch");
            let summary = item
                .get("arguments")
                .or_else(|| item.get("query"))
                .or_else(|| item.get("prompt"))
                .map(|value| {
                    let text = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    engram_harness_sdk::truncate_utf8(&text, 1024)
                });
            emit(
                events,
                HarnessEvent::ToolCallStarted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: tool_name.into(),
                    args_summary: summary,
                },
            )
            .await
        }
        _ => {}
    }
}

async fn emit_item_completed(events: &mpsc::Sender<HarnessEvent>, run_id: &str, item: &Value) {
    let id = item.get("id").and_then(Value::as_str).unwrap_or("item");
    match item.get("type").and_then(Value::as_str) {
        Some("agentMessage") => {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                emit(
                    events,
                    HarnessEvent::AgentMessage {
                        run_id: run_id.into(),
                        message_id: id.into(),
                        role: AgentRole::Assistant,
                        text: engram_harness_sdk::truncate_utf8(text, 64 * 1024),
                    },
                )
                .await;
            }
        }
        Some("commandExecution") => {
            emit(
                events,
                HarnessEvent::ToolCallCompleted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: "Shell".into(),
                    ok: item.get("status").and_then(Value::as_str) == Some("completed"),
                    duration_ms: item.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
                    result_summary: item
                        .get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .map(|s| engram_harness_sdk::truncate_utf8(s, MAX_SUMMARY)),
                },
            )
            .await
        }
        Some("fileChange") => {
            let ok = item.get("status").and_then(Value::as_str) == Some("completed");
            emit(
                events,
                HarnessEvent::ToolCallCompleted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: "FileChange".into(),
                    ok,
                    duration_ms: 0,
                    result_summary: None,
                },
            )
            .await;
            if ok {
                for change in item
                    .get("changes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let (Some(path), Some(diff)) = (
                        change.get("path").and_then(Value::as_str),
                        change.get("diff").and_then(Value::as_str),
                    ) {
                        emit(
                            events,
                            HarnessEvent::FileChanged {
                                run_id: run_id.into(),
                                tool_call_id: id.into(),
                                path: path.into(),
                                change: FileChange::Patch {
                                    unified_diff: engram_harness_sdk::truncate_utf8(
                                        diff,
                                        64 * 1024,
                                    ),
                                },
                            },
                        )
                        .await;
                    }
                }
            }
        }
        Some("mcpToolCall") => {
            emit(
                events,
                HarnessEvent::ToolCallCompleted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: item
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("MCP")
                        .into(),
                    ok: item.get("status").and_then(Value::as_str) == Some("completed"),
                    duration_ms: 0,
                    result_summary: item
                        .get("result")
                        .map(|v| engram_harness_sdk::truncate_utf8(&v.to_string(), MAX_SUMMARY)),
                },
            )
            .await
        }
        Some("dynamicToolCall") | Some("collabAgentToolCall") | Some("webSearch") => {
            let tool_name = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("WebSearch");
            let status = item.get("status").and_then(Value::as_str);
            emit(
                events,
                HarnessEvent::ToolCallCompleted {
                    run_id: run_id.into(),
                    tool_call_id: id.into(),
                    tool_name: tool_name.into(),
                    ok: item
                        .get("success")
                        .and_then(Value::as_bool)
                        .unwrap_or(status.is_none() || status == Some("completed")),
                    duration_ms: item.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
                    result_summary: item.get("result").or_else(|| item.get("action")).map(
                        |value| engram_harness_sdk::truncate_utf8(&value.to_string(), MAX_SUMMARY),
                    ),
                },
            )
            .await
        }
        _ => {}
    }
}

fn parse_questions(value: Option<&Value>) -> Vec<Question> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|q| Question {
            question: q
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            header: q
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("Question")
                .into(),
            multi_select: q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            options: q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|o| QuestionOption {
                    label: o
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                    description: o
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                })
                .collect(),
        })
        .collect()
}

/// Recover prompt acceptance from Codex's durable thread history. This closes
/// the crash window where app-server persisted a `turn/start` or `turn/steer`
/// before the wrapper could acknowledge the prompt to Engram.
fn persisted_prompts(response: &Value) -> HashMap<String, PersistedTurn> {
    let mut prompts = HashMap::new();
    for turn in response
        .pointer("/result/thread/turns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(turn_id) = turn.get("id").and_then(Value::as_str) else {
            continue;
        };
        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("failed");
        let mut client_message_index = 0usize;
        for item in turn
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if item.get("type").and_then(Value::as_str) != Some("userMessage") {
                continue;
            }
            if let Some(client_id) = item.get("clientId").and_then(Value::as_str) {
                prompts.insert(
                    client_id.to_owned(),
                    PersistedTurn {
                        turn_id: turn_id.to_owned(),
                        status: status.to_owned(),
                        steered: client_message_index > 0,
                    },
                );
            }
            client_message_index += 1;
        }
    }
    prompts
}

fn resolve_codex_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("codex")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("codex"))
}

fn thread_params() -> Value {
    let mut params = json!({
        "cwd": std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/workspace")),
        "approvalPolicy": "never",
        "sandbox": "danger-full-access",
        "config": {
            "shell_environment_policy": {
                "exclude": ["CODEX_ACCESS_TOKEN", "CODEX_API_KEY", "OPENAI_API_KEY", "CODEX_HOME"]
            }
        }
    });
    if let Ok(model) = std::env::var("ENGRAM_CODEX_MODEL") {
        params["model"] = json!(model);
    }
    if let Ok(instructions) = std::env::var("ENGRAM_APPEND_SYSTEM_PROMPT") {
        params["developerInstructions"] = json!(instructions);
    }
    params
}

async fn persist_thread_id(id: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(THREAD_ID_FILE).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(THREAD_ID_FILE, format!("{id}\n"))
        .await
        .map_err(|e| e.to_string())
}

async fn ensure_skills_link(home: &Path) {
    let link = home.join("skills");
    if tokio::fs::symlink_metadata(&link).await.is_ok() {
        return;
    }
    let _ = tokio::fs::symlink("/root/.agents/skills", link).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_user_input_shape() {
        let value = json!([{
            "id":"q1",
            "question":"Deploy now?",
            "header":"Deploy",
            "multiSelect":false,
            "options":[{"label":"Yes","description":"Deploy it"}]
        }]);
        let questions = parse_questions(Some(&value));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Deploy now?");
        assert_eq!(questions[0].options[0].label, "Yes");
    }

    #[test]
    fn resume_history_recovers_client_prompt_ids() {
        let recovered = persisted_prompts(&json!({"result":{"thread":{"turns":[{
            "id":"turn-1",
            "status":"completed",
            "items":[
                {"id":"user-1","type":"userMessage","clientId":"prompt-1","content":[]},
                {"id":"user-2","type":"userMessage","clientId":"prompt-steer","content":[]}
            ]
        }]}}}));
        let turn = recovered.get("prompt-1").unwrap();
        assert_eq!(turn.turn_id, "turn-1");
        assert_eq!(turn.status, "completed");
        assert!(!turn.steered);
        assert!(recovered.get("prompt-steer").unwrap().steered);
    }

    #[test]
    fn model_tool_environment_excludes_all_credentials() {
        let params = thread_params();
        let excluded = params
            .pointer("/config/shell_environment_policy/exclude")
            .and_then(Value::as_array)
            .unwrap();
        for name in [
            "CODEX_ACCESS_TOKEN",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "CODEX_HOME",
        ] {
            assert!(excluded.iter().any(|value| value == name));
        }
        assert_eq!(params.get("approvalPolicy"), Some(&json!("never")));
        assert_eq!(params.get("sandbox"), Some(&json!("danger-full-access")));
    }

    #[tokio::test]
    async fn codex_file_change_becomes_unified_patch_event() {
        let (tx, mut rx) = mpsc::channel(4);
        emit_item_completed(
            &tx,
            "turn-1",
            &json!({
                "id":"item-1",
                "type":"fileChange",
                "status":"completed",
                "changes":[{"path":"src/main.rs","diff":"@@ -1 +1 @@\n-old\n+new\n"}]
            }),
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(HarnessEvent::ToolCallCompleted { ok: true, .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(HarnessEvent::FileChanged {
                change: FileChange::Patch { .. },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn agent_delta_and_complete_share_message_id() {
        let (tx, mut rx) = mpsc::channel(4);
        emit_item_completed(
            &tx,
            "turn-1",
            &json!({"id":"msg-1","type":"agentMessage","text":"done"}),
        )
        .await;
        assert!(matches!(
            rx.recv().await,
            Some(HarnessEvent::AgentMessage { message_id, text, .. })
                if message_id == "msg-1" && text == "done"
        ));
    }
}
