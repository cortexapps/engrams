use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;

use clap::Parser;
use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, AgentRole, FileChange, ForgeOp, ForgeRequest, ForgeResponse,
    HarnessCommand, HarnessEvent, FORGE_VSOCK_PORT,
};
use engram_harness_sdk::browser_view;
use engram_harness_sdk::parked::{ParkedCall, ParkedCallKind, ParkedCallStore};
use engram_harness_sdk::questions::{Answers, Question, QuestionOption};
use engram_harness_sdk::{emit, Channels, ConnectionConfig, QueuedPrompt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex, Notify};

const DEFAULT_CODEX_HOME: &str = "/workspace/.engrams/codex";
const THREAD_ID_FILE: &str = "/workspace/.engrams/codex-thread-id";
const PARKED_CALLS_FILE: &str = "/workspace/.engrams/codex-parked-calls.json";
const MAX_SUMMARY: usize = 4096;
const MAX_OAUTH_BUNDLE_BYTES: usize = 256 * 1024;
const OPENAI_CODEX_PROVIDER: &str = "openai-codex";

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
    /// Test seam for the ADR 0107 session-mode stamp.
    #[arg(skip)]
    mode_stamp_file: Option<PathBuf>,
    /// Test-only service-account credential; production reads CODEX_API_KEY.
    #[arg(skip)]
    test_api_key: Option<String>,
    /// Test-only credential broker override.
    #[arg(skip)]
    test_credential_control: Option<CredentialControl>,
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

    fn mode_stamp_file(&self) -> &Path {
        self.mode_stamp_file
            .as_deref()
            .unwrap_or_else(|| Path::new(engram_harness_sdk::mode_stamp::MODE_STAMP_FILE))
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
    /// ADR 0107: the session-mode stamp path — read at every turn start so
    /// per-turn params (sandbox, plan preamble) follow the latch.
    mode_stamp_path: PathBuf,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: i64,
    thread_id: String,
    persisted_prompts: HashMap<String, PersistedTurn>,
    buffered: VecDeque<Value>,
    stderr_task: Option<tokio::task::JoinHandle<Vec<String>>>,
    oauth_watcher: Option<tokio::task::JoinHandle<()>>,
    _oauth_home: Option<tempfile::TempDir>,
    generation: u64,
}

#[derive(Clone)]
struct CredentialControl {
    session_id: SessionId,
    broker_token: String,
    endpoint: Option<String>,
}

impl std::fmt::Debug for CredentialControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialControl")
            .field("session_id", &self.session_id)
            .field("endpoint", &self.endpoint)
            .field("broker_token", &"[redacted]")
            .finish()
    }
}

struct OAuthSession {
    control: CredentialControl,
    version: i64,
    last_payload: Vec<u8>,
}

fn codex_app_server_command(bin: &Path, runtime_home: &Path) -> Command {
    let mut command = Command::new(bin);
    command
        .args(["app-server", "--stdio"])
        .env("CODEX_HOME", runtime_home)
        .env("CODEX_NON_INTERACTIVE", "1")
        // Authentication is delivered over the trusted app-server/control
        // protocols. Neither provider credentials nor the session broker
        // capability may be inherited by the child process environment.
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ENGRAM_CREDENTIAL_BROKER_TOKEN")
        .env_remove("ENGRAM_CREDENTIAL_ENDPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

impl CredentialControl {
    fn from_env(session_id: SessionId) -> Result<Self, String> {
        let broker_token = std::env::var("ENGRAM_CREDENTIAL_BROKER_TOKEN")
            .map_err(|_| "OpenAI is not connected for this session".to_string())?;
        Ok(Self {
            session_id,
            broker_token,
            endpoint: std::env::var("ENGRAM_CREDENTIAL_ENDPOINT").ok(),
        })
    }

    async fn exchange(&self, op: ForgeOp) -> Result<ForgeResponse, String> {
        let request = ForgeRequest {
            session_id: self.session_id,
            broker_token: self.broker_token.clone(),
            op,
        };
        if let Some(endpoint) = &self.endpoint {
            let url = format!(
                "{}/api/v1/sessions/{}/credential-control",
                endpoint.trim_end_matches('/'),
                self.session_id
            );
            return reqwest::Client::new()
                .post(url)
                .json(&request)
                .send()
                .await
                .map_err(|_| "credential control request failed".to_string())?
                .error_for_status()
                .map_err(|_| "credential control request was rejected".to_string())?
                .json()
                .await
                .map_err(|_| "credential control response was malformed".to_string());
        }
        let transport = engram_transport::from_env()
            .map_err(|_| "credential control transport unavailable".to_string())?;
        let mut stream = transport
            .dial(FORGE_VSOCK_PORT)
            .await
            .map_err(|_| "credential control connection failed".to_string())?;
        write_msg(&mut stream, &request)
            .await
            .map_err(|_| "credential control request failed".to_string())?;
        read_msg(&mut stream)
            .await
            .map_err(|_| "credential control response was malformed".to_string())
    }
}

impl OAuthSession {
    async fn fetch(session_id: SessionId) -> Result<Self, String> {
        Self::fetch_from(CredentialControl::from_env(session_id)?).await
    }

    async fn fetch_from(control: CredentialControl) -> Result<Self, String> {
        match control.exchange(ForgeOp::FetchOAuthCredential).await? {
            ForgeResponse::OAuthCredential {
                provider,
                version,
                opaque_bundle,
            } if provider == OPENAI_CODEX_PROVIDER
                && !opaque_bundle.is_empty()
                && opaque_bundle.len() <= MAX_OAUTH_BUNDLE_BYTES =>
            {
                Ok(Self {
                    control,
                    version,
                    last_payload: opaque_bundle,
                })
            }
            ForgeResponse::Error { message } => {
                Err(format!("OAuth credential unavailable: {message}"))
            }
            _ => Err("credential control returned an invalid OpenAI cache".into()),
        }
    }

    async fn sync_cache(&mut self, path: &Path) -> Result<(), String> {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("could not inspect refreshed OAuth cache".into()),
        };
        if !metadata.file_type().is_file() || metadata.len() as usize > MAX_OAUTH_BUNDLE_BYTES {
            let _ = remove_auth_cache(path).await;
            return Err("refreshed OAuth cache was rejected".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                let _ = remove_auth_cache(path).await;
                return Err("refreshed OAuth cache permissions were rejected".into());
            }
        }
        let payload = tokio::fs::read(path)
            .await
            .map_err(|_| "could not read refreshed OAuth cache".to_string())?;
        if payload.is_empty() {
            let _ = remove_auth_cache(path).await;
            return Err("refreshed OAuth cache was empty".into());
        }
        if payload != self.last_payload {
            let response = self
                .control
                .exchange(ForgeOp::UpdateOAuthCredential {
                    expected_version: self.version,
                    opaque_bundle: payload.clone(),
                })
                .await;
            let removal = remove_auth_cache(path).await;
            match response? {
                ForgeResponse::OAuthCredential {
                    provider,
                    version,
                    opaque_bundle,
                } if provider == OPENAI_CODEX_PROVIDER => {
                    self.version = version;
                    self.last_payload = opaque_bundle;
                }
                ForgeResponse::Error { message } if message == "version_conflict" => {
                    // The winning cache is fetched on the next app-server
                    // restart boundary; never overwrite it with stale state.
                    tracing::warn!("OAuth cache refresh lost a version race; deferring to restart");
                }
                ForgeResponse::Error { message } => {
                    return Err(format!("OAuth cache refresh rejected: {message}"));
                }
                _ => return Err("credential control returned an invalid refresh response".into()),
            }
            removal?;
            return Ok(());
        }
        remove_auth_cache(path).await
    }
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
                    if let Some(watcher) = server.oauth_watcher.take() {
                        watcher.abort();
                    }
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
                if let Some(watcher) = server.oauth_watcher.take() {
                    watcher.abort();
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

async fn write_auth_cache(path: &Path, payload: &[u8]) -> Result<(), String> {
    if payload.is_empty() || payload.len() > MAX_OAUTH_BUNDLE_BYTES {
        return Err("OAuth cache size rejected".into());
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .await
        .map_err(|_| "could not create private OAuth cache".to_string())?;
    file.write_all(payload)
        .await
        .map_err(|_| "could not write OAuth cache".to_string())?;
    file.flush()
        .await
        .map_err(|_| "could not flush OAuth cache".to_string())
}

async fn remove_auth_cache(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("could not remove OAuth cache".into()),
    }
}

fn spawn_oauth_watcher(
    oauth: Arc<Mutex<OAuthSession>>,
    auth_path: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = oauth.lock().await.sync_cache(&auth_path).await {
                // Error strings are deliberately provider-independent and never
                // include cache bytes, JSON-RPC messages, or token values.
                tracing::warn!(%error, "could not synchronize refreshed OAuth cache");
            }
        }
    })
}

impl AppServer {
    async fn spawn(cli: &Cli, generation: u64) -> Result<Self, String> {
        let api_key = cli
            .test_api_key
            .clone()
            .or_else(|| std::env::var("CODEX_API_KEY").ok());
        let oauth = if api_key.is_none() {
            Some(match cli.test_credential_control.clone() {
                Some(control) => OAuthSession::fetch_from(control).await?,
                None => OAuthSession::fetch(cli.session_id).await?,
            })
        } else {
            None
        };
        let oauth_home = if oauth.is_some() {
            let dir = tempfile::Builder::new()
                .prefix("engram-codex-oauth-")
                .tempdir()
                .map_err(|_| "could not create private Codex home".to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                    .map_err(|_| "could not protect private Codex home".to_string())?;
            }
            Some(dir)
        } else {
            tokio::fs::create_dir_all(&cli.codex_home)
                .await
                .map_err(|e| format!("create CODEX_HOME: {e}"))?;
            None
        };
        let runtime_home = oauth_home
            .as_ref()
            .map_or_else(|| cli.codex_home.clone(), |dir| dir.path().to_path_buf());
        ensure_skills_link(&runtime_home).await;
        let auth_path = runtime_home.join("auth.json");
        if let Some(oauth) = &oauth {
            write_auth_cache(&auth_path, &oauth.last_payload).await?;
        }
        let bin = cli.codex_bin.clone().unwrap_or_else(resolve_codex_bin);
        let mut command = codex_app_server_command(&bin, &runtime_home);
        let mut child = command.spawn().map_err(|e| format!("spawn {bin:?}: {e}"))?;
        let stdin = child.stdin.take().ok_or("app-server stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("app-server stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("app-server stderr unavailable")?;
        let mut server = Self {
            child,
            mode_stamp_path: cli.mode_stamp_file().to_path_buf(),
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 1,
            thread_id: String::new(),
            persisted_prompts: HashMap::new(),
            buffered: VecDeque::new(),
            stderr_task: Some(engram_harness_sdk::spawn_stderr_tail(stderr, 64, true)),
            oauth_watcher: None,
            _oauth_home: oauth_home,
            generation,
        };
        server
            .request_wait(
                "initialize",
                json!({"clientInfo":{"name":"engrams","title":"Engrams","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),
            )
            .await?;
        server.notify("initialized", json!({})).await?;
        if let Some(api_key) = api_key {
            server
                .request_wait(
                    "account/login/start",
                    json!({"type":"apiKey","apiKey":api_key}),
                )
                .await?;
        } else {
            let response = server
                .request_wait("account/read", json!({"refreshToken":true}))
                .await?;
            if response
                .pointer("/result/account/type")
                .and_then(Value::as_str)
                != Some("chatgpt")
            {
                return Err("Codex did not load managed ChatGPT authentication".into());
            }
        }
        let oauth = oauth.map(|oauth| Arc::new(Mutex::new(oauth)));
        if let Some(oauth) = &oauth {
            oauth.lock().await.sync_cache(&auth_path).await?;
        } else if let Err(error) = remove_auth_cache(&auth_path).await {
            tracing::warn!(%error, "could not remove Codex credential cache");
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
        if let Some(oauth) = oauth {
            server.oauth_watcher = Some(spawn_oauth_watcher(oauth, auth_path));
        }
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
                Some(HarnessCommand::Prompt { prompt_id, text, mode }) => {
                    // ADR 0107: latch the mode directive to the workspace
                    // stamp; every subsequent turn start reads it (per-turn
                    // sandbox + preamble — codex never respawns for a mode).
                    if let Some(mode) = &mode {
                        if let Err(e) = engram_harness_sdk::mode_stamp::write_mode_stamp(
                            &server.mode_stamp_path,
                            mode,
                        ) {
                            tracing::warn!(error = %e, %mode, "mode stamp write failed");
                        }
                    }
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
                        RouteCtx {
                            pending: &mut pending,
                            active: &active,
                            queued,
                        },
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

/// ADR 0107: the harness-owned plan-turn preamble. Codex has no native plan
/// permission mode; the read-only turn sandbox is the enforcement and this
/// contract tells the model how the pass ends.
const PLAN_MODE_PREAMBLE: &str = "You are in plan mode: a read-only design pass. \
    Explore the repository and design an implementation plan. Do not modify files \
    and do not run mutating commands. When your plan is complete, call the \
    exit_plan_mode tool with the full plan as markdown and wait for the review \
    decision.";

/// ADR 0107: the synthesized build turn after an approval (the plan turn is
/// interrupted; this fresh turn starts under the flipped, full-access stamp).
const PLAN_APPROVED_MESSAGE: &str =
    "Your plan was approved. Implement it now, following the plan you presented.";

async fn start_turn(server: &mut AppServer, prompt: &QueuedPrompt) -> Result<i64, String> {
    // ADR 0107: per-turn mode application — no respawn, ever. The stamp is
    // read fresh at every turn start; `sandboxPolicy` is an explicit
    // override each time BECAUSE the app-server treats it as sticky ("this
    // turn and subsequent turns"), so the build turn after an approval must
    // restore the external-sandbox policy itself.
    let plan_mode =
        engram_harness_sdk::mode_stamp::read_mode_stamp(&server.mode_stamp_path) == "plan";
    let text = if plan_mode {
        format!("{PLAN_MODE_PREAMBLE}\n\n{}", prompt.text)
    } else {
        prompt.text.clone()
    };
    let sandbox = if plan_mode {
        // Codex's own OS sandbox enforces read-only inside the VM; network
        // stays on (the VM egress proxy is the real gate).
        json!({"type":"readOnly","networkAccess":true})
    } else {
        json!({"type":"externalSandbox","networkAccess":"enabled"})
    };
    let mut params = json!({
        "threadId":server.thread_id,
        "clientUserMessageId":prompt.prompt_id,
        "input":[{"type":"text","text":text}],
        "approvalPolicy":"never",
        "sandboxPolicy":sandbox,
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
    if tool.name == "exit_plan_mode"
        && engram_harness_sdk::mode_stamp::read_mode_stamp(&server.mode_stamp_path) != "plan"
    {
        // ADR 0107: outside plan mode the exit tool is a protocol error, not
        // a park — nothing is waiting to review a plan.
        let _ = server
            .respond(
                request_id,
                json!({
                    "success": false,
                    "contentItems": [{
                        "type": "inputText",
                        "text": "Not in plan mode — no reviewer is waiting for a plan, so this \
                call did nothing. Tell the user: if they want an approval-gated plan, they can \
                turn on Plan mode (the plan chip next to the composer, or Shift+Tab) and ask \
                again. Then continue with the task as normal."
                    }]
                }),
            )
            .await;
        return;
    }
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

/// The drive-loop state `route_tool_result` mutates — bundled (like
/// `ToolContext`) so the signature stays within clippy's argument budget as
/// the plan-mode routing grew it.
struct RouteCtx<'a> {
    pending: &'a mut HashMap<i64, Pending>,
    active: &'a Option<String>,
    queued: &'a mut VecDeque<QueuedPrompt>,
}

async fn route_tool_result(
    server: &mut AppServer,
    parked: &mut ParkedCallStore,
    ctx: RouteCtx<'_>,
    events: &mpsc::Sender<HarnessEvent>,
    call_id: &str,
    result_json: String,
) {
    let RouteCtx {
        pending,
        active,
        queued,
    } = ctx;
    let Some(call) = parked.get(call_id).cloned() else {
        tracing::error!(%call_id, "ToolResult has no parked Codex call");
        return;
    };
    if call.kind != ParkedCallKind::DynamicTool {
        tracing::error!(%call_id, kind = ?call.kind, "ToolResult does not match a dynamic tool call");
        return;
    }
    // ADR 0107: an approved plan flips the stamp FIRST, so every turn that
    // starts after this point (the live build turn below, or the
    // stale-generation follow-up) reads full access. A reject leaves the
    // plan stamp and rides the ordinary respond-in-place path: the model
    // sees `{decision:"reject", feedback}` as the tool result and keeps
    // planning in the same read-only turn.
    let plan_decision = (call.tool_name == "exit_plan_mode")
        .then(|| engram_harness_sdk::plan::parse_plan_decision(&result_json))
        .flatten();
    // ADR 0107: a REJECT must read as a FAILED call with the reviewer's
    // feedback as the reason — the session fe3cd981 regression: answering it
    // `success: true` with raw decision JSON made the model narrate "plan
    // submitted" and end the turn instead of revising. Mirrors the claude
    // adapter's deny-with-reason semantics.
    if let Some(decision) = plan_decision.as_ref().filter(|d| !d.approved()) {
        let text = format!(
            "{} Revise the plan now and call exit_plan_mode again with the              updated markdown. Stay in plan mode and do not modify files.",
            decision.reject_reason()
        );
        if call.request_generation == server.generation && !call.request_id.is_null() {
            if let Err(error) = server
                .respond(
                    call.request_id.clone(),
                    json!({
                        "success": false,
                        "contentItems": [{"type": "inputText", "text": text}]
                    }),
                )
                .await
            {
                tracing::error!(%error, %call_id, "could not answer rejected exit_plan_mode");
                return;
            }
            if let Err(error) = parked.take(call_id) {
                tracing::error!(%error, %call_id, "could not retire rejected plan call");
            }
            return;
        }
        // Stale generation: deliver the revision ask as a follow-up user turn
        // (the stamp is still `plan`, so it starts read-only).
        let completion = FollowUpCompletion::Tool {
            name: call.tool_name.clone(),
            result_summary: result_json,
        };
        if let Err(error) = send_follow_up(server, active, pending, call_id, text, completion).await
        {
            tracing::error!(%error, %call_id, "could not deliver plan rejection as user message");
        }
        return;
    }
    let plan_approval = plan_decision.filter(|decision| decision.approved());
    if plan_approval.is_some() {
        if let Err(error) = engram_harness_sdk::mode_stamp::write_mode_stamp(
            &server.mode_stamp_path,
            engram_harness_sdk::mode_stamp::DEFAULT_MODE,
        ) {
            tracing::error!(%error, %call_id, "mode stamp flip on plan approval failed");
        }
    }
    if let Some(decision) = plan_approval {
        if call.request_generation == server.generation && !call.request_id.is_null() {
            // Live parked call: acknowledge it, interrupt the read-only plan
            // turn, and queue the build turn — turn/completed consumes the
            // queue, and start_turn reads the flipped stamp (full access).
            let _ = decision;
            if let Err(error) = server
                .respond(
                    call.request_id.clone(),
                    json!({
                        "success": true,
                        "contentItems": [{
                            "type": "inputText",
                            "text": "Plan approved. A fresh build turn starts next."
                        }]
                    }),
                )
                .await
            {
                tracing::error!(%error, %call_id, "could not answer approved exit_plan_mode");
            }
            if let Err(error) = parked.take(call_id) {
                tracing::error!(%error, %call_id, "could not retire approved plan call");
            }
            emit(
                events,
                HarnessEvent::ToolCallCompleted {
                    run_id: active.clone().unwrap_or_default(),
                    tool_call_id: call_id.to_owned(),
                    tool_name: call.tool_name.clone(),
                    ok: true,
                    duration_ms: 0,
                    result_summary: Some("approved".to_string()),
                },
            )
            .await;
            if let Some(turn_id) = active.as_deref() {
                match server
                    .send_request(
                        "turn/interrupt",
                        json!({"threadId": server.thread_id, "turnId": turn_id}),
                    )
                    .await
                {
                    Ok(id) => {
                        pending.insert(id, Pending::Interrupt);
                    }
                    Err(error) => {
                        tracing::error!(%error, %call_id, "could not interrupt the plan turn");
                    }
                }
            }
            queued.push_back(QueuedPrompt {
                prompt_id: format!("plan-approved-{}", uuid::Uuid::new_v4()),
                text: PLAN_APPROVED_MESSAGE.to_string(),
            });
            return;
        }
        // Stale generation (harness respawned since the park): the follow-up
        // machinery delivers the approval as a fresh user turn — which now
        // starts full-access because the stamp already flipped.
        let completion = FollowUpCompletion::Tool {
            name: call.tool_name.clone(),
            result_summary: result_json,
        };
        if let Err(error) = send_follow_up(
            server,
            active,
            pending,
            call_id,
            PLAN_APPROVED_MESSAGE.to_string(),
            completion,
        )
        .await
        {
            tracing::error!(%error, %call_id, "could not deliver plan approval as user message");
        }
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
                "exclude": ["CODEX_API_KEY", "OPENAI_API_KEY", "CODEX_HOME", "ENGRAM_CREDENTIAL_BROKER_TOKEN", "ENGRAM_CREDENTIAL_ENDPOINT"]
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

    async fn write_fake_oauth_codex() -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let base = std::env::temp_dir().join(format!("fake-codex-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await.unwrap();
        let script = base.join("codex");
        let record = base.join("requests.jsonl");
        let body = format!(
            r#"#!/bin/sh
record='{}'
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$record"
  id=$(printf '%s\n' "$line" | jq -r '.id // empty')
  method=$(printf '%s\n' "$line" | jq -r '.method // empty')
  case "$method" in
    initialize) printf '{{"id":%s,"result":{{}}}}\n' "$id" ;;
    account/read) printf '{{"id":%s,"result":{{"account":{{"type":"chatgpt","email":"person@example.com","planType":"plus"}}}}}}\n' "$id" ;;
    thread/start)
      if [ -e "$CODEX_HOME/auth.json" ]; then printf '%s\n' 'AUTH_CACHE_LEAKED' >> "$record"; fi
      printf '{{"id":%s,"result":{{"thread":{{"id":"t1","turns":[]}}}}}}\n' "$id"
      ;;
  esac
done
"#,
            record.display()
        );
        tokio::fs::write(&script, body).await.unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, record)
    }

    async fn serve_one_oauth_fetch(payload: Vec<u8>) -> String {
        use tokio::io::AsyncReadExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 64 * 1024];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0);
            let body = serde_json::to_vec(&ForgeResponse::OAuthCredential {
                provider: OPENAI_CODEX_PROVIDER.into(),
                version: 1,
                opaque_bundle: payload,
            })
            .unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&body).await.unwrap();
        });
        format!("http://{address}")
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
            mode_stamp_file: Some(
                std::env::temp_dir().join(format!("codex-mode-stamp-{}", uuid::Uuid::new_v4())),
            ),
            test_api_key: Some("test-api-key".into()),
            test_credential_control: None,
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
                mode: None,
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
    async fn oauth_cache_is_imported_and_removed_before_thread_start() {
        let payload = serde_json::to_vec(&json!({
            "auth_mode":"chatgpt",
            "OPENAI_API_KEY":null,
            "tokens":{
                "access_token":"access-secret",
                "refresh_token":"refresh-secret",
                "account_id":"acct-1"
            }
        }))
        .unwrap();
        let endpoint = serve_one_oauth_fetch(payload).await;
        let (script, record) = write_fake_oauth_codex().await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("unused-home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.test_api_key = None;
        cli.test_credential_control = Some(CredentialControl {
            session_id: cli.session_id,
            broker_token: "session-broker-token".into(),
            endpoint: Some(endpoint),
        });

        let mut server = AppServer::spawn(&cli, 1).await.unwrap();
        if let Some(watcher) = server.oauth_watcher.take() {
            watcher.abort();
        }
        let requests = tokio::fs::read_to_string(record).await.unwrap();
        assert!(requests.contains("account/read"));
        assert!(requests.contains("thread/start"));
        assert!(!requests.contains("AUTH_CACHE_LEAKED"));
        assert!(!requests.contains("access-secret"));
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
                mode: None,
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

    /// ADR 0107: a `plan` mode directive makes the turn start read-only
    /// with the harness-owned preamble — per-turn params, no respawn.
    #[tokio::test]
    async fn plan_mode_prompt_starts_a_read_only_turn_with_preamble() {
        let (script, record) = write_fake_codex(&[]).await;
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
        command_tx
            .send(HarnessCommand::Prompt {
                prompt_id: "prompt-plan".into(),
                text: "plan the feature".into(),
                mode: Some("plan".into()),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(event_rx.recv().await, Some(HarnessEvent::RunStarted { .. })) {
                    break;
                }
            }
        })
        .await
        .expect("plan turn starts");

        let requests = recorded_requests(&record).await;
        let turn = requests
            .iter()
            .find(|request| request.get("method") == Some(&json!("turn/start")))
            .expect("turn/start recorded");
        assert_eq!(
            turn.pointer("/params/sandboxPolicy/type"),
            Some(&json!("readOnly")),
            "plan turns run under codex's read-only sandbox"
        );
        let text = turn
            .pointer("/params/input/0/text")
            .and_then(Value::as_str)
            .unwrap();
        assert!(text.starts_with("You are in plan mode"), "preamble: {text}");
        assert!(text.contains("plan the feature"), "prompt rides: {text}");
        engine.abort();
    }

    /// ADR 0107: exit_plan_mode outside plan mode is answered in place as a
    /// protocol error — never parked, never surfaced as a pending call.
    #[tokio::test]
    async fn exit_plan_mode_outside_plan_mode_is_rejected_in_place() {
        let tool_call = r#"{"id":77,"method":"item/tool/call","params":{"callId":"call-plan","tool":"exit_plan_mode","arguments":{"plan":"draft plan"},"threadId":"t1","turnId":"turn-1"}}"#;
        let (script, record) = write_fake_codex(&[tool_call]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = parse_tool_manifest(
            r#"[{"name":"exit_plan_mode","description":"Present the plan","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{}}]"#,
        )
        .unwrap();
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
                text: "just build it".into(),
                mode: None,
            })
            .await
            .unwrap();

        let requested = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match event_rx.recv().await {
                    Some(HarnessEvent::ToolCallRequested { .. }) => break true,
                    Some(_) => {}
                    None => break false,
                }
            }
        })
        .await;
        assert!(
            requested.is_err() || !requested.unwrap(),
            "a non-plan exit_plan_mode call must not become a pending tool call"
        );
        let requests = recorded_requests(&record).await;
        let response = requests
            .iter()
            .find(|request| {
                request.get("id") == Some(&json!(77)) && request.get("result").is_some()
            })
            .expect("in-place JSON-RPC response recorded");
        assert_eq!(
            response.pointer("/result/success"),
            Some(&json!(false)),
            "rejected as a protocol error"
        );
        let parked = ParkedCallStore::open(parked_path).unwrap().all();
        assert!(parked.is_empty(), "never parked: {parked:?}");
        engine.abort();
    }

    /// ADR 0107 (session fe3cd981 regression): a REJECT decision must land
    /// as a FAILED call carrying the reviewer's feedback — a success-shaped
    /// response made the model narrate "plan submitted" and end the turn.
    #[tokio::test]
    async fn plan_reject_answers_the_parked_call_as_failure_with_feedback() {
        let tool_call = r#"{"id":77,"method":"item/tool/call","params":{"callId":"call-plan","tool":"exit_plan_mode","arguments":{"plan":"draft"},"threadId":"t1","turnId":"turn-1"}}"#;
        let (script, record) = write_fake_codex(&[tool_call]).await;
        let base = script.parent().unwrap().to_path_buf();
        let mut cli = test_cli(script, base.join("home"));
        cli.thread_id_file = Some(base.join("thread-id"));
        cli.tool_manifest = parse_tool_manifest(
            r#"[{"name":"exit_plan_mode","description":"Present the plan","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{}}]"#,
        )
        .unwrap();
        // Latch plan mode so the call parks instead of being rejected in place.
        engram_harness_sdk::mode_stamp::write_mode_stamp(cli.mode_stamp_file(), "plan").unwrap();
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
                prompt_id: "prompt-plan".into(),
                text: "plan it".into(),
                mode: Some("plan".into()),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(
                    event_rx.recv().await,
                    Some(HarnessEvent::ToolCallRequested { ref call_id, .. }) if call_id == "call-plan"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("plan call parks");

        command_tx
            .send(HarnessCommand::ToolResult {
                call_id: "call-plan".into(),
                result_json: r#"{"decision":"reject","feedback":"Update the README.md to say that tests are needed"}"#.into(),
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let requests = recorded_requests(&record).await;
                if let Some(response) = requests.iter().find(|request| {
                    request.get("id") == Some(&json!(77)) && request.get("result").is_some()
                }) {
                    assert_eq!(
                        response.pointer("/result/success"),
                        Some(&json!(false)),
                        "a rejected plan is a FAILED call: {response}"
                    );
                    let text = response
                        .pointer("/result/contentItems/0/text")
                        .and_then(Value::as_str)
                        .unwrap();
                    assert!(
                        text.contains("Update the README.md"),
                        "feedback rides: {text}"
                    );
                    assert!(
                        text.contains("call exit_plan_mode again"),
                        "revision ask: {text}"
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("reject response recorded");
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
                mode: None,
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
                mode: None,
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
                mode: None,
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
                mode: None,
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
                mode: None,
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
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "CODEX_HOME",
            "ENGRAM_CREDENTIAL_BROKER_TOKEN",
            "ENGRAM_CREDENTIAL_ENDPOINT",
        ] {
            assert!(excluded.iter().any(|value| value == name));
        }
        assert_eq!(params.get("approvalPolicy"), Some(&json!("never")));
        assert_eq!(params.get("sandbox"), Some(&json!("danger-full-access")));
    }

    #[test]
    fn app_server_child_environment_removes_all_credentials() {
        let command = codex_app_server_command(Path::new("codex"), Path::new("/tmp/codex-home"));
        for name in [
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "ENGRAM_CREDENTIAL_BROKER_TOKEN",
            "ENGRAM_CREDENTIAL_ENDPOINT",
        ] {
            assert!(command
                .as_std()
                .get_envs()
                .any(|(key, value)| key == name && value.is_none()));
        }
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
