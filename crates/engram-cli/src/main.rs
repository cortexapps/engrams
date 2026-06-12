//! Ops/admin CLI. Talks to the coordinator's app gRPC surface (ADR 0039)
//! for session/host/image/registry lifecycle operations; invokes the
//! `engram-image-builder` library directly for `image build` (it doesn't
//! go through the coordinator).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format};
use engram_protocol::app::{
    add_registry_request, fleet_service_client::FleetServiceClient,
    image_service_client::ImageServiceClient, session_service_client::SessionServiceClient,
    AddRegistryRequest, CreateSessionRequest, DeleteRegistryRequest, DeleteSessionRequest,
    DisableImageRequest, DrainHostRequest, EnableImageRequest, ExecRequest, FlushSessionRequest,
    GcpWorkloadIdentityRegistryAuth, GetEnableJobRequest, GetHostRequest, GetLogRequest,
    GetSessionRequest, ListEnabledImagesRequest, ListHostsRequest, ListRegistriesRequest,
    ListSessionsRequest, RefreshImageRequest, ResumeRequest, SendPromptRequest, StaticRegistryAuth,
    StreamEventsRequest,
};
use serde_json::Value;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

#[derive(Parser, Debug)]
#[command(name = "engram", version, about = "Engram ops CLI")]
struct Cli {
    /// gRPC address for the coordinator's app surface.
    #[arg(
        long,
        env = "ENGRAM_APP_GRPC",
        default_value = "http://127.0.0.1:50061"
    )]
    grpc_addr: String,

    /// Bearer token for the coordinator's app gRPC surface. When the
    /// coordinator runs without `ENGRAM_APP_GRPC_TOKENS` (dev mode),
    /// leave this unset. Set via env `ENGRAM_APP_TOKEN` — no baked-in
    /// default is intentional.
    #[arg(long, env = "ENGRAM_APP_TOKEN")]
    token: Option<String>,

    /// Print raw JSON instead of the human-readable view. Off by
    /// default so an interactive terminal stays readable; tooling
    /// (jq pipelines, tests) opts in.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Operations on sessions.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Operations on hosts.
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Operations on warm images.
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
    /// Manage Docker registry credentials. Credentials are
    /// envelope-encrypted in Postgres; the password never leaves
    /// the coordinator in plaintext after the initial call.
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    // ADR 0021 P1.5a/P1.7 retired the `Harness` subcommand entirely.
    // Built-in harness publish moved to the `engram-publish-builtin-
    // harness` crate (CI-only); the `/api/harnesses` registry was
    // deleted with the rest of the standalone subsystem.
    /// Admin operations — explicit triggers for primitives whose
    /// production driver is implicit (disk-pressure detector etc.).
    /// Auth-gated behind the same bearer token as the rest of the API.
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AdminCmd {
    /// Force a cold-tier flush of one Idle session's snapshot.
    /// Errors if the session isn't chunk-tracked. Returns the outcome.
    Flush { id: String },
    // `FlushIdle` was retired server-side along with
    // `POST /api/admin/flush-idle` (coordinator admin.rs). The RPC
    // never existed in the gRPC surface (ADR 0039); removed here to
    // match. Use per-session `admin flush <id>` instead.
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// Create a new session. Prints the new session_id on stdout.
    Create {
        /// Image to boot, as a flat OCI URI. Required.
        ///
        /// Example: `--image ghcr.io/cortex/api:warm-2026-04`
        #[arg(long)]
        image: String,
        /// Boot the image as a pure dev VM — leave the baked harness
        /// (if any) resident-but-undriven. Default is to drive the
        /// image's harness (ADR 0021 P1.3 replaced per-session
        /// harness selection: which harness an image runs is now an
        /// image property).
        #[arg(long, default_value_t = false)]
        dev_vm: bool,
        /// Initial prompt for the agent. Only meaningful in the
        /// default (agent) mode against an image that has a baked
        /// harness.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// List sessions in `pending` / `active` / `idle` status.
    List,
    /// Print one session's row.
    Get { id: String },
    /// Run a command synchronously inside the session's sandbox and
    /// print stdout (and stderr if non-empty). Newlines preserved.
    Exec {
        id: String,
        /// Command to run as a shell string (passed to `sh -c`).
        cmd: String,
        /// Optional wall-clock timeout in seconds.
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
    /// Mark the session completed and tear down its sandbox.
    Delete { id: String },
    /// Tail the persistent event log via gRPC StreamEvents. Stays
    /// open; Ctrl-C to stop. `--since N` resumes after a given idx.
    Logs {
        id: String,
        #[arg(long)]
        since: Option<i64>,
    },
    /// Show the conversation timeline (`session_events`).
    Log {
        id: String,
        /// Cap on rows returned.
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Resume an Idle session via its hot snapshot. Dead sessions
    /// can't be resumed (snapshot invalidated).
    Resume { id: String },
    /// Push a prompt to a running session's agent. Auto-resumes
    /// Idle sessions; Gone for Dead.
    Prompt { id: String, text: String },
}

#[derive(Subcommand, Debug)]
enum HostCmd {
    /// List hosts the coordinator knows about.
    List,
    /// Print one host's row + capacity / local-snapshot view.
    Get { id: String },
    /// Flip a host to `draining`. New sessions won't be assigned to
    /// it; in-flight sessions stay (evacuation lands separately).
    Drain { id: String },
}

#[derive(Subcommand, Debug)]
enum RegistryCmd {
    /// Add (or update) a Docker registry credential.
    ///
    /// - `--auth-kind static` (default): paste a username + password
    ///   / PAT / service-account JSON. Sealed under the deployment
    ///   KEK, decrypted on each pull.
    /// - `--auth-kind gcp-workload-identity`: Engram's host-agent
    ///   uses its ambient GCP identity to fetch a short-lived OAuth
    ///   token per pull.
    Add {
        /// Registry host. Examples: `gcr.io`, `ghcr.io`,
        /// `us-east1-docker.pkg.dev`.
        #[arg(long)]
        host: String,
        /// Authentication kind. Defaults to `static`.
        #[arg(long, default_value = "static")]
        auth_kind: String,
        /// Static-only: registry username.
        #[arg(long, required_if_eq("auth_kind", "static"))]
        username: Option<String>,
        /// Static-only: path to a file containing the password / PAT.
        /// Trailing newline is stripped.
        #[arg(long, conflicts_with = "password_stdin")]
        password_file: Option<PathBuf>,
        /// Static-only: read the password from stdin until EOF.
        #[arg(long)]
        password_stdin: bool,
        /// GCP-WI-only: impersonate this service account via IAM
        /// Credentials API. Omit for ambient-identity.
        #[arg(long)]
        impersonate_sa: Option<String>,
    },
    /// List registered registry credentials. Never returns passwords.
    List,
    /// Remove the credential for a registry host.
    Rm {
        /// Registry host to remove (matches `--host` from `add`).
        host: String,
    },
}

// ADR 0021 P1.7 retired `HarnessCmd` entirely.

// `Build` has many optional path fields. Boxing each to silence
// `large_enum_variant` would just move the bytes off the stack without
// gaining anything — clap parses this enum exactly once at startup.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
enum ImageCmd {
    /// List the operator-curated set of enabled images on the coordinator.
    List,
    /// Enable an image: tell the coordinator to fetch and cache the
    /// engram manifest from the given OCI URI. The bytes must already
    /// exist at the URI (push via `engram image build --push <uri>`).
    Enable {
        /// Full OCI URI: `<host>[:port]/<repo>:<tag>`.
        #[arg(long)]
        uri: String,
        /// Don't poll the enable job to completion — print the job id
        /// and return immediately (ADR 0036: enabling is async).
        #[arg(long)]
        no_wait: bool,
    },
    /// Disable an image. Removes the row; the artifact in the
    /// registry is untouched.
    Disable {
        #[arg(long)]
        uri: String,
    },
    /// Re-fetch the manifest layer for an already-enabled image.
    /// Useful when a moved tag (e.g. `:latest`) resolves to a new digest.
    Refresh {
        #[arg(long)]
        uri: String,
    },
    /// Bake an image from a Dockerfile + engram.toml in the source repo.
    Build {
        /// Repo identifier (e.g. `cortex/api`).
        #[arg(long)]
        repo: String,

        /// Path to the source repo. Default `.`.
        #[arg(long, default_value = ".")]
        source: PathBuf,

        /// Override the produced tag. Default `warm-<rfc3339>`.
        #[arg(long)]
        tag: Option<String>,

        /// Where the registry lives.
        #[arg(
            long,
            env = "ENGRAM_IMAGES_DIR",
            default_value = "./var/snapshots/images"
        )]
        images_dir: PathBuf,

        /// Override the docker binary (e.g. `podman`).
        #[arg(long, env = "ENGRAM_DOCKER_BIN")]
        docker_bin: Option<String>,

        /// Output format. `directory` for the dev backend (Process),
        /// `ext4` for Firecracker. Defaults to `directory`.
        #[arg(long, value_parser = parse_image_format, default_value = "directory")]
        format: Format,

        /// Inject a static-musl `engram-agentd` binary into the rootfs.
        #[arg(long)]
        inject_agent: Option<PathBuf>,

        /// Which `engram-transport` impl the in-VM binaries should
        /// select at runtime.
        #[arg(long, value_parser = parse_transport, default_value = "vsock")]
        transport: engram_image_builder::Transport,

        /// Guest platform to resolve a built-in `[harness]` artifact for.
        #[arg(long, value_parser = parse_harness_platform)]
        harness_platform: Option<engram_image_builder::Platform>,

        /// Push the baked image to a Docker registry as an Engram OCI
        /// artifact.
        #[arg(long)]
        push: Option<String>,
    },
}

fn parse_harness_platform(s: &str) -> Result<engram_image_builder::Platform, String> {
    engram_image_builder::Platform::parse(s)
}

fn parse_transport(s: &str) -> Result<engram_image_builder::Transport, String> {
    engram_image_builder::Transport::parse(s)
}

fn parse_image_format(s: &str) -> Result<Format, String> {
    match s {
        "directory" | "dir" => Ok(Format::Directory),
        "ext4" => Ok(Format::Ext4),
        other => Err(format!(
            "unknown format `{other}`; expected `directory` or `ext4`"
        )),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let cli = Cli::parse();

    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Grpc(code, msg)) => {
            eprintln!("engram-cli: gRPC {code}: {msg}");
            ExitCode::from(1)
        }
        Err(CliError::Other(e)) => {
            eprintln!("engram-cli: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
enum CliError {
    Grpc(tonic::Code, String),
    Other(String),
}

impl<E: std::error::Error> From<E> for CliError {
    fn from(e: E) -> Self {
        Self::Other(e.to_string())
    }
}

fn grpc_err(s: tonic::Status) -> CliError {
    CliError::Grpc(s.code(), s.message().to_string())
}

/// Wrap a request body with a bearer token in the gRPC metadata.
fn with_bearer<T>(body: T, token: &Option<String>) -> Request<T> {
    let mut req = Request::new(body);
    if let Some(tok) = token {
        let val: MetadataValue<_> = format!("Bearer {tok}")
            .parse()
            .expect("token bytes are valid ASCII");
        req.metadata_mut().insert("authorization", val);
    }
    req
}

async fn connect(addr: &str) -> Result<Channel, CliError> {
    Channel::from_shared(addr.to_string())
        .map_err(|e| CliError::Other(format!("invalid gRPC address `{addr}`: {e}")))?
        .connect()
        .await
        .map_err(|e| CliError::Other(format!("gRPC connect to `{addr}`: {e}")))
}

async fn run(cli: &Cli) -> Result<(), CliError> {
    // image build never touches the coordinator — skip the channel build.
    if let Cmd::Image {
        cmd: ImageCmd::Build { .. },
    } = &cli.cmd
    {
        return run_image_build(cli).await;
    }

    let channel = connect(&cli.grpc_addr).await?;
    let tok = &cli.token;

    match &cli.cmd {
        Cmd::Session { cmd } => {
            let mut sc = SessionServiceClient::new(channel);
            match cmd {
                SessionCmd::Create {
                    image,
                    dev_vm,
                    prompt,
                } => {
                    session_create(&mut sc, tok, image, *dev_vm, prompt.as_deref(), cli.json).await
                }
                SessionCmd::List => session_list(&mut sc, tok, cli.json).await,
                SessionCmd::Get { id } => session_get(&mut sc, tok, id, cli.json).await,
                SessionCmd::Exec {
                    id,
                    cmd: shell,
                    timeout_secs,
                } => session_exec(&mut sc, tok, id, shell, *timeout_secs).await,
                SessionCmd::Delete { id } => session_delete(&mut sc, tok, id).await,
                SessionCmd::Logs { id, since } => session_logs(&mut sc, tok, id, *since).await,
                SessionCmd::Log { id, limit } => {
                    session_log(&mut sc, tok, id, *limit, cli.json).await
                }
                SessionCmd::Resume { id } => session_resume(&mut sc, tok, id, cli.json).await,
                SessionCmd::Prompt { id, text } => {
                    session_prompt(&mut sc, tok, id, text, cli.json).await
                }
            }
        }
        Cmd::Image { cmd } => {
            let mut ic = ImageServiceClient::new(channel);
            match cmd {
                ImageCmd::List => image_list(&mut ic, tok, cli.json).await,
                ImageCmd::Enable { uri, no_wait } => {
                    image_enable(&mut ic, tok, uri, cli.json, *no_wait).await
                }
                ImageCmd::Disable { uri } => image_disable(&mut ic, tok, uri).await,
                ImageCmd::Refresh { uri } => image_refresh(&mut ic, tok, uri, cli.json).await,
                ImageCmd::Build { .. } => unreachable!("handled above"),
            }
        }
        Cmd::Host { cmd } => {
            let mut fc = FleetServiceClient::new(channel);
            match cmd {
                HostCmd::List => host_list(&mut fc, tok, cli.json).await,
                HostCmd::Get { id } => host_get(&mut fc, tok, id, cli.json).await,
                HostCmd::Drain { id } => host_drain(&mut fc, tok, id).await,
            }
        }
        Cmd::Registry { cmd } => {
            let mut ic = ImageServiceClient::new(channel);
            match cmd {
                RegistryCmd::Add {
                    host,
                    auth_kind,
                    username,
                    password_file,
                    password_stdin,
                    impersonate_sa,
                } => {
                    registry_add(
                        &mut ic,
                        tok,
                        host,
                        auth_kind,
                        username.as_deref(),
                        password_file.as_deref(),
                        *password_stdin,
                        impersonate_sa.as_deref(),
                        cli.json,
                    )
                    .await
                }
                RegistryCmd::List => registry_list(&mut ic, tok, cli.json).await,
                RegistryCmd::Rm { host } => registry_rm(&mut ic, tok, host).await,
            }
        }
        // ADR 0021 P1.7 retired the `Cmd::Harness` arm.
        Cmd::Admin { cmd } => {
            let mut fc = FleetServiceClient::new(channel);
            match cmd {
                AdminCmd::Flush { id } => admin_flush(&mut fc, tok, id, cli.json).await,
            }
        }
    }
}

// ---- admin subcommands -------------------------------------------------

async fn admin_flush(
    fc: &mut FleetServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = fc
        .flush_session(with_bearer(
            FlushSessionRequest {
                session_id: id.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": id,
            "outcome": resp.outcome,
            "manifest_version": resp.manifest_version,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let ver = resp
        .manifest_version
        .map(|v| v.to_string())
        .unwrap_or_else(|| "(none)".to_string());
    println!(
        "flushed {id}: outcome={} manifest_version={ver}",
        resp.outcome
    );
    Ok(())
}

// ---- session subcommands ------------------------------------------------

async fn session_list(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let resp = sc
        .list_sessions(with_bearer(ListSessionsRequest {}, tok))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        // Produce the same JSON shape that scripts parse:
        // {"sessions": [{"id": ..., "status": ..., "image": ..., "mode": ...}]}
        let sessions: Vec<Value> = resp
            .sessions
            .iter()
            .filter_map(|item| item.session.as_ref())
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "status": s.status,
                    "image": s.image,
                    "mode": s.mode,
                    "host_id": s.host_id,
                    "created_at": s.created_at,
                    "last_active_at": s.last_active_at,
                })
            })
            .collect();
        let out = serde_json::json!({ "sessions": sessions });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let sessions: Vec<_> = resp
        .sessions
        .iter()
        .filter_map(|item| item.session.as_ref())
        .collect();
    if sessions.is_empty() {
        println!("(no active sessions)");
        return Ok(());
    }
    println!(
        "{:<36}  {:<10}  {:<10}  {:<24}  BRANCH",
        "ID", "STATUS", "KIND", "REPO"
    );
    for s in sessions {
        println!(
            "{:<36}  {:<10}  {:<10}  {:<24}  -",
            s.id,
            s.status,
            s.mode,
            truncate(&s.image, 24),
        );
    }
    Ok(())
}

async fn session_get(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = sc
        .get_session(with_bearer(
            GetSessionRequest {
                session_id: id.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    let s = resp
        .session
        .ok_or_else(|| CliError::Other("server returned no session".to_string()))?;

    if json {
        let v = serde_json::json!({
            "id": s.id,
            "status": s.status,
            "image": s.image,
            "mode": s.mode,
            "host_id": s.host_id,
            "sandbox_id": s.sandbox_id,
            "created_at": s.created_at,
            "last_active_at": s.last_active_at,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("id              : {}", s.id);
    println!("status          : {}", s.status);
    println!("image           : {}", s.image);
    println!("mode            : {}", s.mode);
    if let Some(h) = &s.host_id {
        println!("host_id         : {h}");
    }
    if let Some(sb) = &s.sandbox_id {
        println!("sandbox_id      : {sb}");
    }
    println!("created_at      : {}", s.created_at);
    println!("last_active     : {}", s.last_active_at);
    Ok(())
}

async fn session_delete(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
) -> Result<(), CliError> {
    sc.delete_session(with_bearer(
        DeleteSessionRequest {
            session_id: id.to_string(),
        },
        tok,
    ))
    .await
    .map_err(grpc_err)?;
    println!("deleted");
    Ok(())
}

async fn session_create(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    image: &str,
    dev_vm: bool,
    prompt: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let mode = if dev_vm { "dev_vm" } else { "agent" }.to_string();
    let resp = sc
        .create_session(with_bearer(
            CreateSessionRequest {
                image_uri: image.to_string(),
                mode,
                prompt: prompt.map(str::to_string),
                harness_secret_id: None,
                secrets: Default::default(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "status": resp.status,
            "kind": resp.kind,
            "image_version": resp.image_version,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{}", resp.session_id);
    }
    Ok(())
}

async fn session_exec(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    cmd: &str,
    timeout_secs: Option<u64>,
) -> Result<(), CliError> {
    use futures::StreamExt;

    let mut stream = sc
        .exec(with_bearer(
            ExecRequest {
                session_id: id.to_string(),
                command: Some(cmd.to_string()),
                argv: vec![],
                env: Default::default(),
                workdir: None,
                timeout_secs,
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    let mut exit_code: Option<i32> = None;
    while let Some(msg) = stream.next().await {
        let msg = msg.map_err(grpc_err)?;
        use engram_protocol::app::exec_output::Event;
        match msg.event {
            Some(Event::Started(_)) => {}
            Some(Event::Stdout(bytes)) => {
                print!("{}", String::from_utf8_lossy(&bytes));
            }
            Some(Event::Stderr(bytes)) => {
                eprint!("{}", String::from_utf8_lossy(&bytes));
            }
            Some(Event::Exit(ex)) => {
                exit_code = ex.exit_status;
            }
            None => {}
        }
    }
    if let Some(code) = exit_code {
        if code != 0 {
            return Err(CliError::Other(format!("exec exited with {code}")));
        }
    }
    Ok(())
}

async fn session_logs(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    since: Option<i64>,
) -> Result<(), CliError> {
    use futures::StreamExt;

    let mut stream = sc
        .stream_events(with_bearer(
            StreamEventsRequest {
                session_id: id.to_string(),
                since,
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    while let Some(msg) = stream.next().await {
        let ev = msg.map_err(grpc_err)?;
        let idx_str = ev
            .idx
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!("[{idx_str:>6}] {}: {}", ev.kind, ev.payload_json);
    }
    Ok(())
}

async fn session_log(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    limit: Option<i64>,
    json: bool,
) -> Result<(), CliError> {
    let resp = sc
        .get_log(with_bearer(
            GetLogRequest {
                session_id: id.to_string(),
                kind: Some("conversation".to_string()),
                limit,
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        let events: Vec<Value> = resp
            .events
            .iter()
            .map(|e| {
                serde_json::json!({
                    "idx": e.idx,
                    "kind": e.kind,
                    "at": e.at,
                    "payload_json": e.payload_json,
                })
            })
            .collect();
        let out = serde_json::json!({ "events": events });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if resp.events.is_empty() {
        println!("(no events)");
        return Ok(());
    }
    println!("{:<6}  {:<28}  {:<25}  PAYLOAD", "IDX", "KIND", "AT");
    for e in &resp.events {
        // payload_json is a raw JSON string; parse for display summary.
        let summary = match serde_json::from_str::<Value>(&e.payload_json) {
            Ok(Value::Object(map)) if !map.is_empty() => {
                let pairs: Vec<String> = map
                    .iter()
                    .take(3)
                    .map(|(k, v)| format!("{k}={}", short_value(v)))
                    .collect();
                pairs.join(" ")
            }
            Ok(Value::Null) | Err(_) => String::new(),
            Ok(other) => short_value(&other),
        };
        println!(
            "{:<6}  {:<28}  {:<25}  {}",
            e.idx,
            truncate(&e.kind, 28),
            e.at,
            truncate(&summary, 80),
        );
    }
    Ok(())
}

fn short_value(v: &Value) -> String {
    match v {
        Value::String(s) => truncate(s, 24),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(a) => format!("[{} items]", a.len()),
        Value::Object(o) => format!("{{{} keys}}", o.len()),
    }
}

async fn session_resume(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = sc
        .resume(with_bearer(
            ResumeRequest {
                session_id: id.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "note": resp.note,
            "snapshot_id": resp.snapshot_id,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let note = if resp.note.is_empty() {
        "resumed".to_string()
    } else {
        resp.note
    };
    println!("{note}");
    Ok(())
}

async fn session_prompt(
    sc: &mut SessionServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    text: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = sc
        .send_prompt(with_bearer(
            SendPromptRequest {
                session_id: id.to_string(),
                text: text.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "note": resp.note,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let note = if resp.note.is_empty() {
            "prompt forwarded".to_string()
        } else {
            resp.note
        };
        println!("{note}");
    }
    Ok(())
}

// ---- host subcommands ---------------------------------------------------

async fn host_list(
    fc: &mut FleetServiceClient<Channel>,
    tok: &Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let resp = fc
        .list_hosts(with_bearer(ListHostsRequest {}, tok))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        // Produce the same JSON shape that scripts parse:
        // {"hosts": [{"id": ..., "hostname": ..., "ready_image_digests": [...], ...}]}
        let hosts: Vec<Value> = resp
            .hosts
            .iter()
            .map(|h| {
                serde_json::json!({
                    "id": h.id,
                    "hostname": h.hostname,
                    "status": h.status,
                    "capacity_used_mib": h.capacity_used_mib,
                    "capacity_total_mib": h.capacity_total_mib,
                    "running_sandboxes": h.running_sandboxes,
                    "local_snapshots": h.local_snapshots,
                    "ready_images": h.ready_images,
                    "ready_image_digests": h.ready_image_digests,
                    "last_heartbeat_at": h.last_heartbeat_at,
                })
            })
            .collect();
        let out = serde_json::json!({ "hosts": hosts });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if resp.hosts.is_empty() {
        println!("(no hosts registered)");
        return Ok(());
    }
    println!(
        "{:<36}  {:<10}  {:<10}  {:<10}  SNAPSHOTS",
        "ID", "STATUS", "USED_MIB", "TOTAL_MIB"
    );
    for h in &resp.hosts {
        println!(
            "{:<36}  {:<10}  {:<10}  {:<10}  {}",
            h.id, h.status, h.capacity_used_mib, h.capacity_total_mib, h.local_snapshots,
        );
    }
    Ok(())
}

async fn host_get(
    fc: &mut FleetServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = fc
        .get_host(with_bearer(
            GetHostRequest {
                host_id: id.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    let h = resp
        .host
        .ok_or_else(|| CliError::Other("server returned no host".to_string()))?;

    if json {
        let v = serde_json::json!({
            "id": h.id,
            "hostname": h.hostname,
            "status": h.status,
            "capacity_used_mib": h.capacity_used_mib,
            "capacity_total_mib": h.capacity_total_mib,
            "running_sandboxes": h.running_sandboxes,
            "local_snapshots": h.local_snapshots,
            "ready_images": h.ready_images,
            "ready_image_digests": h.ready_image_digests,
            "last_heartbeat_at": h.last_heartbeat_at,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("id              : {}", h.id);
    println!("hostname        : {}", h.hostname);
    println!("status          : {}", h.status);
    println!("capacity_used   : {} MiB", h.capacity_used_mib);
    println!("capacity_total  : {} MiB", h.capacity_total_mib);
    println!("running_sandboxes: {}", h.running_sandboxes);
    println!("local_snapshots : {}", h.local_snapshots);
    Ok(())
}

async fn host_drain(
    fc: &mut FleetServiceClient<Channel>,
    tok: &Option<String>,
    id: &str,
) -> Result<(), CliError> {
    fc.drain_host(with_bearer(
        DrainHostRequest {
            host_id: id.to_string(),
        },
        tok,
    ))
    .await
    .map_err(grpc_err)?;
    println!("draining");
    Ok(())
}

// ---- registry subcommands ---------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn registry_add(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    host: &str,
    auth_kind: &str,
    username: Option<&str>,
    password_file: Option<&std::path::Path>,
    password_stdin: bool,
    impersonate_sa: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let auth = match auth_kind {
        "static" => {
            let username = username
                .ok_or_else(|| CliError::Other("--auth-kind static requires --username".into()))?
                .to_string();
            let password = match (password_file, password_stdin) {
                (Some(path), false) => {
                    let raw = std::fs::read_to_string(path)
                        .map_err(|e| CliError::Other(format!("read {}: {e}", path.display())))?;
                    raw.strip_suffix('\n').unwrap_or(&raw).to_string()
                }
                (None, true) => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .map_err(|e| CliError::Other(format!("stdin: {e}")))?;
                    buf.strip_suffix('\n').unwrap_or(&buf).to_string()
                }
                (None, false) => {
                    return Err(CliError::Other(
                        "static auth requires --password-file <path> or --password-stdin".into(),
                    ));
                }
                (Some(_), true) => unreachable!("clap conflicts_with"),
            };
            if password.is_empty() {
                return Err(CliError::Other("password is empty".into()));
            }
            add_registry_request::Auth::Static(StaticRegistryAuth { username, password })
        }
        "gcp-workload-identity" | "gcp_workload_identity" => {
            add_registry_request::Auth::GcpWorkloadIdentity(GcpWorkloadIdentityRegistryAuth {
                impersonate_sa: impersonate_sa.map(str::to_string),
            })
        }
        other => {
            return Err(CliError::Other(format!(
                "unknown --auth-kind `{other}` (expected: static | gcp-workload-identity)"
            )));
        }
    };

    let resp = ic
        .add_registry(with_bearer(
            AddRegistryRequest {
                host: host.to_string(),
                auth: Some(auth),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        let v = serde_json::json!({
            "id": resp.id,
            "host": resp.host,
            "auth_kind": resp.auth_kind,
            "auth_principal": resp.auth_principal,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "added: host={} auth_kind={} principal={}",
            resp.host,
            resp.auth_kind,
            resp.auth_principal.as_deref().unwrap_or("(none)"),
        );
    }
    Ok(())
}

async fn registry_list(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let resp = ic
        .list_registries(with_bearer(ListRegistriesRequest {}, tok))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        let regs: Vec<Value> = resp
            .registries
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "registry_host": r.registry_host,
                    "auth_kind": r.auth_kind,
                    "auth_principal": r.auth_principal,
                    "created_at": r.created_at,
                })
            })
            .collect();
        let out = serde_json::json!({ "registries": regs });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if resp.registries.is_empty() {
        println!("(no registries configured)");
        return Ok(());
    }
    println!("{:<40}  {:<24}  PRINCIPAL", "HOST", "AUTH_KIND");
    for r in &resp.registries {
        println!(
            "{:<40}  {:<24}  {}",
            r.registry_host,
            r.auth_kind,
            r.auth_principal.as_deref().unwrap_or("(none)"),
        );
    }
    Ok(())
}

async fn registry_rm(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    host: &str,
) -> Result<(), CliError> {
    ic.delete_registry(with_bearer(
        DeleteRegistryRequest {
            host: host.to_string(),
        },
        tok,
    ))
    .await
    .map_err(grpc_err)?;
    println!("removed");
    Ok(())
}

// ADR 0021 P1.5a + P1.7 retired the entire `engram harness ...`
// surface — the publish primitive moved to its own CI-only crate
// (`engram-publish-builtin-harness`); the registry-CRUD subcommands
// (add/list/rm) went with the `/api/harnesses` endpoint.

// ---- enabled-images subcommands ---------------------------------------

async fn image_list(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let resp = ic
        .list_enabled_images(with_bearer(ListEnabledImagesRequest {}, tok))
        .await
        .map_err(grpc_err)?
        .into_inner();

    if json {
        // Produce the same JSON shape that scripts parse:
        // {"images": [{"image_uri": ..., "manifest_digest": ..., ...}]}
        let images: Vec<Value> = resp
            .images
            .iter()
            .map(|img| {
                serde_json::json!({
                    "id": img.id,
                    "image_uri": img.image_uri,
                    "manifest_digest": img.manifest_digest,
                    "manifest_name": img.manifest_name,
                    "manifest_description": img.manifest_description,
                    "harness_name": img.harness_name,
                    "last_refreshed_at": img.last_refreshed_at,
                    "created_at": img.created_at,
                })
            })
            .collect();
        let out = serde_json::json!({ "images": images });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if resp.images.is_empty() {
        println!("(no images enabled — `engram image enable --uri <uri>` to add one)");
        return Ok(());
    }
    println!("{:<48}  {:<22}  DIGEST", "URI", "NAME");
    for img in &resp.images {
        println!(
            "{:<48}  {:<22}  {}",
            img.image_uri,
            img.manifest_name.as_deref().unwrap_or("(unparsed)"),
            img.manifest_digest,
        );
    }
    Ok(())
}

async fn image_enable(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    uri: &str,
    json: bool,
    no_wait: bool,
) -> Result<(), CliError> {
    // ADR 0036: enabling is async — returns an enable job.
    let resp = ic
        .enable_image(with_bearer(
            EnableImageRequest {
                image_uri: uri.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();

    let job = resp
        .job
        .ok_or_else(|| CliError::Other("server returned no enable job".to_string()))?;

    if no_wait {
        if json {
            let v = enable_job_to_json(&job);
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("enable job {} accepted (state={})", job.id, job.state);
        }
        return Ok(());
    }
    poll_enable_job(ic, tok, &job.id, json).await
}

fn enable_job_to_json(job: &engram_protocol::app::EnableJob) -> Value {
    serde_json::json!({
        "id": job.id,
        "image_uri": job.image_uri,
        "manifest_digest": job.manifest_digest,
        "state": job.state,
        "chunks_total": job.chunks_total,
        "chunks_done": job.chunks_done,
        "error": job.error,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    })
}

/// Poll an enable job until `ready`/`failed`, rendering progress.
async fn poll_enable_job(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    job_id: &str,
    json: bool,
) -> Result<(), CliError> {
    let mut last_line_len = 0usize;
    loop {
        let resp = ic
            .get_enable_job(with_bearer(
                GetEnableJobRequest {
                    job_id: job_id.to_string(),
                },
                tok,
            ))
            .await
            .map_err(grpc_err)?
            .into_inner();
        let job = resp
            .job
            .ok_or_else(|| CliError::Other("server returned no job".to_string()))?;
        match job.state.as_str() {
            "ready" => {
                if last_line_len > 0 {
                    eprintln!();
                }
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&enable_job_to_json(&job))?
                    );
                } else {
                    println!(
                        "enabled: uri={} digest={}",
                        job.image_uri,
                        job.manifest_digest.as_deref().unwrap_or(""),
                    );
                }
                return Ok(());
            }
            "failed" => {
                if last_line_len > 0 {
                    eprintln!();
                }
                let err = job.error.as_deref().unwrap_or("unknown error");
                return Err(CliError::Other(format!(
                    "enable job {job_id} failed: {err}"
                )));
            }
            _ => {
                let progress = match job.chunks_total {
                    Some(t) if t > 0 => format!("{}/{t} chunks", job.chunks_done),
                    _ => String::new(),
                };
                let line = format!("\r{:<14} {:<24}", job.state, progress);
                eprint!("{line}");
                last_line_len = line.len();
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

async fn image_disable(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    uri: &str,
) -> Result<(), CliError> {
    ic.disable_image(with_bearer(
        DisableImageRequest {
            image_uri: uri.to_string(),
        },
        tok,
    ))
    .await
    .map_err(grpc_err)?;
    println!("disabled");
    Ok(())
}

async fn image_refresh(
    ic: &mut ImageServiceClient<Channel>,
    tok: &Option<String>,
    uri: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = ic
        .refresh_image(with_bearer(
            RefreshImageRequest {
                image_uri: uri.to_string(),
            },
            tok,
        ))
        .await
        .map_err(grpc_err)?
        .into_inner();
    let job = resp
        .job
        .ok_or_else(|| CliError::Other("server returned no refresh job".to_string()))?;
    // ADR 0036: refresh is an enable job too — poll it like enable.
    poll_enable_job(ic, tok, &job.id, json).await
}

// ---- image build (no coordinator contact) ----------------------------

async fn run_image_build(cli: &Cli) -> Result<(), CliError> {
    let Cmd::Image {
        cmd:
            ImageCmd::Build {
                repo,
                source,
                tag,
                images_dir,
                docker_bin,
                format,
                inject_agent,
                transport,
                harness_platform,
                push,
            },
    } = &cli.cmd
    else {
        unreachable!()
    };
    image_build(
        repo,
        source,
        tag.as_deref(),
        images_dir,
        docker_bin.as_deref(),
        *format,
        inject_agent.as_deref(),
        *transport,
        *harness_platform,
        push.as_deref(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn image_build(
    repo: &str,
    source: &Path,
    tag: Option<&str>,
    images_dir: &Path,
    docker_bin: Option<&str>,
    format: Format,
    inject_agent: Option<&Path>,
    transport: engram_image_builder::Transport,
    harness_platform: Option<engram_image_builder::Platform>,
    push: Option<&str>,
) -> Result<(), CliError> {
    let resolved_tag = tag
        .map(str::to_string)
        .unwrap_or_else(|| format!("warm-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));
    let agent_injection = inject_agent.map(|p| engram_image_builder::AgentInjection {
        agent_binary: p.to_path_buf(),
        vsock_port: 1024,
        transport,
        init_script: None,
    });
    let req = BuildRequest {
        source: source.to_path_buf(),
        repo: repo.to_string(),
        tag: resolved_tag.clone(),
        images_dir: images_dir.to_path_buf(),
        format,
        agent_injection,
    };
    let docker = match docker_bin {
        Some(bin) => DockerCli::with_binary(bin.to_string()),
        None => DockerCli::new(),
    };
    let chunk_root = images_dir.join("store");
    tokio::fs::create_dir_all(&chunk_root)
        .await
        .map_err(|e| CliError::Other(format!("chunk store root: {e}")))?;
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> =
        std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(chunk_root));
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let oci =
        engram_oci::OciClient::new(std::sync::Arc::new(engram_oci::DockerConfigResolver::new()));
    let mut builder = Builder::new(docker, chunk_store).with_oci(oci);
    if let Some(platform) = harness_platform {
        builder = builder.with_harness_platform(platform);
    }
    let outcome = builder
        .build(&req)
        .await
        .map_err(|e| CliError::Other(e.to_string()))?;
    println!(
        "{repo} {tag} -> {dir} ({size} bytes)",
        repo = repo,
        tag = resolved_tag,
        dir = outcome.image_dir.display(),
        size = outcome.size_bytes,
    );

    if let Some(target) = push {
        let push = builder
            .push_to_registry(&req, &outcome, target)
            .await
            .map_err(|e| CliError::Other(format!("push: {e}")))?;
        println!(
            "✓ pushed {uri} (digest {digest})",
            uri = push.uri,
            digest = push.manifest_digest.as_str(),
        );
    }

    Ok(())
}

// ---- shared output helpers ---------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_passes_short_strings_through() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn truncate_appends_ellipsis_when_too_long() {
        // Counts characters, not bytes. "abcdef" with max=4 yields
        // "abc…" (3 chars + ellipsis = 4 visible columns). Avoids the
        // classic UTF-8 boundary bug from byte-based truncation.
        assert_eq!(truncate("abcdef", 4), "abc…");
    }

    #[test]
    fn truncate_handles_multibyte_chars() {
        // Each emoji is one char; max=2 should leave one + ellipsis.
        let s = "🦀🦀🦀";
        assert_eq!(truncate(s, 2), "🦀…");
    }

    #[test]
    fn short_value_renders_scalar_payload_kinds() {
        assert_eq!(short_value(&Value::String("hello".into())), "hello");
        assert_eq!(short_value(&Value::Bool(true)), "true");
        assert_eq!(short_value(&Value::Null), "null");
        // Long strings get truncated so a single payload field can't
        // blow up the terminal column.
        let s = "a".repeat(40);
        assert!(short_value(&Value::String(s)).chars().count() <= 24);
    }

    #[test]
    fn short_value_summarizes_compound_payloads() {
        let arr = Value::Array(vec![Value::Null, Value::Null, Value::Null]);
        assert_eq!(short_value(&arr), "[3 items]");
        let obj = Value::Object(serde_json::Map::from_iter([
            ("a".into(), Value::Null),
            ("b".into(), Value::Null),
        ]));
        assert_eq!(short_value(&obj), "{2 keys}");
    }
}
