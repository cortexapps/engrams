//! Ops/admin CLI. Talks to the coordinator's HTTP API for session
//! lifecycle operations; invokes the `engram-image-builder` library
//! directly for `image build` (it doesn't go through the coordinator).
//! Implementations land progressively as the surface grows.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format};
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "engram", version, about = "Engram ops CLI")]
struct Cli {
    #[arg(long, env = "ENGRAM_ENDPOINT", default_value = "http://localhost:8080")]
    endpoint: String,

    /// Bearer token for the coordinator's protected endpoints. When
    /// the coordinator runs without `ENGRAM_AUTH_TOKENS` (dev mode),
    /// leave this unset.
    #[arg(long, env = "ENGRAM_TOKEN")]
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
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// Create a new session. Prints the new session_id on stdout.
    ///
    /// Three orthogonal axes shape a session: `image`, `workspace`,
    /// `harness`. The flags below set each one independently —
    /// matching the `POST /sessions` wire shape exactly.
    Create {
        /// Image to boot, in `repo:tag` form. Required.
        ///
        /// Example: `--image cortex/api:warm-2026-04`
        #[arg(long)]
        image: String,
        /// Clone the given git URL into the workspace. Omit for an
        /// `Empty` workspace ("just a VM and a shell").
        ///
        /// Example: `--git https://github.com/cortex/api.git`
        #[arg(long)]
        git: Option<String>,
        /// Branch to check out for `--git` workspaces. Ignored
        /// otherwise.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Make the workspace read-only — for `--git` this also
        /// suppresses the per-session checkpoint branch.
        #[arg(long)]
        read_only: bool,
        /// Which baked-in harness to attach. `none` (default) boots
        /// the VM with no agent; otherwise the value is the manifest
        /// `[[harness]] name = ...` to attach (e.g. `claude`, `noop`).
        #[arg(long, default_value = "none")]
        harness: String,
        /// Initial prompt for the agent. Only meaningful when
        /// `--harness` is not `none`; the API rejects with 400 if a
        /// prompt is supplied alongside `--harness none`.
        #[arg(long)]
        prompt: Option<String>,
        /// Free-form user identifier surfaced on the row.
        #[arg(long)]
        user_id: Option<String>,
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
    /// Tail the persistent event log via SSE. Stays open; Ctrl-C to
    /// stop. `--since N` resumes after a given idx (matches
    /// `?since=N` on the events endpoint).
    Logs {
        id: String,
        #[arg(long)]
        since: Option<i64>,
    },
    /// Show the conversation timeline (`session_events`) or the
    /// workspace `git log` for a session's checkpoint branch.
    Log {
        id: String,
        /// `conversation` (default) or `workspace`.
        #[arg(long, default_value = "conversation")]
        kind: String,
        /// Cap on rows / commits returned.
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Show the workspace diff between the session's checkpoint
    /// branch and `--vs` (defaults to the session's base branch).
    Diff {
        id: String,
        #[arg(long)]
        vs: Option<String>,
    },
    /// Fork a session at HEAD or at `--at <event_idx>`. Creates a
    /// new session row whose checkpoint branch starts at the
    /// referenced workspace commit.
    Fork {
        id: String,
        /// Fork point in the source's `session_events` log.
        /// Defaults to "fork from current HEAD".
        #[arg(long)]
        at: Option<i64>,
        #[arg(long)]
        title: Option<String>,
    },
    /// Resume an Idle session via its FC snapshot. Dead sessions
    /// can't be resumed (snapshot invalidated) — use
    /// `engram session fork <id>` to continue from the workspace.
    Resume { id: String },
    /// Force a checkpoint flush on a Git session — Postgres event
    /// + git commit + push to `engram/sessions/<id>`.
    Checkpoint { id: String },
    /// Push a prompt to a running session's agent. Auto-resumes
    /// Idle sessions; 410 Gone for Dead.
    Prompt { id: String, text: String },
}

#[derive(Subcommand, Debug)]
enum HostCmd {
    /// List hosts the coordinator knows about (Postgres rows + live
    /// scheduler view).
    List,
    /// Print one host's row + capacity / warm-pool view.
    Get { id: String },
    /// Flip a host to `draining`. New sessions won't be assigned to
    /// it; in-flight sessions stay (evacuation lands in 3d).
    Drain { id: String },
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    List {
        repo: String,
    },
    /// Bake an image from a Dockerfile + engram.toml in the source repo.
    Build {
        /// Repo identifier (e.g. `cortex/api`). Determines the registry
        /// path the produced image lands at.
        #[arg(long)]
        repo: String,

        /// Path to the source repo. Default `.`.
        #[arg(long, default_value = ".")]
        source: PathBuf,

        /// Override the produced tag. Default `warm-<rfc3339>`.
        #[arg(long)]
        tag: Option<String>,

        /// Where the registry lives — must match the coordinator's
        /// `<storage_local_path>/images`.
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

        /// Inject a static-musl `engram-agentd` binary into the
        /// rootfs at `/sbin/engram-agentd` and write a `/sbin/engram-init`
        /// shim that exec's it on the reserved vsock port (1024).
        /// Required for Firecracker images; without it the host
        /// can't `exec()` against the VM. Pre-build the binary with
        /// `cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release`.
        #[arg(long)]
        inject_agent: Option<PathBuf>,

        /// Inject a static-musl `engram-bootstrap` binary at
        /// `/sbin/engram-bootstrap`. The default init shim spawns
        /// it in the background; it listens on vsock 1025 for a
        /// `BootstrapLaunch` from the host and exec's the per-
        /// session harness over the virtio-fs mount at
        /// `/run/engram/harnesses/`. Harness binaries themselves
        /// are no longer baked into images — they live host-side
        /// in `cfg.harnesses_dir`.
        #[arg(long)]
        inject_bootstrap: Option<PathBuf>,

        /// Which `engram-transport` impl the in-VM binaries should
        /// select at runtime. The init shim writes
        /// `ENGRAM_TRANSPORT=<value>` into the rootfs.
        ///
        /// `vsock` (default): AF_VSOCK on Linux, used by the
        /// Firecracker production path. Requires
        /// `CONFIG_VIRTIO_VSOCKETS=y` in the guest kernel.
        ///
        /// `console`: virtio-console on Apple Virtualization.framework,
        /// used by the vz-bake-* recipes. Universally available in
        /// every Linux kernel.
        #[arg(long, value_parser = parse_transport, default_value = "vsock")]
        transport: engram_image_builder::Transport,
    },
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
        Err(CliError::NotImplemented(what)) => {
            eprintln!("engram-cli: `{what}` is not implemented yet.");
            ExitCode::from(2)
        }
        Err(CliError::Http(status, body)) => {
            eprintln!("engram-cli: server returned {status}: {body}");
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
    NotImplemented(String),
    Http(u16, String),
    Other(String),
}

impl<E: std::error::Error> From<E> for CliError {
    fn from(e: E) -> Self {
        Self::Other(e.to_string())
    }
}

async fn run(cli: &Cli) -> Result<(), CliError> {
    let client = build_client(cli.token.as_deref())?;
    match &cli.cmd {
        Cmd::Session { cmd } => match cmd {
            SessionCmd::Create {
                image,
                git,
                branch,
                read_only,
                harness,
                prompt,
                user_id,
            } => {
                session_create(
                    &client,
                    &cli.endpoint,
                    image,
                    git.as_deref(),
                    branch,
                    *read_only,
                    harness,
                    prompt.as_deref(),
                    user_id.as_deref(),
                    cli.json,
                )
                .await
            }
            SessionCmd::List => session_list(&client, &cli.endpoint, cli.json).await,
            SessionCmd::Get { id } => session_get(&client, &cli.endpoint, id, cli.json).await,
            SessionCmd::Exec {
                id,
                cmd: shell,
                timeout_secs,
            } => session_exec(&client, &cli.endpoint, id, shell, *timeout_secs, cli.json).await,
            SessionCmd::Delete { id } => session_delete(&client, &cli.endpoint, id).await,
            SessionCmd::Logs { id, since } => {
                session_logs(&client, &cli.endpoint, id, *since).await
            }
            SessionCmd::Log { id, kind, limit } => {
                session_log(&client, &cli.endpoint, id, kind, *limit, cli.json).await
            }
            SessionCmd::Diff { id, vs } => {
                session_diff(&client, &cli.endpoint, id, vs.as_deref()).await
            }
            SessionCmd::Fork { id, at, title } => {
                session_fork(&client, &cli.endpoint, id, *at, title.as_deref(), cli.json).await
            }
            SessionCmd::Resume { id } => session_resume(&client, &cli.endpoint, id, cli.json).await,
            SessionCmd::Checkpoint { id } => {
                session_checkpoint(&client, &cli.endpoint, id, cli.json).await
            }
            SessionCmd::Prompt { id, text } => {
                session_prompt(&client, &cli.endpoint, id, text, cli.json).await
            }
        },
        Cmd::Image { cmd } => match cmd {
            ImageCmd::Build {
                repo,
                source,
                tag,
                images_dir,
                docker_bin,
                format,
                inject_agent,
                inject_bootstrap,
                transport,
            } => {
                image_build(
                    repo,
                    source,
                    tag.as_deref(),
                    images_dir,
                    docker_bin.as_deref(),
                    *format,
                    inject_agent.as_deref(),
                    inject_bootstrap.as_deref(),
                    *transport,
                )
                .await
            }
            ImageCmd::List { .. } => Err(CliError::NotImplemented(
                "image list — coordinator endpoint not yet exposed".into(),
            )),
        },
        Cmd::Host { cmd } => match cmd {
            HostCmd::List => host_list(&client, &cli.endpoint, cli.json).await,
            HostCmd::Get { id } => host_get(&client, &cli.endpoint, id, cli.json).await,
            HostCmd::Drain { id } => host_drain(&client, &cli.endpoint, id).await,
        },
    }
}

// ---- session subcommands ------------------------------------------------

async fn session_list(
    client: &reqwest::Client,
    endpoint: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/sessions")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let empty = Vec::new();
    let sessions = body["sessions"].as_array().unwrap_or(&empty);
    if sessions.is_empty() {
        println!("(no active sessions)");
        return Ok(());
    }
    // Plain columnar layout: header + rows. Avoids pulling a TUI dep
    // for what's effectively two-line output most of the time.
    println!(
        "{:<36}  {:<10}  {:<10}  {:<24}  BRANCH",
        "ID", "STATUS", "KIND", "REPO"
    );
    for s in sessions {
        println!(
            "{:<36}  {:<10}  {:<10}  {:<24}  {}",
            s["id"].as_str().unwrap_or(""),
            s["status"].as_str().unwrap_or(""),
            s["session_kind"].as_str().unwrap_or(""),
            truncate(s["repo"].as_str().unwrap_or(""), 24),
            s["branch"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn session_get(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/sessions/{id}")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    println!("id              : {}", body["id"].as_str().unwrap_or(""));
    println!(
        "status          : {}",
        body["status"].as_str().unwrap_or("")
    );
    println!(
        "session_kind    : {}",
        body["session_kind"].as_str().unwrap_or("")
    );
    println!("repo            : {}", body["repo"].as_str().unwrap_or(""));
    println!(
        "branch          : {}",
        body["branch"].as_str().unwrap_or("")
    );
    if let Some(b) = body["checkpoint_branch"].as_str() {
        println!("checkpoint_branch: {b}");
    }
    println!(
        "image_version   : {}",
        body["image_version"].as_str().unwrap_or(""),
    );
    if let Some(uid) = body["user_id"].as_str() {
        println!("user_id         : {uid}");
    }
    println!(
        "created_at      : {}",
        body["created_at"].as_str().unwrap_or(""),
    );
    println!(
        "last_active     : {}",
        body["last_active_at"].as_str().unwrap_or(""),
    );
    Ok(())
}

async fn session_delete(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
) -> Result<(), CliError> {
    let resp = client
        .delete(format!("{endpoint}/sessions/{id}"))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }
    println!("deleted");
    Ok(())
}

/// Split `--image cortex/api:warm-1` on the *last* colon so repo
/// names containing `:` (rare but legal in registry URLs) don't get
/// truncated. Returns `(repo, tag)`.
fn parse_image_ref(spec: &str) -> Result<(String, String), CliError> {
    match spec.rsplit_once(':') {
        Some((repo, tag)) if !repo.is_empty() && !tag.is_empty() => {
            Ok((repo.to_string(), tag.to_string()))
        }
        _ => Err(CliError::Other(format!(
            "invalid --image `{spec}` — expected `<repo>:<tag>`"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn session_create(
    client: &reqwest::Client,
    endpoint: &str,
    image: &str,
    git: Option<&str>,
    branch: &str,
    read_only: bool,
    harness: &str,
    prompt: Option<&str>,
    user_id: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let (image_repo, image_tag) = parse_image_ref(image)?;
    let workspace_value = match git {
        Some(url) if !url.is_empty() => serde_json::json!({
            "kind": "git",
            "url": url,
            "branch": branch,
            "read_only": read_only,
        }),
        _ => serde_json::json!({"kind": "empty"}),
    };
    let harness_value = match harness {
        "none" => serde_json::json!({"kind": "none"}),
        name => serde_json::json!({"kind": "builtin", "name": name}),
    };
    let mut payload = serde_json::Map::new();
    payload.insert(
        "image".into(),
        serde_json::json!({
            "kind": "registry",
            "repo": image_repo,
            "tag": image_tag,
        }),
    );
    payload.insert("workspace".into(), workspace_value);
    payload.insert("harness".into(), harness_value);
    if let Some(u) = user_id {
        payload.insert("user_id".into(), Value::from(u));
    }
    if let Some(p) = prompt {
        payload.insert("prompt".into(), Value::from(p));
    }
    let resp = client
        .post(format!("{endpoint}/sessions"))
        .json(&Value::Object(payload))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
    } else {
        println!("{}", parsed["session_id"].as_str().unwrap_or(""));
    }
    Ok(())
}

async fn session_exec(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    cmd: &str,
    timeout_secs: Option<u64>,
    json: bool,
) -> Result<(), CliError> {
    let mut payload = serde_json::Map::new();
    payload.insert("command".into(), Value::from(cmd));
    if let Some(t) = timeout_secs {
        payload.insert("timeout_secs".into(), Value::from(t));
    }
    let resp = client
        .post(format!("{endpoint}/sessions/{id}/exec"))
        .json(&Value::Object(payload))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
        return Ok(());
    }
    // Plain mode: stdout to stdout, stderr to stderr. Exit code
    // mirrors the remote process's exit so shell pipelines compose.
    if let Some(s) = parsed["stdout"].as_str() {
        if !s.is_empty() {
            print!("{s}");
        }
    }
    if let Some(s) = parsed["stderr"].as_str() {
        if !s.is_empty() {
            eprint!("{s}");
        }
    }
    if let Some(exit) = parsed["exit_status"].as_i64() {
        if exit != 0 {
            return Err(CliError::Other(format!("exec exited with {exit}")));
        }
    }
    Ok(())
}

// ---- host subcommands ---------------------------------------------------

async fn host_list(client: &reqwest::Client, endpoint: &str, json: bool) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/api/hosts")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let empty = Vec::new();
    let hosts = body["hosts"].as_array().unwrap_or(&empty);
    if hosts.is_empty() {
        println!("(no hosts registered)");
        return Ok(());
    }
    println!(
        "{:<36}  {:<10}  {:<10}  {:<10}  WARM_POOLS",
        "ID", "STATUS", "USED_MIB", "TOTAL_MIB"
    );
    for h in hosts {
        let warm_count = h["warm_pools"].as_array().map(|v| v.len()).unwrap_or(0);
        println!(
            "{:<36}  {:<10}  {:<10}  {:<10}  {}",
            h["id"].as_str().unwrap_or(""),
            h["status"].as_str().unwrap_or(""),
            h["capacity_used_mib"].as_u64().unwrap_or(0),
            h["capacity_total_mib"].as_u64().unwrap_or(0),
            warm_count,
        );
    }
    Ok(())
}

async fn host_get(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/api/hosts/{id}")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    println!("id              : {}", body["id"].as_str().unwrap_or(""));
    println!(
        "hostname        : {}",
        body["hostname"].as_str().unwrap_or("")
    );
    println!(
        "status          : {}",
        body["status"].as_str().unwrap_or("")
    );
    println!(
        "capacity_used   : {} MiB",
        body["capacity_used_mib"].as_u64().unwrap_or(0)
    );
    println!(
        "capacity_total  : {} MiB",
        body["capacity_total_mib"].as_u64().unwrap_or(0)
    );
    println!(
        "running_sandboxes: {}",
        body["running_sandboxes"].as_u64().unwrap_or(0)
    );
    println!(
        "local_snapshots : {}",
        body["local_snapshots"].as_u64().unwrap_or(0)
    );
    if let Some(pools) = body["warm_pools"].as_array() {
        println!("warm_pools      :");
        for p in pools {
            println!(
                "  - {repo} {tag}: {ready}/{target}",
                repo = p["repo"].as_str().unwrap_or(""),
                tag = p["image_version"].as_str().unwrap_or(""),
                ready = p["ready"].as_u64().unwrap_or(0),
                target = p["target"].as_u64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

async fn host_drain(client: &reqwest::Client, endpoint: &str, id: &str) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/api/hosts/{id}/drain"))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }
    println!("draining");
    Ok(())
}

async fn session_logs(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    since: Option<i64>,
) -> Result<(), CliError> {
    use futures::StreamExt;

    let mut url = format!("{endpoint}/sessions/{id}/events");
    if let Some(s) = since {
        url.push_str(&format!("?since={s}"));
    }
    let resp = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }

    // Minimal SSE parser: split on "\n\n" frame boundaries; print
    // `event:` + `data:` per frame. Server-sent comments (`:`-prefix
    // keep-alives) are ignored. We rely on the body being closed
    // upstream (or the user Ctrl-C'ing) to end the stream.
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(idx) = buf.find("\n\n") {
            let frame = buf[..idx].to_string();
            buf.drain(..idx + 2);
            print_sse_frame(&frame);
        }
    }
    Ok(())
}

async fn session_log(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    kind: &str,
    limit: Option<i64>,
    json: bool,
) -> Result<(), CliError> {
    let mut url = format!("{endpoint}/sessions/{id}/log?kind={kind}");
    if let Some(l) = limit {
        url.push_str(&format!("&limit={l}"));
    }
    let body = get_json(client, &url).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let kind_str = body["kind"].as_str().unwrap_or("");
    if kind_str == "conversation" {
        let empty = Vec::new();
        let events = body["events"].as_array().unwrap_or(&empty);
        if events.is_empty() {
            println!("(no events)");
            return Ok(());
        }
        println!("{:<6}  {:<28}  {:<25}  PAYLOAD", "IDX", "KIND", "AT");
        for e in events {
            let payload = e["payload"].clone();
            let summary = match payload {
                Value::Object(map) if !map.is_empty() => {
                    let pairs: Vec<String> = map
                        .iter()
                        .take(3)
                        .map(|(k, v)| format!("{k}={}", short_value(v)))
                        .collect();
                    pairs.join(" ")
                }
                Value::Null => String::new(),
                other => short_value(&other),
            };
            println!(
                "{:<6}  {:<28}  {:<25}  {}",
                e["idx"].as_i64().unwrap_or(0),
                truncate(e["kind"].as_str().unwrap_or(""), 28),
                e["at"].as_str().unwrap_or(""),
                truncate(&summary, 80),
            );
        }
    } else {
        // workspace: list of commits
        let empty = Vec::new();
        let commits = body["commits"].as_array().unwrap_or(&empty);
        if commits.is_empty() {
            println!("(no checkpoint commits)");
            return Ok(());
        }
        for c in commits {
            let sha = c["sha"].as_str().unwrap_or("");
            let short: String = sha.chars().take(10).collect();
            println!(
                "{short}  {date}  {author}  {message}",
                date = c["date"].as_str().unwrap_or(""),
                author = truncate(c["author"].as_str().unwrap_or(""), 24),
                message = c["message"].as_str().unwrap_or(""),
            );
        }
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

async fn session_diff(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    vs: Option<&str>,
) -> Result<(), CliError> {
    let mut url = format!("{endpoint}/sessions/{id}/diff");
    if let Some(v) = vs {
        // The vs param is a git ref; assume the caller has already
        // shell-quoted it. URL-encoding could happen here later if
        // we ever surface refs with awkward characters.
        url.push_str(&format!("?vs={v}"));
    }
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    print!("{body}");
    Ok(())
}

async fn session_fork(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    at: Option<i64>,
    title: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let mut payload = serde_json::Map::new();
    if let Some(idx) = at {
        payload.insert("from_event_idx".into(), Value::from(idx));
    }
    if let Some(t) = title {
        payload.insert("title".into(), Value::from(t));
    }
    let resp = client
        .post(format!("{endpoint}/sessions/{id}/fork"))
        .json(&Value::Object(payload))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
        return Ok(());
    }
    println!("forked: {}", parsed["session_id"].as_str().unwrap_or(""));
    if let Some(branch) = parsed["branch"].as_str() {
        println!("branch: {branch}");
    }
    if let Some(sha) = parsed["from_sha"].as_str() {
        println!("from  : {sha}");
    }
    Ok(())
}

async fn session_resume(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let url = format!("{endpoint}/sessions/{id}/resume");
    let resp = client.post(url).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
        return Ok(());
    }
    println!("{}", parsed["note"].as_str().unwrap_or("resumed"));
    Ok(())
}

async fn session_checkpoint(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/sessions/{id}/checkpoint"))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&parsed)?);
        return Ok(());
    }
    if let Some(sha) = parsed["commit_sha"].as_str() {
        println!("checkpoint: {sha}");
    } else {
        println!("checkpoint: (no-op — workspace clean)");
    }
    Ok(())
}

async fn session_prompt(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    text: &str,
    json: bool,
) -> Result<(), CliError> {
    let payload = serde_json::json!({ "text": text });
    let resp = client
        .post(format!("{endpoint}/sessions/{id}/prompt"))
        .json(&payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    if json {
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| CliError::Other(format!("invalid JSON: {e}")))?;
        println!("{}", serde_json::to_string_pretty(&parsed)?);
    } else {
        println!("prompt forwarded");
    }
    Ok(())
}

fn print_sse_frame(frame: &str) {
    if let Some(line) = format_sse_frame(frame) {
        println!("{line}");
    }
}

/// Render one SSE frame as a single human-readable line. `None` for
/// frames that carry only a comment / keep-alive (no `event:` or
/// `data:` field). Pulled out of `print_sse_frame` so it's unit-testable.
fn format_sse_frame(frame: &str) -> Option<String> {
    let mut event: Option<String> = None;
    let mut id: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start_matches(' '));
        }
    }
    if event.is_none() && data_lines.is_empty() {
        return None;
    }
    let event = event.unwrap_or_else(|| "message".to_string());
    let data = data_lines.join("\n");
    let parsed: Value = serde_json::from_str(&data).unwrap_or(Value::String(data));
    let id_str = id.as_deref().unwrap_or("-");
    Some(format!("[{id_str:>6}] {event}: {parsed}"))
}

// ---- image subcommand ---------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn image_build(
    repo: &str,
    source: &Path,
    tag: Option<&str>,
    images_dir: &Path,
    docker_bin: Option<&str>,
    format: Format,
    inject_agent: Option<&Path>,
    inject_bootstrap: Option<&Path>,
    transport: engram_image_builder::Transport,
) -> Result<(), CliError> {
    let resolved_tag = tag
        .map(str::to_string)
        .unwrap_or_else(|| format!("warm-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));
    let agent_injection = inject_agent.map(|p| engram_image_builder::AgentInjection {
        agent_binary: p.to_path_buf(),
        // Reserved port engram-agentd listens on inside the guest.
        // Hard-coded here (and in engram-sandbox-firecracker as
        // ENGRAM_AGENTD_PORT) so the bake and the host's connect
        // logic agree without a config flow.
        vsock_port: 1024,
        transport,
        init_script: None,
        bootstrap_binary: inject_bootstrap.map(|p| p.to_path_buf()),
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
    let builder = Builder::new(docker);
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
    Ok(())
}

// ---- shared helpers -----------------------------------------------------

fn build_client(token: Option<&str>) -> Result<reqwest::Client, CliError> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = token {
        // Default header travels on every request (incl. SSE GETs).
        // Errors here mean the token has illegal header bytes —
        // surface that early instead of letting reqwest 4xx for us.
        let mut headers = reqwest::header::HeaderMap::new();
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
            .map_err(|e| CliError::Other(format!("invalid token for header: {e}")))?;
        headers.insert(reqwest::header::AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    builder
        .build()
        .map_err(|e| CliError::Other(format!("http client: {e}")))
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<Value, CliError> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    serde_json::from_str(&body).map_err(|e| CliError::Other(format!("invalid JSON: {e}")))
}

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
    fn format_sse_frame_pretty_prints_event_id_data() {
        let frame = "event:exec_completed\nid:7\ndata:{\"exec_id\":\"x\",\"exit_status\":0}";
        let out = format_sse_frame(frame).unwrap();
        assert!(out.contains("exec_completed"), "got {out}");
        assert!(out.contains("\"exec_id\":\"x\""), "got {out}");
        assert!(
            out.contains("[     7]"),
            "id should be right-aligned: {out}"
        );
    }

    #[test]
    fn format_sse_frame_defaults_event_to_message() {
        let frame = "data:hello";
        let out = format_sse_frame(frame).unwrap();
        // No `event:` field -> default per the SSE spec.
        assert!(out.contains("message"), "got {out}");
    }

    #[test]
    fn format_sse_frame_skips_keepalive_comments() {
        // axum's keep-alive emits `: keep-alive\n\n` — there's no
        // event or data, just a comment line. Filtering at the
        // formatter stops every keep-alive from polluting the
        // user's terminal.
        assert!(format_sse_frame(": keep-alive").is_none());
        assert!(format_sse_frame(":").is_none());
    }

    #[test]
    fn format_sse_frame_treats_non_json_data_as_string() {
        let out = format_sse_frame("event:raw\ndata:not-json").unwrap();
        assert!(out.contains("\"not-json\""), "got {out}");
    }

    #[test]
    fn format_sse_frame_uses_dash_for_missing_id() {
        let out = format_sse_frame("event:x\ndata:1").unwrap();
        assert!(out.contains("[     -]"), "got {out}");
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
