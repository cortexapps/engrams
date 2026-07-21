use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;

use clap::Parser;
use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{AgentRole, FileChange, HarnessCommand, HarnessEvent};
use engram_harness_sdk::browser_view;
use engram_harness_sdk::parked::{ParkedCall, ParkedCallKind, ParkedCallStore};
use engram_harness_sdk::questions::{Answers, Question, QuestionOption};
use engram_harness_sdk::{emit, Channels, ConnectionConfig, QueuedPrompt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Notify};

const DEFAULT_CODEX_HOME: &str = "/workspace/.engrams/codex";
const THREAD_ID_FILE: &str = "/workspace/.engrams/codex-thread-id";
const PARKED_CALLS_FILE: &str = "/workspace/.engrams/codex-parked-calls.json";
const MAX_SUMMARY: usize = 4096;

type ToolManifest = Vec<ManifestTool>;

#[derive(Clone, Debug)]
struct ManifestTool {
    name: String,
    description: String,
    input_schema: Value,
    execution: ToolExecution,
    codex_native_binding: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolExecution {
    Sync,
    Deferred,
}

fn parse_tool_manifest(raw: &str) -> Result<ToolManifest, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let entries = value
        .as_array()
        .ok_or("ENGRAM_TOOLS must be a JSON array")?;
    entries
        .iter()
        .map(|entry| {
            let object = entry
                .as_object()
                .ok_or("ENGRAM_TOOLS entries must be objects")?;
            let required_string = |field: &str| {
                object
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("ENGRAM_TOOLS entry missing string {field}"))
            };
            let execution = match required_string("execution")?.as_str() {
                "sync" => ToolExecution::Sync,
                "deferred" => ToolExecution::Deferred,
                other => return Err(format!("invalid ENGRAM_TOOLS execution {other}")),
            };
            let codex_native_binding = match object.get("nativeBindings") {
                None => None,
                Some(Value::Object(bindings)) => match bindings.get("codex") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(binding)) => Some(binding.clone()),
                    Some(_) => {
                        return Err("ENGRAM_TOOLS nativeBindings.codex must be a string".to_string())
                    }
                },
                Some(_) => return Err("ENGRAM_TOOLS nativeBindings must be an object".to_string()),
            };
            Ok(ManifestTool {
                name: required_string("name")?,
                description: required_string("description")?,
                input_schema: object
                    .get("inputSchema")
                    .cloned()
                    .ok_or("ENGRAM_TOOLS entry missing inputSchema")?,
                execution,
                codex_native_binding,
            })
        })
        .collect()
}

fn manifest_from_env() -> ToolManifest {
    let manifest = match std::env::var("ENGRAM_TOOLS") {
        Ok(raw) if !raw.trim().is_empty() => match parse_tool_manifest(&raw) {
            Ok(manifest) => manifest,
            Err(error) => {
                tracing::error!(%error, "invalid ENGRAM_TOOLS manifest; exposing no dynamic tools");
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    with_browser_view(manifest, browser_view::enabled())
}

fn with_browser_view(mut manifest: ToolManifest, enabled: bool) -> ToolManifest {
    if enabled
        && !manifest
            .iter()
            .any(|tool| tool.name == browser_view::TOOL_NAME)
    {
        manifest.push(ManifestTool {
            name: browser_view::TOOL_NAME.into(),
            description: browser_view::TOOL_DESCRIPTION.into(),
            input_schema: browser_view::input_schema(),
            execution: ToolExecution::Sync,
            codex_native_binding: None,
        });
    }
    manifest
}

fn dynamic_tools(manifest: &ToolManifest) -> Value {
    Value::Array(
        manifest
            .iter()
            .filter(|tool| tool.codex_native_binding.is_none())
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema,
                })
            })
            .collect(),
    )
}

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
    /// Test seam for per-session state. Production uses THREAD_ID_FILE.
    #[arg(skip)]
    thread_id_file: Option<PathBuf>,
    /// Parsed once from ENGRAM_TOOLS by the harness entrypoint.
    #[arg(skip)]
    tool_manifest: ToolManifest,
    /// Test seam for the durable correlation table.
    #[arg(skip)]
    parked_calls_file: Option<PathBuf>,
}

impl Cli {
    fn thread_id_file(&self) -> &Path {
        self.thread_id_file
            .as_deref()
            .unwrap_or_else(|| Path::new(THREAD_ID_FILE))
    }

    fn parked_calls_file(&self) -> &Path {
        self.parked_calls_file
            .as_deref()
            .unwrap_or_else(|| Path::new(PARKED_CALLS_FILE))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let mut cli = Cli::parse();
    cli.tool_manifest = manifest_from_env();
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
    generation: u64,
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
    FollowUp {
        call_id: String,
        completion: FollowUpCompletion,
    },
}

#[derive(Debug)]
enum FollowUpCompletion {
    Tool {
        name: String,
        result_summary: String,
    },
}

struct ToolContext<'a> {
    parked: &'a mut ParkedCallStore,
    manifest: &'a ToolManifest,
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
    let mut generation = 0u64;
    let mut parked = match ParkedCallStore::open(cli.parked_calls_file()) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(%error, path = %cli.parked_calls_file().display(), "could not open Codex parked-call table");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = parked.mark_requests_stale() {
        tracing::error!(%error, "could not mark restored Codex request IDs stale");
        return ExitCode::from(1);
    }
    loop {
        generation = generation.saturating_add(1);
        match AppServer::spawn(&cli, generation).await {
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
                    &mut parked,
                    &cli.tool_manifest,
                )
                .await;
                if matches!(
                    outcome,
                    DriveOutcome::Shutdown | DriveOutcome::ChannelClosed
                ) {
                    // ChildStdin::shutdown() only flushes — dropping the
                    // handle is what closes the pipe and delivers the EOF
                    // the app-server exits on. Without it every drain ate
                    // the full 5s timeout and ended in SIGKILL.
                    let AppServer {
                        mut child, stdin, ..
                    } = server;
                    drop(stdin);
                    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                        .await
                        .is_err()
                    {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                    }
                    return ExitCode::SUCCESS;
                }
                let _ = server.child.start_kill();
                let _ = server.child.wait().await;
                if let Err(error) = parked.mark_requests_stale() {
                    tracing::error!(%error, "could not stale Codex requests after app-server crash");
                    return ExitCode::from(1);
                }
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
    async fn spawn(cli: &Cli, generation: u64) -> Result<Self, String> {
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
            generation,
        };
        server
            .request_wait(
                "initialize",
                json!({"clientInfo":{"name":"engrams","title":"Engrams","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
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
        let prior = tokio::fs::read_to_string(cli.thread_id_file())
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
        params["dynamicTools"] = dynamic_tools(&cli.tool_manifest);
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
        persist_thread_id(cli.thread_id_file(), &server.thread_id).await?;
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
    parked: &mut ParkedCallStore,
    manifest: &ToolManifest,
) -> DriveOutcome {
    let mut active: Option<String> = None;
    let mut pending = HashMap::<i64, Pending>::new();
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
                ToolContext { parked, manifest },
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
                Some(HarnessCommand::ToolResult { call_id, result_json }) => {
                    route_tool_result(
                        server,
                        parked,
                        &mut pending,
                        &active,
                        events,
                        &call_id,
                        result_json,
                    )
                    .await;
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
                    handle_message(
                        server,
                        value,
                        events,
                        &mut active,
                        &mut pending,
                        queued,
                        ToolContext { parked, manifest },
                    ).await;
                    if completed {
                        interrupt_deadline = None;
                        interrupt_requested = false;
                    }
                },
                Ok(None) | Err(_) => {
                    for request in pending.drain().map(|(_, request)| request) {
                        match request {
                            Pending::Start(prompt) | Pending::Steer(prompt) => queued.push_back(prompt),
                            Pending::Interrupt | Pending::FollowUp { .. } => {}
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

async fn handle_dynamic_tool_call(
    server: &mut AppServer,
    value: Value,
    events: &mpsc::Sender<HarnessEvent>,
    active: &Option<String>,
    parked: &mut ParkedCallStore,
    manifest: &ToolManifest,
) {
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let Some(request_id) = value.get("id").cloned() else {
        tracing::error!("item/tool/call request missing JSON-RPC id");
        return;
    };
    let Some(call_id) = params.get("callId").and_then(Value::as_str) else {
        tracing::error!(request_id = %request_id, "item/tool/call missing callId");
        return;
    };
    let Some(tool_name) = params.get("tool").and_then(Value::as_str) else {
        tracing::error!(%call_id, "item/tool/call missing tool name");
        return;
    };
    let Some(tool) = manifest
        .iter()
        .find(|tool| tool.name == tool_name && tool.codex_native_binding.is_none())
    else {
        tracing::error!(%call_id, %tool_name, "Codex requested an undeclared dynamic tool");
        let _ = server
            .respond(
                request_id,
                json!({
                    "success":false,
                    "contentItems":[{
                        "type":"inputText",
                        "text":format!("undeclared dynamic tool: {tool_name}")
                    }]
                }),
            )
            .await;
        return;
    };
    if tool.name == browser_view::TOOL_NAME {
        let result = params
            .pointer("/arguments/path")
            .and_then(Value::as_str)
            .ok_or_else(|| "browser_view requires an absolute string path".to_string())
            .and_then(browser_view::load);
        let response = match result {
            Ok(image) => json!({
                "success": true,
                "contentItems": [
                    {
                        "type": "inputText",
                        "text": "Internal browser observation. This image was not shared with the user."
                    },
                    {
                        "type": "inputImage",
                        "imageUrl": format!("data:{};base64,{}", image.mime_type, image.base64)
                    }
                ]
            }),
            Err(error) => json!({
                "success": false,
                "contentItems": [{"type": "inputText", "text": error}]
            }),
        };
        if let Err(error) = server.respond(request_id, response).await {
            tracing::error!(%error, %call_id, "could not return browser observation to Codex");
        }
        return;
    }
    let execution = match tool.execution {
        ToolExecution::Sync => "sync",
        ToolExecution::Deferred => "deferred",
    };
    let call = ParkedCall::new(
        call_id,
        ParkedCallKind::DynamicTool,
        request_id,
        tool_name,
        server.generation,
        json!({"execution":execution}),
    );
    if let Err(error) = parked.record(call) {
        tracing::error!(%error, %call_id, "could not durably park Codex dynamic tool call");
        return;
    }
    let run_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .or(active.as_deref())
        .unwrap_or("")
        .to_owned();
    emit(
        events,
        HarnessEvent::ToolCallRequested {
            run_id,
            call_id: call_id.to_owned(),
            name: tool_name.to_owned(),
            args_json: params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}))
                .to_string(),
        },
    )
    .await;
    if tool.execution == ToolExecution::Deferred {
        emit(events, HarnessEvent::Parked).await;
    }
}

async fn route_tool_result(
    server: &mut AppServer,
    parked: &mut ParkedCallStore,
    pending: &mut HashMap<i64, Pending>,
    active: &Option<String>,
    events: &mpsc::Sender<HarnessEvent>,
    call_id: &str,
    result_json: String,
) {
    let Some(call) = parked.get(call_id).cloned() else {
        tracing::error!(%call_id, "ToolResult has no parked Codex call");
        return;
    };
    if call.kind != ParkedCallKind::DynamicTool {
        tracing::error!(%call_id, kind = ?call.kind, "ToolResult does not match a dynamic tool call");
        return;
    }
    let native_question =
        call.context.get("nativeBinding").and_then(Value::as_str) == Some("requestUserInput");
    let answers = if native_question {
        match serde_json::from_str::<Answers>(&result_json) {
            Ok(answers) => Some(answers),
            Err(error) => {
                tracing::error!(%error, %call_id, "ToolResult for ask_user_question is not a canonical answer map");
                return;
            }
        }
    } else {
        None
    };
    if call.request_generation != server.generation || call.request_id.is_null() {
        let message = if native_question {
            format!(
                "Answer to the earlier question ({call_id}) after Codex restarted: {result_json}"
            )
        } else {
            format!(
                "The earlier `{}` tool call ({call_id}) completed after Codex restarted. Result: {result_json}",
                call.tool_name
            )
        };
        let completion = FollowUpCompletion::Tool {
            name: call.tool_name.clone(),
            result_summary: result_json,
        };
        if let Err(error) =
            send_follow_up(server, active, pending, call_id, message, completion).await
        {
            tracing::error!(%error, %call_id, "could not deliver late ToolResult as user message");
        }
        return;
    }
    if native_question {
        let codex_answers = codex_answers(&call, answers.as_ref().expect("parsed above"));
        if let Err(error) = server
            .respond(call.request_id, json!({"answers":codex_answers}))
            .await
        {
            tracing::error!(%error, %call_id, "could not answer Codex requestUserInput");
            return;
        }
    } else if let Err(error) = server
        .respond(
            call.request_id,
            json!({
                "success":true,
                "contentItems":[{"type":"inputText","text":result_json}]
            }),
        )
        .await
    {
        tracing::error!(%error, %call_id, "could not answer Codex dynamic tool call");
        return;
    }
    if let Err(error) = parked.take(call_id) {
        tracing::error!(%error, %call_id, "could not retire answered Codex dynamic tool call");
    }
    if native_question {
        emit(
            events,
            HarnessEvent::ToolCallCompleted {
                run_id: active.clone().unwrap_or_default(),
                tool_call_id: call_id.to_owned(),
                tool_name: call.tool_name,
                ok: true,
                duration_ms: 0,
                result_summary: Some(engram_harness_sdk::truncate_utf8(&result_json, MAX_SUMMARY)),
            },
        )
        .await;
    }
}

fn codex_answers(call: &ParkedCall, answers: &Answers) -> serde_json::Map<String, Value> {
    let ids_by_text = call.context.get("idsByText").and_then(Value::as_object);
    answers
        .iter()
        .map(|(text, selections)| {
            let id = ids_by_text
                .and_then(|ids| ids.get(text))
                .and_then(Value::as_str)
                .unwrap_or(text)
                .to_owned();
            (id, json!({"answers":selections}))
        })
        .collect()
}

async fn send_follow_up(
    server: &mut AppServer,
    active: &Option<String>,
    pending: &mut HashMap<i64, Pending>,
    call_id: &str,
    text: String,
    completion: FollowUpCompletion,
) -> Result<(), String> {
    let client_id = format!("codex-follow-up-{}", uuid::Uuid::new_v4());
    let request_id = if let Some(turn_id) = active.as_deref() {
        server
            .send_request(
                "turn/steer",
                json!({
                    "threadId":server.thread_id,
                    "expectedTurnId":turn_id,
                    "clientUserMessageId":client_id,
                    "input":[{"type":"text","text":text}],
                }),
            )
            .await?
    } else {
        start_turn(
            server,
            &QueuedPrompt {
                prompt_id: client_id,
                text,
            },
        )
        .await?
    };
    pending.insert(
        request_id,
        Pending::FollowUp {
            call_id: call_id.to_owned(),
            completion,
        },
    );
    Ok(())
}

async fn handle_message(
    server: &mut AppServer,
    value: Value,
    events: &mpsc::Sender<HarnessEvent>,
    active: &mut Option<String>,
    pending: &mut HashMap<i64, Pending>,
    queued: &mut VecDeque<QueuedPrompt>,
    tools: ToolContext<'_>,
) {
    if value.get("method").and_then(Value::as_str) == Some("item/tool/call") {
        handle_dynamic_tool_call(server, value, events, active, tools.parked, tools.manifest).await;
        return;
    }
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
        let ids_by_text: serde_json::Map<String, Value> = params
            .get("questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|q| {
                Some((
                    q.get("question")?.as_str()?.to_owned(),
                    Value::String(q.get("id")?.as_str()?.to_owned()),
                ))
            })
            .collect();
        let Some(request_id) = value.get("id").cloned() else {
            tracing::error!(%item_id, "requestUserInput missing JSON-RPC id");
            return;
        };
        let native_tool = tools
            .manifest
            .iter()
            .find(|tool| tool.codex_native_binding.as_deref() == Some("requestUserInput"))
            .map(|tool| tool.name.clone());
        let tool_name = native_tool.unwrap_or_else(|| {
            tracing::warn!(
                %item_id,
                "requestUserInput has no manifest native binding; falling back to deferred ask_user_question"
            );
            "ask_user_question".to_string()
        });
        let call = ParkedCall::new(
            &item_id,
            ParkedCallKind::DynamicTool,
            request_id,
            &tool_name,
            server.generation,
            json!({
                "nativeBinding":"requestUserInput",
                "idsByText":ids_by_text
            }),
        );
        if let Err(error) = tools.parked.record(call) {
            tracing::error!(%error, %item_id, "could not durably park Codex requestUserInput");
            return;
        }
        emit(
            events,
            HarnessEvent::ToolCallRequested {
                run_id,
                call_id: item_id,
                name: tool_name,
                args_json: json!({"questions":qs}).to_string(),
            },
        )
        .await;
        emit(events, HarnessEvent::Parked).await;
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
                Pending::FollowUp {
                    call_id,
                    completion,
                } if value.get("error").is_none() => {
                    if let Some(turn_id) = value.pointer("/result/turn/id").and_then(Value::as_str)
                    {
                        *active = Some(turn_id.to_owned());
                        emit(
                            events,
                            HarnessEvent::RunStarted {
                                run_id: turn_id.to_owned(),
                                prompt_summary: None,
                                prompt_id: None,
                            },
                        )
                        .await;
                    }
                    if let Err(error) = tools.parked.take(&call_id) {
                        tracing::error!(%error, %call_id, "could not retire crash-degraded Codex call");
                    }
                    match completion {
                        FollowUpCompletion::Tool {
                            name,
                            result_summary,
                        } => {
                            emit(
                                events,
                                HarnessEvent::ToolCallCompleted {
                                    run_id: active.clone().unwrap_or_default(),
                                    tool_call_id: call_id,
                                    tool_name: name,
                                    ok: true,
                                    duration_ms: 0,
                                    result_summary: Some(engram_harness_sdk::truncate_utf8(
                                        &result_summary,
                                        MAX_SUMMARY,
                                    )),
                                },
                            )
                            .await;
                        }
                    }
                }
                Pending::FollowUp { call_id, .. } => {
                    tracing::error!(%call_id, error = ?value.get("error"), "Codex rejected crash-degrade follow-up");
                }
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

async fn persist_thread_id(path: &Path, id: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(path, format!("{id}\n"))
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
    fn browser_view_is_declared_only_when_enabled() {
        let enabled = with_browser_view(Vec::new(), true);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, browser_view::TOOL_NAME);
        assert_eq!(
            dynamic_tools(&enabled)[0]["inputSchema"]["required"],
            json!(["path"])
        );
        assert!(with_browser_view(Vec::new(), false).is_empty());
    }

    async fn write_fake_codex(scripted_after_turn_start: &[&str]) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("fake-codex-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let script = base.join("codex");
        let record = base.join("requests.jsonl");
        let mut body = String::from("#!/bin/sh\n");
        body.push_str(&format!("record='{}'\n", record.display()));
        body.push_str(
            r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$record"
  id=$(printf '%s\n' "$line" | jq -r '.id // empty')
  method=$(printf '%s\n' "$line" | jq -r '.method // empty')
  case "$method" in
    initialize|account/login/start)
      printf '{"id":%s,"result":{}}\n' "$id"
      ;;
    thread/start|thread/resume)
      printf '{"id":%s,"result":{"thread":{"id":"t1","turns":[]}}}\n' "$id"
      ;;
    turn/start)
      printf '{"id":%s,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}\n' "$id"
"#,
        );
        for line in scripted_after_turn_start {
            body.push_str(&format!("      printf '%s\\n' '{line}'\n"));
        }
        body.push_str(
            r#"      ;;
  esac
done
"#,
        );
        tokio::fs::write(&script, body).await.unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        (script, record)
    }

    async fn write_crashing_question_fake() -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("fake-codex-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let script = base.join("codex");
        let record = base.join("requests.jsonl");
        let generation = base.join("generation");
        let mut body = String::from("#!/bin/sh\n");
        body.push_str(&format!("record='{}'\n", record.display()));
        body.push_str(&format!("generation_file='{}'\n", generation.display()));
        body.push_str(
            r#"generation=0
if [ -f "$generation_file" ]; then
  generation=$(cat "$generation_file")
fi
generation=$((generation + 1))
printf '%s\n' "$generation" > "$generation_file"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$record"
  id=$(printf '%s\n' "$line" | jq -r '.id // empty')
  method=$(printf '%s\n' "$line" | jq -r '.method // empty')
  case "$method" in
    initialize|account/login/start)
      printf '{"id":%s,"result":{}}\n' "$id"
      ;;
    thread/start)
      printf '{"id":%s,"result":{"thread":{"id":"t1","turns":[]}}}\n' "$id"
      ;;
    thread/resume)
      printf '{"id":%s,"result":{"thread":{"id":"t1","turns":[{"id":"turn-1","status":"interrupted","items":[]}]}}}\n' "$id"
      ;;
    turn/start)
      if [ "$generation" -eq 1 ]; then
        printf '{"id":%s,"result":{"turn":{"id":"turn-1","status":"inProgress"}}}\n' "$id"
        printf '%s\n' '{"id":88,"method":"item/tool/requestUserInput","params":{"itemId":"question-1","threadId":"t1","turnId":"turn-1","questions":[{"id":"q1","question":"Deploy now?","header":"Deploy","multiSelect":false,"options":[{"label":"Yes","description":"Deploy it"}]}]}}'
        exit 91
      else
        printf '{"id":%s,"result":{"turn":{"id":"turn-follow-up","status":"inProgress"}}}\n' "$id"
      fi
      ;;
  esac
done
"#,
        );
        tokio::fs::write(&script, body).await.unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        (script, record)
    }

    fn test_cli(codex_bin: PathBuf, codex_home: PathBuf) -> Cli {
        let parked_calls_file = codex_home
            .parent()
            .unwrap_or(&codex_home)
            .join("parked-calls.json");
        Cli {
            connect: None,
            port: Some(1),
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            binding_epoch: 1,
            codex_bin: Some(codex_bin),
            codex_home,
            thread_id_file: None,
            tool_manifest: manifest_from_env(),
            parked_calls_file: Some(parked_calls_file),
        }
    }

    fn native_question_manifest() -> ToolManifest {
        parse_tool_manifest(
            r#"[{"name":"ask_user_question","description":"Ask the user one or more structured questions.","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{"codex":"requestUserInput"}}]"#,
        )
        .unwrap()
    }

    async fn recorded_requests(path: &Path) -> Vec<Value> {
        tokio::fs::read_to_string(path)
            .await
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn fake_app_server_completes_one_prompt_turn() {
        let completed = r#"{"method":"turn/completed","params":{"threadId":"t1","turn":{"id":"turn-1","status":"completed"}}}"#;
        let (script, _) = write_fake_codex(&[completed]).await;
        let base = script.parent().unwrap();
        let mut cli = test_cli(script.clone(), base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));

        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-1".into(),
                text: "hello".into(),
            })
            .await
            .unwrap();
        let completed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut started = false;
            while let Some(event) = event_rx.recv().await {
                match event {
                    HarnessEvent::RunStarted { run_id, .. } => {
                        assert_eq!(run_id, "turn-1");
                        started = true;
                    }
                    HarnessEvent::RunCompleted { run_id, ok } => {
                        assert!(started);
                        assert_eq!(run_id, "turn-1");
                        assert!(ok);
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        if completed.is_err() {
            engine.abort();
        }
        completed.expect("fake app-server prompt turn timed out");

        engine.abort();
    }

    #[tokio::test]
    async fn initialize_enables_experimental_api() {
        let (script, record) = write_fake_codex(&[]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        let mut server = AppServer::spawn(&cli, 1).await.unwrap();

        let requests = recorded_requests(&record).await;
        let initialize = requests
            .iter()
            .find(|request| request.get("method") == Some(&json!("initialize")))
            .unwrap();
        assert_eq!(
            initialize.pointer("/params/capabilities/experimentalApi"),
            Some(&json!(true))
        );
        let _ = server.child.start_kill();
        let _ = server.child.wait().await;
    }

    async fn assert_manifest_declared_on_thread(method: &str, resume: bool) {
        let manifest = r#"[
            {"name":"save_memory","description":"Save a memory","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"execution":"sync","nativeBindings":{}},
            {"name":"ask_user_question","description":"Ask the user","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{"codex":"requestUserInput"}},
            {"name":"claude_native","description":"Only native on Claude","inputSchema":{"type":"object"},"execution":"sync","nativeBindings":{"claude":"Example"}}
        ]"#;
        std::env::set_var("ENGRAM_TOOLS", manifest);
        let (script, record) = write_fake_codex(&[]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        if resume {
            tokio::fs::write(cli.thread_id_file.as_ref().unwrap(), "existing-thread\n")
                .await
                .unwrap();
        }
        let mut server = AppServer::spawn(&cli, 1).await.unwrap();

        let requests = recorded_requests(&record).await;
        let thread = requests
            .iter()
            .find(|request| request.get("method") == Some(&json!(method)))
            .unwrap();
        assert_eq!(
            thread.pointer("/params/dynamicTools"),
            Some(&json!([
                {
                    "name":"save_memory",
                    "description":"Save a memory",
                    "inputSchema":{
                        "type":"object",
                        "properties":{"text":{"type":"string"}}
                    }
                },
                {
                    "name":"claude_native",
                    "description":"Only native on Claude",
                    "inputSchema":{"type":"object"}
                }
            ]))
        );
        let _ = server.child.start_kill();
        let _ = server.child.wait().await;
        std::env::remove_var("ENGRAM_TOOLS");
    }

    #[tokio::test]
    async fn thread_start_declares_manifest_dynamic_tools() {
        assert_manifest_declared_on_thread("thread/start", false).await;
    }

    #[tokio::test]
    async fn thread_resume_redeclares_manifest_dynamic_tools() {
        assert_manifest_declared_on_thread("thread/resume", true).await;
    }

    #[tokio::test]
    async fn deferred_dynamic_tool_call_emits_requested_then_parked_without_idle() {
        let tool_call = r#"{"id":77,"method":"item/tool/call","params":{"callId":"call-1","tool":"save_memory","arguments":{"text":"remember this"},"threadId":"t1","turnId":"turn-1"}}"#;
        let (script, _) = write_fake_codex(&[tool_call]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = parse_tool_manifest(
            r#"[{"name":"save_memory","description":"Save a memory","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{}}]"#,
        )
        .unwrap();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-1".into(),
                text: "remember".into(),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(
                    event_rx.recv().await,
                    Some(HarnessEvent::ToolCallRequested { ref call_id, .. })
                        if call_id == "call-1"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("deferred dynamic tool request timed out");
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv())
                .await
                .expect("deferred call did not emit a parked marker"),
            Some(HarnessEvent::Parked)
        ));
        let idle = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                if matches!(event_rx.recv().await, Some(HarnessEvent::Idle)) {
                    break;
                }
            }
        })
        .await;
        assert!(idle.is_err(), "a parked open turn must not emit Idle");
        engine.abort();
    }

    #[tokio::test]
    async fn sync_dynamic_tool_call_emits_request_and_routes_result() {
        let tool_call = r#"{"id":77,"method":"item/tool/call","params":{"callId":"call-1","tool":"save_memory","arguments":{"text":"remember this"},"threadId":"t1","turnId":"turn-1"}}"#;
        let (script, record) = write_fake_codex(&[tool_call]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = parse_tool_manifest(
            r#"[{"name":"save_memory","description":"Save a memory","inputSchema":{"type":"object"},"execution":"sync","nativeBindings":{}}]"#,
        )
        .unwrap();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-1".into(),
                text: "remember".into(),
            })
            .await
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(HarnessEvent::ToolCallRequested {
                    run_id,
                    call_id,
                    name,
                    args_json,
                }) = event_rx.recv().await
                {
                    break (run_id, call_id, name, args_json);
                }
            }
        })
        .await
        .expect("dynamic tool request timed out");
        assert_eq!(
            event,
            (
                "turn-1".into(),
                "call-1".into(),
                "save_memory".into(),
                r#"{"text":"remember this"}"#.into(),
            )
        );
        let parked = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                if matches!(event_rx.recv().await, Some(HarnessEvent::Parked)) {
                    break;
                }
            }
        })
        .await;
        assert!(parked.is_err(), "sync dynamic tools must not emit Parked");

        command_tx
            .send(HarnessCommand::ToolResult {
                call_id: "call-1".into(),
                result_json: r#"{"saved":true}"#.into(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let requests = recorded_requests(&record).await;
                if requests.iter().any(|request| {
                    request
                        == &json!({
                            "id":77,
                            "result":{
                                "success":true,
                                "contentItems":[{
                                    "type":"inputText",
                                    "text":r#"{"saved":true}"#
                                }]
                            }
                        })
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("dynamic tool result response timed out");
        engine.abort();
    }

    #[tokio::test]
    async fn native_request_user_input_uses_generic_frames_and_is_parked_durably() {
        let question = r#"{"id":88,"method":"item/tool/requestUserInput","params":{"itemId":"question-1","threadId":"t1","turnId":"turn-1","questions":[{"id":"q1","question":"Deploy now?","header":"Deploy","multiSelect":false,"options":[{"label":"Yes","description":"Deploy it"}]}]}}"#;
        let (script, record) = write_fake_codex(&[question]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = native_question_manifest();
        let parked_path = cli.parked_calls_file().to_path_buf();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-1".into(),
                text: "deploy".into(),
            })
            .await
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(HarnessEvent::ToolCallRequested {
                    run_id,
                    call_id,
                    name,
                    args_json,
                }) = event_rx.recv().await
                {
                    break (run_id, call_id, name, args_json);
                }
            }
        })
        .await
        .expect("requestUserInput event timed out");
        assert_eq!(event.0, "turn-1");
        assert_eq!(event.1, "question-1");
        assert_eq!(event.2, "ask_user_question");
        assert_eq!(
            serde_json::from_str::<Value>(&event.3).unwrap(),
            json!({"questions":[{
                "question":"Deploy now?",
                "header":"Deploy",
                "multiSelect":false,
                "options":[{"label":"Yes","description":"Deploy it"}]
            }]})
        );
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv())
                .await
                .expect("requestUserInput did not emit a parked marker"),
            Some(HarnessEvent::Parked)
        ));
        let parked = ParkedCallStore::open(parked_path).unwrap().all();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].tool_call_id, "question-1");
        assert_eq!(parked[0].kind, ParkedCallKind::DynamicTool);
        assert_eq!(parked[0].tool_name, "ask_user_question");
        assert_eq!(parked[0].request_id, json!(88));

        command_tx
            .send(HarnessCommand::ToolResult {
                call_id: "question-1".into(),
                result_json: json!({"Deploy now?":["Yes"]}).to_string(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if recorded_requests(&record).await.iter().any(|request| {
                    request
                        == &json!({
                            "id":88,
                            "result":{"answers":{"q1":{"answers":["Yes"]}}}
                        })
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("native question result response timed out");
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let Some(HarnessEvent::ToolCallCompleted {
                        tool_call_id,
                        tool_name,
                        ok,
                        ..
                    }) = event_rx.recv().await
                    {
                        break (tool_call_id, tool_name, ok);
                    }
                }
            })
            .await,
            Ok((call_id, name, true))
                if call_id == "question-1" && name == "ask_user_question"
        ));
        engine.abort();
    }

    #[tokio::test]
    async fn unbound_request_user_input_falls_back_to_generic_question_protocol() {
        let question = r#"{"id":88,"method":"item/tool/requestUserInput","params":{"itemId":"legacy-question","threadId":"t1","turnId":"turn-1","questions":[{"id":"q1","question":"Deploy now?","header":"Deploy","multiSelect":false,"options":[{"label":"Yes","description":"Deploy it"}]}]}}"#;
        let (script, record) = write_fake_codex(&[question]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = Vec::new();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-legacy".into(),
                text: "deploy".into(),
            })
            .await
            .unwrap();
        let requested = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(HarnessEvent::ToolCallRequested {
                    call_id,
                    name,
                    args_json,
                    ..
                }) = event_rx.recv().await
                {
                    break (call_id, name, args_json);
                }
            }
        })
        .await
        .expect("fallback requestUserInput event timed out");
        assert_eq!(requested.0, "legacy-question");
        assert_eq!(requested.1, "ask_user_question");
        assert_eq!(
            serde_json::from_str::<Value>(&requested.2).unwrap(),
            json!({"questions":[{
                "question":"Deploy now?",
                "header":"Deploy",
                "multiSelect":false,
                "options":[{"label":"Yes","description":"Deploy it"}]
            }]})
        );

        command_tx
            .send(HarnessCommand::ToolResult {
                call_id: "legacy-question".into(),
                result_json: json!({"Deploy now?":["Yes"]}).to_string(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if recorded_requests(&record).await.iter().any(|request| {
                    request
                        == &json!({
                            "id":88,
                            "result":{"answers":{"q1":{"answers":["Yes"]}}}
                        })
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fallback ToolResult response timed out");
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if let Some(HarnessEvent::ToolCallCompleted {
                        tool_call_id,
                        tool_name,
                        ok,
                        ..
                    }) = event_rx.recv().await
                    {
                        break (tool_call_id, tool_name, ok);
                    }
                }
            })
            .await,
            Ok((call_id, name, true))
                if call_id == "legacy-question"
                    && name == "ask_user_question"
        ));
        engine.abort();
    }

    #[tokio::test]
    async fn shutdown_returns_promptly_while_dynamic_tool_call_is_parked() {
        let tool_call = r#"{"id":77,"method":"item/tool/call","params":{"callId":"call-1","tool":"save_memory","arguments":{"text":"remember this"},"threadId":"t1","turnId":"turn-1"}}"#;
        let (script, _) = write_fake_codex(&[tool_call]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = parse_tool_manifest(
            r#"[{"name":"save_memory","description":"Save a memory","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{}}]"#,
        )
        .unwrap();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-1".into(),
                text: "remember".into(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(event_rx.recv().await, Some(HarnessEvent::Parked)) {
                    break;
                }
            }
        })
        .await
        .expect("deferred call did not park");

        command_tx
            .send(HarnessCommand::Shutdown { grace_secs: 1 })
            .await
            .unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), engine)
            .await
            .expect("shutdown waited on the open JSON-RPC tool call")
            .expect("engine task panicked");
        assert_eq!(exit, ExitCode::SUCCESS);
    }

    #[tokio::test]
    async fn native_question_result_after_app_server_crash_becomes_follow_up_user_message() {
        let (script, record) = write_crashing_question_fake().await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = native_question_manifest();
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-1".into(),
                text: "deploy".into(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(
                    event_rx.recv().await,
                    Some(HarnessEvent::ToolCallRequested { ref call_id, ref name, .. })
                        if call_id == "question-1" && name == "ask_user_question"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("pre-crash question event timed out");
        tokio::time::timeout(std::time::Duration::from_secs(6), async {
            loop {
                if recorded_requests(&record)
                    .await
                    .iter()
                    .any(|request| request.get("method") == Some(&json!("thread/resume")))
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("fake app-server did not respawn");

        command_tx
            .send(HarnessCommand::ToolResult {
                call_id: "question-1".into(),
                result_json: json!({"Deploy now?":["Yes"]}).to_string(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let requests = recorded_requests(&record).await;
                if requests.iter().any(|request| {
                    request.get("method") == Some(&json!("turn/start"))
                        && request
                            .pointer("/params/input/0/text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.contains("question-1") && text.contains("Yes"))
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("late answer was not delivered as a follow-up user message");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if matches!(
                    event_rx.recv().await,
                    Some(HarnessEvent::ToolCallCompleted {
                        ref tool_call_id,
                        ref tool_name,
                        ok: true,
                        ..
                    }) if tool_call_id == "question-1" && tool_name == "ask_user_question"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("late result was not confirmed after the follow-up was accepted");
        engine.abort();
    }

    #[tokio::test]
    async fn unknown_result_logs_an_error_without_panicking() {
        use std::io::Write;
        use std::sync::Mutex;

        #[derive(Clone)]
        struct LogWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let logs = Arc::new(Mutex::new(Vec::new()));
        let make_writer = {
            let logs = logs.clone();
            move || LogWriter(logs.clone())
        };
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .with_writer(make_writer)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (script, _) = write_fake_codex(&[]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let engine = tokio::spawn(run_engine(
            cli,
            command_rx,
            Arc::new(Notify::new()),
            event_tx,
        ));
        // This test launches a shell + jq-backed fake app-server alongside the
        // rest of the nextest process set. Two seconds flaked under ordinary
        // parallel load even though the same test completed in 350ms alone.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !matches!(event_rx.recv().await, Some(HarnessEvent::Idle)) {}
        })
        .await
        .unwrap();
        command_tx
            .send(HarnessCommand::ToolResult {
                call_id: "missing-tool".into(),
                result_json: "null".into(),
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(output.contains("missing-tool"), "logs were: {output}");
        assert!(!engine.is_finished(), "unknown correlation must not panic");
        engine.abort();
    }

    async fn write_fake_schema_codex(include_malformed_dynamic: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("fake-codex-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let script = base.join("codex");
        let mut body =
            String::from("#!/bin/sh\nfor out do :; done\nmkdir -p \"$out/v1\" \"$out/v2\"\n");
        let schemas = [
            ("v2/ThreadStartParams.json", r#"{}"#),
            ("v2/ThreadResumeParams.json", r#"{}"#),
            ("v2/TurnStartParams.json", r#"{}"#),
            (
                "v2/TurnSteerParams.json",
                r#"{"properties":{"clientUserMessageId":{},"expectedTurnId":{}}}"#,
            ),
            (
                "v2/TurnInterruptParams.json",
                r#"{"properties":{"turnId":{},"threadId":{}}}"#,
            ),
            ("v2/TurnCompletedNotification.json", r#"{}"#),
            (
                "v2/AgentMessageDeltaNotification.json",
                r#"{"properties":{"delta":{},"itemId":{}}}"#,
            ),
            ("v2/FileChangePatchUpdatedNotification.json", r#"{}"#),
            (
                "v2/ThreadNameUpdatedNotification.json",
                r#"{"properties":{"threadId":{},"threadName":{}}}"#,
            ),
            ("ToolRequestUserInputParams.json", r#"{}"#),
            (
                "ServerNotification.json",
                r#"{"definitions":{"TurnStatus":{"enum":["completed","failed","interrupted","inProgress"]}}}"#,
            ),
        ];
        for (path, schema) in schemas {
            body.push_str(&format!("printf '%s\\n' '{schema}' > \"$out/{path}\"\n"));
        }
        if include_malformed_dynamic {
            for path in [
                "v1/InitializeParams.json",
                "DynamicToolCallParams.json",
                "DynamicToolCallResponse.json",
            ] {
                body.push_str(&format!("printf '%s\\n' '{{}}' > \"$out/{path}\"\n"));
            }
        }
        tokio::fs::write(&script, body).await.unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        script
    }

    async fn run_schema_checker(fake_codex: &Path) -> std::process::Output {
        let checker = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../deploy/harness-codex/check-app-server-schema.sh");
        tokio::process::Command::new("bash")
            .arg(checker)
            .arg(fake_codex)
            .output()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn schema_checker_rejects_missing_dynamic_tool_contract() {
        let fake = write_fake_schema_codex(false).await;
        let output = run_schema_checker(&fake).await;
        assert!(
            !output.status.success(),
            "checker accepted schemas with no dynamic-tool contract"
        );
    }

    #[tokio::test]
    async fn schema_checker_rejects_malformed_dynamic_tool_contract() {
        let fake = write_fake_schema_codex(true).await;
        let output = run_schema_checker(&fake).await;
        assert!(
            !output.status.success(),
            "checker accepted malformed dynamic-tool schemas"
        );
    }

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
