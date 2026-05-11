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
    /// Manage Docker registry credentials (Phase 5+). Credentials
    /// are envelope-encrypted in Postgres; the password never leaves
    /// the coordinator in plaintext after the initial POST.
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    /// Manage harness packs registered with the coordinator. Pack
    /// bytes live in a Docker registry; this surface manages the
    /// `(name, registry_uri)` index that sessions select by.
    Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    },
    /// Admin operations — explicit triggers for primitives whose
    /// production driver is implicit (disk-pressure detector etc.).
    /// Auth-gated behind the same bearer token as the rest of the
    /// API.
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AdminCmd {
    /// Force a cold-tier flush of one Idle session's snapshot.
    /// Errors with 409 if the session isn't Idle (no snapshot to
    /// flush). Returns the FlushOutcome JSON.
    Flush { id: String },
    /// Flush every Idle session in the cluster, in parallel
    /// (bounded). Useful for "drain before redeploy" + integration
    /// tests. Returns one entry per session — success or
    /// per-session error.
    FlushIdle,
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// Create a new session. Prints the new session_id on stdout.
    ///
    /// Two axes shape a session: `image`, `harness`. The bake image's
    /// `/workspace` is the workspace; ADR 0005 retired the platform's
    /// git surface, so agents that want to push code do it themselves
    /// inside the sandbox using credentials mounted via `[secrets.X]`.
    Create {
        /// Image to boot, as a flat OCI URI. Required.
        ///
        /// Example: `--image ghcr.io/cortex/api:warm-2026-04`
        #[arg(long)]
        image: String,
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
enum RegistryCmd {
    /// Add (or update) a Docker registry credential. Two auth kinds
    /// today; the schema is designed for AWS instance role and
    /// service-account impersonation to slot in later as siblings.
    ///
    /// - `--auth-kind static` (default): paste a username + password
    ///   / PAT / service-account JSON. Sealed under the deployment
    ///   KEK, decrypted on each pull.
    /// - `--auth-kind gcp-workload-identity`: Engram's host-agent
    ///   uses its ambient GCP identity (GKE WI, GCE/Cloud Run SA)
    ///   to fetch a short-lived OAuth token per pull. No stored
    ///   password. `--impersonate-sa <email>` chains identity
    ///   through IAM Credentials API (deferred — schema-ready).
    Add {
        /// Registry host. Examples: `gcr.io`, `ghcr.io`,
        /// `us-east1-docker.pkg.dev`,
        /// `123456.dkr.ecr.us-east-1.amazonaws.com`, `localhost:5001`.
        #[arg(long)]
        host: String,
        /// Authentication kind. Defaults to `static` for backward-
        /// compat with the original v1 surface.
        #[arg(long, default_value = "static")]
        auth_kind: String,
        /// Static-only: registry username. For GCP SA-JSON-keys, use
        /// the literal string `_json_key`.
        #[arg(long, required_if_eq("auth_kind", "static"))]
        username: Option<String>,
        /// Static-only: path to a file containing the password / PAT
        /// / service-account JSON. Trailing newline is stripped.
        #[arg(long, conflicts_with = "password_stdin")]
        password_file: Option<PathBuf>,
        /// Static-only: read the password from stdin until EOF.
        #[arg(long)]
        password_stdin: bool,
        /// GCP-WI-only: impersonate this service account via IAM
        /// Credentials API. Omit for ambient-identity (the common
        /// case — same SA the host-agent runs under).
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

#[derive(Subcommand, Debug)]
enum HarnessCmd {
    /// Tar+gzip a local pack directory and push it as an OCI artifact
    /// to the registry. Pure registry-side work — no Postgres write,
    /// no coordinator API call. Symmetric with `engram image build
    /// --push`. After the push, register the pack with the
    /// coordinator via `engram harness add` (or the dashboard's
    /// Settings → Harnesses panel).
    Push {
        /// Local directory containing the pack: at least an
        /// executable `harness` entry-point, plus any sidecars the
        /// wrapper exec's at runtime (bundled CLI binaries, etc.).
        #[arg(long)]
        from: PathBuf,
        /// Destination OCI URI, e.g.
        /// `localhost:5001/cortex/harness-claude:v1` or
        /// `gcr.io/cortex/harness-claude:v1.2`.
        #[arg(long)]
        to: String,
    },
    /// Register an already-pushed harness pack with the coordinator.
    /// POSTs to `/api/harnesses`; this is the only path that writes a
    /// `harness_packs` row.
    ///
    /// Re-registering the same name updates the URI in place.
    Add {
        /// Logical harness name sessions select by (e.g. `claude`,
        /// `noop`). Must be unique.
        #[arg(long)]
        name: String,
        /// Already-pushed OCI URI, e.g. `gcr.io/cortex/harness-claude:v1.2`.
        #[arg(long)]
        registry_uri: String,
        /// Optional one-line description for the dashboard dropdown.
        #[arg(long)]
        description: Option<String>,
    },
    /// List registered harness packs.
    List,
    /// Remove a harness-pack registration. Does not delete the
    /// artifact in the registry — only un-registers it from the
    /// coordinator.
    Rm { name: String },
}

#[derive(Subcommand, Debug)]
enum ImageCmd {
    /// List the operator-curated set of enabled images on the
    /// coordinator. Sessions may only reference URIs in this set.
    List,
    /// Enable an image: tell the coordinator to fetch and cache the
    /// engram manifest from the given OCI URI so sessions can
    /// reference it. The bytes must already exist at the URI (push
    /// via `engram image build --push <uri>`).
    Enable {
        /// Full OCI URI: `<host>[:port]/<repo>:<tag>`.
        #[arg(long)]
        uri: String,
    },
    /// Disable an image. Removes the row; the artifact in the
    /// registry is untouched.
    Disable {
        #[arg(long)]
        uri: String,
    },
    /// Re-fetch the manifest layer for an already-enabled image.
    /// Useful when a moved tag (e.g. `:latest`) now resolves to a new
    /// digest.
    Refresh {
        #[arg(long)]
        uri: String,
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

        /// Push the baked image to a Docker registry as an Engram OCI
        /// artifact. Accepts either `host/repo` (auto-appends the
        /// produced tag) or `host/repo:tag`. Format must be `ext4`.
        ///
        /// The bake step intentionally does NOT touch Postgres — once
        /// pushed, an image is reachable to engram by URI alone (the
        /// host-agent pulls via the auth resolver). Engram learns
        /// about the image when a session references its URI.
        #[arg(long)]
        push: Option<String>,
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
                harness,
                prompt,
                user_id,
            } => {
                session_create(
                    &client,
                    &cli.endpoint,
                    image,
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
            SessionCmd::Log { id, limit } => {
                session_log(&client, &cli.endpoint, id, *limit, cli.json).await
            }
            SessionCmd::Resume { id } => session_resume(&client, &cli.endpoint, id, cli.json).await,
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
                push,
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
                    push.as_deref(),
                )
                .await
            }
            ImageCmd::List => image_list(&client, &cli.endpoint, cli.json).await,
            ImageCmd::Enable { uri } => image_enable(&client, &cli.endpoint, uri, cli.json).await,
            ImageCmd::Disable { uri } => image_disable(&client, &cli.endpoint, uri).await,
            ImageCmd::Refresh { uri } => image_refresh(&client, &cli.endpoint, uri, cli.json).await,
        },
        Cmd::Host { cmd } => match cmd {
            HostCmd::List => host_list(&client, &cli.endpoint, cli.json).await,
            HostCmd::Get { id } => host_get(&client, &cli.endpoint, id, cli.json).await,
            HostCmd::Drain { id } => host_drain(&client, &cli.endpoint, id).await,
        },
        Cmd::Registry { cmd } => match cmd {
            RegistryCmd::Add {
                host,
                auth_kind,
                username,
                password_file,
                password_stdin,
                impersonate_sa,
            } => {
                registry_add(
                    &client,
                    &cli.endpoint,
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
            RegistryCmd::List => registry_list(&client, &cli.endpoint, cli.json).await,
            RegistryCmd::Rm { host } => registry_rm(&client, &cli.endpoint, host).await,
        },
        Cmd::Harness { cmd } => match cmd {
            HarnessCmd::Push { from, to } => harness_push(from, to).await,
            HarnessCmd::Add {
                name,
                registry_uri,
                description,
            } => {
                harness_add(
                    &client,
                    &cli.endpoint,
                    name,
                    registry_uri,
                    description.as_deref(),
                    cli.json,
                )
                .await
            }
            HarnessCmd::List => harness_list(&client, &cli.endpoint, cli.json).await,
            HarnessCmd::Rm { name } => harness_rm(&client, &cli.endpoint, name).await,
        },
        Cmd::Admin { cmd } => match cmd {
            AdminCmd::Flush { id } => admin_flush(&client, &cli.endpoint, id, cli.json).await,
            AdminCmd::FlushIdle => admin_flush_idle(&client, &cli.endpoint, cli.json).await,
        },
    }
}

// ---- admin subcommands -------------------------------------------------

async fn admin_flush(
    client: &reqwest::Client,
    endpoint: &str,
    id: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/api/admin/sessions/{id}/flush"))
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
    println!(
        "flushed {}: {} bytes in {}ms",
        parsed["session_id"].as_str().unwrap_or(id),
        parsed["blob_size_bytes"].as_u64().unwrap_or(0),
        parsed["took_ms"].as_u64().unwrap_or(0),
    );
    Ok(())
}

async fn admin_flush_idle(
    client: &reqwest::Client,
    endpoint: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/api/admin/flush-idle"))
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
    let empty = Vec::new();
    let results = parsed["results"].as_array().unwrap_or(&empty);
    if results.is_empty() {
        println!("(no idle sessions to flush)");
        return Ok(());
    }
    let (mut ok, mut err) = (0usize, 0usize);
    for r in results {
        if r["ok"].as_bool().unwrap_or(false) {
            ok += 1;
            let oc = &r["outcome"];
            println!(
                "ok  {} ({} bytes, {}ms)",
                r["session_id"].as_str().unwrap_or(""),
                oc["blob_size_bytes"].as_u64().unwrap_or(0),
                oc["took_ms"].as_u64().unwrap_or(0),
            );
        } else {
            err += 1;
            println!(
                "err {}: {}",
                r["session_id"].as_str().unwrap_or(""),
                r["error"].as_str().unwrap_or(""),
            );
        }
    }
    println!("\n{ok} flushed, {err} failed");
    Ok(())
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
    println!("image           : {}", body["image"].as_str().unwrap_or(""),);
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

async fn session_create(
    client: &reqwest::Client,
    endpoint: &str,
    image: &str,
    harness: &str,
    prompt: Option<&str>,
    user_id: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let harness_value = match harness {
        "none" => serde_json::json!({"kind": "none"}),
        name => serde_json::json!({"kind": "builtin", "name": name}),
    };
    let mut payload = serde_json::Map::new();
    // Stage B1+: image is a flat OCI URI string.
    payload.insert("image".into(), Value::from(image));
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
    limit: Option<i64>,
    json: bool,
) -> Result<(), CliError> {
    let mut url = format!("{endpoint}/sessions/{id}/log?kind=conversation");
    if let Some(l) = limit {
        url.push_str(&format!("&limit={l}"));
    }
    let body = get_json(client, &url).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
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
    push: Option<&str>,
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
    // ADR 0007: chunked-storage layout. Chunks land at
    // `<images_dir>/store/` so a single bake produces a
    // self-contained tree (mirrors the image-builder binary's
    // wiring).
    let chunk_root = images_dir.join("store");
    tokio::fs::create_dir_all(&chunk_root)
        .await
        .map_err(|e| CliError::Other(format!("chunk store root: {e}")))?;
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> = std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let builder = Builder::new(docker, chunk_store);
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

    // Optional push to OCI registry. The bake step intentionally does
    // NOT touch Postgres — once pushed, the image is reachable to
    // engram by URI alone. The auth resolver wired into the host-
    // agent's OCI client handles credential lookup at pull time.
    // Anonymous push works for `localhost:5001` and other public
    // registries; private targets need a `docker login`-equivalent
    // upstream of this command (the CLI itself doesn't auth pushes).
    if let Some(target) = push {
        let oci = engram_oci::OciClient::new(std::sync::Arc::new(engram_oci::AnonymousResolver));
        let push = builder
            .push_to_registry(&oci, &req, &outcome, target)
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

// ---- registry subcommands ---------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn registry_add(
    client: &reqwest::Client,
    endpoint: &str,
    host: &str,
    auth_kind: &str,
    username: Option<&str>,
    password_file: Option<&std::path::Path>,
    password_stdin: bool,
    impersonate_sa: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    // Per-kind validation + auth-payload assembly. Shape mirrors the
    // server's `AddRegistryAuth` discriminated enum (serde tag = "kind").
    let auth = match auth_kind {
        "static" => {
            let username = username
                .ok_or_else(|| CliError::Other("--auth-kind static requires --username".into()))?;
            let password = match (password_file, password_stdin) {
                (Some(path), false) => {
                    let raw = std::fs::read_to_string(path)
                        .map_err(|e| CliError::Other(format!("read {}: {e}", path.display())))?;
                    // Strip a single trailing newline if present
                    // (common for `echo "$pat" > file` flows). Other
                    // whitespace is left intact — service-account
                    // JSON has internal newlines.
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
            serde_json::json!({
                "kind": "static",
                "username": username,
                "password": password,
            })
        }
        "gcp-workload-identity" | "gcp_workload_identity" => {
            // No secret material; the host-agent's ambient GCP
            // identity is the credential. `--impersonate-sa` chains
            // identity to a target SA via IAM Credentials API
            // (server-side support deferred — schema is ready).
            let mut obj = serde_json::json!({ "kind": "gcp_workload_identity" });
            if let Some(sa) = impersonate_sa {
                obj["impersonate_sa"] = serde_json::Value::String(sa.into());
            }
            obj
        }
        other => {
            return Err(CliError::Other(format!(
                "unknown --auth-kind `{other}` (expected: static | gcp-workload-identity)"
            )));
        }
    };

    let body = serde_json::json!({ "host": host, "auth": auth });
    let resp = client
        .post(format!("{endpoint}/api/registries"))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), resp_body));
    }
    if json {
        println!("{resp_body}");
    } else {
        let v: Value = serde_json::from_str(&resp_body)
            .map_err(|e| CliError::Other(format!("invalid JSON from server: {e}")))?;
        println!(
            "added: host={} auth_kind={} principal={}",
            v["host"].as_str().unwrap_or(""),
            v["auth_kind"].as_str().unwrap_or(""),
            v["auth_principal"].as_str().unwrap_or("(none)"),
        );
    }
    Ok(())
}

async fn registry_list(
    client: &reqwest::Client,
    endpoint: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/api/registries")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let empty = Vec::new();
    let regs = body["registries"].as_array().unwrap_or(&empty);
    if regs.is_empty() {
        println!("(no registries configured)");
        return Ok(());
    }
    println!("{:<40}  {:<24}  PRINCIPAL", "HOST", "AUTH_KIND");
    for r in regs {
        println!(
            "{:<40}  {:<24}  {}",
            r["registry_host"].as_str().unwrap_or(""),
            r["auth_kind"].as_str().unwrap_or(""),
            r["auth_principal"].as_str().unwrap_or("(none)"),
        );
    }
    Ok(())
}

async fn registry_rm(client: &reqwest::Client, endpoint: &str, host: &str) -> Result<(), CliError> {
    // URL-encode the host so `:` in `localhost:5001` survives the path.
    let encoded = urlencode(host);
    let resp = client
        .delete(format!("{endpoint}/api/registries/{encoded}"))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }
    println!("removed");
    Ok(())
}

// ---- harness subcommands ----------------------------------------------

/// Tar+gzip a local pack directory and push it as an OCI artifact.
/// Pure registry-side work — no API call, no Postgres write. Mirrors
/// the `engram image build --push` shape so bake-and-register stay
/// independent operations.
///
/// Anonymous push works for `localhost:5001` and public registries.
/// For authenticated registries the user pre-runs `docker login` (or
/// pushes via `oras`) — wiring `engram harness push` through the
/// coordinator's encrypted creds resolver lands when that's
/// genuinely needed.
async fn harness_push(from: &Path, to: &str) -> Result<(), CliError> {
    let oci = engram_oci::OciClient::new(std::sync::Arc::new(engram_oci::AnonymousResolver));
    tracing::info!(uri = %to, dir = %from.display(), "pushing harness pack");
    let digest = oci
        .push_harness(to, from)
        .await
        .map_err(|e| CliError::Other(format!("oci push: {e}")))?;
    println!("✓ pushed {to}");
    println!("  digest: {}", digest.as_str());
    println!();
    println!("  register it with the coordinator:");
    println!("    engram harness add --name <name> --registry-uri {to}");
    println!("  or in the dashboard:");
    println!("    http://localhost:5173/settings/harnesses");
    Ok(())
}

async fn harness_add(
    client: &reqwest::Client,
    endpoint: &str,
    name: &str,
    registry_uri: &str,
    description: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let mut body = serde_json::json!({
        "name": name,
        "registry_uri": registry_uri,
    });
    if let Some(d) = description {
        body["description"] = serde_json::Value::String(d.into());
    }
    let resp = client
        .post(format!("{endpoint}/api/harnesses"))
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), resp_body));
    }
    if json {
        println!("{resp_body}");
    } else {
        let v: Value = serde_json::from_str(&resp_body)
            .map_err(|e| CliError::Other(format!("invalid JSON from server: {e}")))?;
        println!(
            "added: name={} registry_uri={}",
            v["name"].as_str().unwrap_or(""),
            v["registry_uri"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn harness_list(
    client: &reqwest::Client,
    endpoint: &str,
    json: bool,
) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/api/harnesses")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let empty = Vec::new();
    let harnesses = body.as_array().unwrap_or(&empty);
    if harnesses.is_empty() {
        println!("(no harnesses registered)");
        return Ok(());
    }
    println!("{:<24}  {:<60}  DESCRIPTION", "NAME", "REGISTRY_URI");
    for h in harnesses {
        let uri = h["registry_uri"].as_str().unwrap_or("(host-resident)");
        println!(
            "{:<24}  {:<60}  {}",
            h["name"].as_str().unwrap_or(""),
            uri,
            h["description"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn harness_rm(client: &reqwest::Client, endpoint: &str, name: &str) -> Result<(), CliError> {
    let resp = client
        .delete(format!("{endpoint}/api/harnesses/{}", urlencode(name)))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }
    println!("removed");
    Ok(())
}

// ---- enabled-images subcommands ---------------------------------------

async fn image_list(client: &reqwest::Client, endpoint: &str, json: bool) -> Result<(), CliError> {
    let body = get_json(client, &format!("{endpoint}/api/enabled-images")).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    let empty = Vec::new();
    let images = body["images"].as_array().unwrap_or(&empty);
    if images.is_empty() {
        println!("(no images enabled — `engram image enable --uri <uri>` to add one)");
        return Ok(());
    }
    println!("{:<48}  {:<22}  DIGEST", "URI", "NAME",);
    for img in images {
        println!(
            "{:<48}  {:<22}  {}",
            img["image_uri"].as_str().unwrap_or(""),
            img["manifest_name"].as_str().unwrap_or("(unparsed)"),
            img["manifest_digest"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn image_enable(
    client: &reqwest::Client,
    endpoint: &str,
    uri: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/api/enabled-images"))
        .json(&serde_json::json!({ "image_uri": uri }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    if json {
        println!("{body}");
    } else {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        println!(
            "enabled: uri={} digest={}",
            v["image_uri"].as_str().unwrap_or(uri),
            v["manifest_digest"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn image_disable(
    client: &reqwest::Client,
    endpoint: &str,
    uri: &str,
) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/api/enabled-images/disable"))
        .json(&serde_json::json!({ "image_uri": uri }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(CliError::Http(status.as_u16(), body));
    }
    println!("disabled");
    Ok(())
}

async fn image_refresh(
    client: &reqwest::Client,
    endpoint: &str,
    uri: &str,
    json: bool,
) -> Result<(), CliError> {
    let resp = client
        .post(format!("{endpoint}/api/enabled-images/refresh"))
        .json(&serde_json::json!({ "image_uri": uri }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(CliError::Http(status.as_u16(), body));
    }
    if json {
        println!("{body}");
    } else {
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        println!(
            "refreshed: uri={} digest={}",
            v["image_uri"].as_str().unwrap_or(uri),
            v["manifest_digest"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

/// Minimal percent-encoder for the path components that registry/host
/// names contain (`:`, `/`). Avoids pulling in the `url` crate just
/// for this — these are always single-segment paths.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
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
