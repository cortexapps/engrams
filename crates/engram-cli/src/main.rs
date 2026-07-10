//! Ops/admin CLI. Talks to the coordinator's app-gRPC API
//! (`engram_protocol::app::*`, ADR 0051 Drip E) for session / fleet /
//! image lifecycle operations. ADR 0080 retired the local `image build`
//! bake: an image is now a plain `docker build && docker push`, enabled
//! via `image enable --config` (the coordinator materializes it host-side).
//!
//! Every RPC authenticates with an `Authorization: Bearer <token>` gRPC
//! metadata header via a tonic interceptor; the bearer + endpoint are
//! discovered from the same env the coordinator + stack wiring use
//! (`ENGRAM_APP_GRPC_ADDR` / `ENGRAM_APP_GRPC_TOKENS`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use engram_protocol::app::image_service_client::ImageServiceClient;
use engram_protocol::app::session_service_client::SessionServiceClient;
use serde_json::Value;
use tonic::codegen::InterceptedService;
use tonic::transport::Channel;

#[derive(Parser, Debug)]
#[command(name = "engram", version, about = "Engram ops CLI")]
struct Cli {
    /// Coordinator app-gRPC endpoint. Needs an `http://` (or `https://`)
    /// scheme so it parses as a tonic endpoint. Matches the stack's
    /// `ENGRAM_APP_GRPC_ADDR`.
    #[arg(
        long,
        env = "ENGRAM_APP_GRPC_ADDR",
        default_value = "http://localhost:50061"
    )]
    grpc_endpoint: String,

    /// Bearer token for the coordinator's app-gRPC surface. Precedence:
    /// this `--token` flag > the singular `ENGRAM_APP_GRPC_TOKEN` env >
    /// the first token of the comma/space-separated `ENGRAM_APP_GRPC_TOKENS`
    /// env (the coord's accepted set). When the coordinator runs with an
    /// empty accepted set (dev mode), leave this unset.
    #[arg(long, env = "ENGRAM_APP_GRPC_TOKEN")]
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
    // `hosts` alias so the plural reads naturally in scripts (the CI
    // bring-up gate + the integration dev tools all say `hosts list`).
    #[command(visible_alias = "hosts")]
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
    /// the coordinator in plaintext after the initial RPC.
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
    // ADR 0021 P1.5a/P1.7 retired the `Harness` subcommand entirely.
    // Built-in harness publish moved to the `engram-publish-builtin-
    // harness` crate (CI-only); the `/api/harnesses` registry was
    // deleted with the rest of the standalone subsystem.
    /// Admin operations — explicit triggers for primitives whose
    /// production driver is implicit (idle detector etc.).
    /// Auth-gated behind the same bearer token as the rest of the
    /// API.
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AdminCmd {
    /// Force an immediate chunked-disk flush of one session's bound
    /// sandbox (`FleetService.FlushSession`). Prints the outcome
    /// (`applied` / `idle` / `stale`) + the new manifest version.
    Flush { id: String },
    /// Explicitly trigger the idle-eviction primitive for ONE active
    /// session (`SessionService.EvictIdle`). Fires the SAME pipeline the
    /// host-side idle detector + the coord idle-detect backstop drive on
    /// a timeout — pause + flush + snapshot + drop the local sandbox,
    /// leaving the session Idle — WITHOUT waiting out the idle TTL.
    /// Synchronous: the session is Idle by the time this returns.
    ///
    /// ADR 0051: this replaces the old fleet-wide `flush-idle` admin
    /// trigger, which has no app-gRPC analog (there is no fleet-wide
    /// "flush every idle session" RPC). This acts on a single session id.
    EvictIdle { id: String },
    /// Run the three blob-storage GC sweeps (bundle generations,
    /// snapshot blobs, chunks — ADR 0035 §5 / ADR 0028 addendum /
    /// ADR 0016 Phase C) and print each report. DRY-RUN by default
    /// (classify + count, no writes); pass --apply for the live
    /// pipeline (candidate upserts + promote-pass deletes). Blobs are
    /// deleted only after their candidate outlives the grace window
    /// (default 24h) — `--grace-secs 0` promotes immediately, the dev
    /// "clear stale generations now" posture.
    Gc {
        /// Actually mark candidates + delete promoted blobs. Without
        /// this flag the sweeps only report what WOULD be marked.
        #[arg(long)]
        apply: bool,
        /// Override the candidate grace window (seconds) for these
        /// sweeps only. 0 = promote (delete) in the same sweep.
        #[arg(long)]
        grace_secs: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// Create a new session. Prints the new session_id on stdout.
    ///
    /// The bake image's `/workspace` is the workspace; ADR 0005 retired
    /// the platform's git surface, so agents that want to push code do it
    /// themselves inside the sandbox using credentials mounted via
    /// `[secrets.X]`.
    Create {
        /// Image to boot, as a flat OCI URI. Required.
        ///
        /// Example: `--image ghcr.io/cortex/api:warm-2026-04`
        #[arg(long)]
        image: String,
        /// Boot the image as a pure dev VM — no harness driven; interact
        /// via shell / exec.
        #[arg(long, default_value_t = false)]
        dev_vm: bool,
        /// ADR 0062: the catalog harness to run (a `HarnessCatalogService`
        /// name, e.g. "claude"). Required for an agent session; ignored with
        /// `--dev-vm`. Run `engram harness list` to see the catalog.
        #[arg(long)]
        harness: Option<String>,
        /// Initial prompt for the agent. Only meaningful in the default
        /// (agent) mode; the API rejects with INVALID_ARGUMENT if a prompt is
        /// supplied alongside `--dev-vm`.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// List sessions the coordinator knows about.
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
    /// Tail the persistent event log via the `StreamEvents` gRPC stream.
    /// Stays open; Ctrl-C to stop. `--since N` replays strictly-after
    /// idx N, then tails; omit it to start from the beginning.
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
    /// Idle sessions.
    Prompt { id: String, text: String },
}

#[derive(Subcommand, Debug)]
enum HostCmd {
    /// List hosts the coordinator knows about (Postgres rows + live
    /// scheduler view).
    List,
    /// Print one host's row + capacity / local-snapshot view.
    Get { id: String },
    /// Flip a host to `draining`. New sessions won't be assigned to
    /// it; in-flight sessions stay.
    Drain { id: String },
    /// Deregister a host row (`FleetService.DeleteHost`). Fails with
    /// FAILED_PRECONDITION while any session is still bound (drain /
    /// reap sessions first); idempotent-success if already gone. The
    /// dev use case: a dead leftover host-agent (e.g. an fc-colima VM
    /// from `just dev-fc`) keeps winning the fleet bundle catalog and
    /// session placement until its row is removed.
    Delete { id: String },
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

// ADR 0021 P1.7 retired `HarnessCmd` entirely. The built-in harness
// publish primitive lives in `crates/engram-publish-builtin-harness`
// (CI-only); user-facing `engram harness {add,list,rm}` retired with
// the `/api/harnesses` registry in P1.5a.

#[derive(Subcommand, Debug)]
enum ImageCmd {
    /// List the operator-curated set of enabled images on the
    /// coordinator. Sessions may only reference URIs in this set.
    List,
    /// Enable an image: tell the coordinator to fetch and cache the
    /// engram artifact metadata from the given OCI URI so sessions can
    /// reference it. The bytes must already exist at the URI (push
    /// via `engram image build --push <uri>`).
    Enable {
        /// Full OCI URI: `<host>[:port]/<repo>:<tag>`.
        #[arg(long)]
        uri: String,
        /// ADR 0080: path to the image-config TOML (name, description,
        /// [env], workdir, [resources], [warm] incl. [[warm.env]] +
        /// [warm.network]). REQUIRED on the first enable; omit on a
        /// re-enable to inherit the already-enabled row's config.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Don't poll the enable job to completion — print the job id
        /// and return immediately (ADR 0036: enabling is async).
        #[arg(long)]
        no_wait: bool,
    },
    /// ADR 0080: edit an enabled image's config (full replace). Cheap
    /// fields (name/description/env/workdir) apply immediately; a diff
    /// touching resources or [warm] needs --allow-recapture and enqueues
    /// a recapture job.
    Update {
        /// Full OCI URI of an already-enabled image.
        #[arg(long)]
        uri: String,
        /// Path to the complete new image-config TOML (full replace,
        /// not a patch).
        #[arg(long)]
        config: PathBuf,
        /// Consent to a base-snapshot recapture when the diff touches
        /// resources or [warm] (takes minutes; the enable pipeline
        /// re-runs).
        #[arg(long)]
        allow_recapture: bool,
        /// Don't poll a recapture job to completion.
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
    /// Useful when a moved tag (e.g. `:latest`) now resolves to a new
    /// digest.
    Refresh {
        #[arg(long)]
        uri: String,
        /// Force a new base-snapshot capture instead of reusing an existing
        /// content-identical snapshot.
        #[arg(long)]
        recapture: bool,
    },
    /// List recent image enable / refresh jobs.
    Jobs,
    /// Print one image enable / refresh job.
    Job { id: String },
    /// Poll one image enable / refresh job until it reaches ready/failed.
    PollJob { id: String },
    /// Re-queue a failed image enable / refresh job.
    RetryJob {
        id: String,
        /// Don't poll the retried job to completion.
        #[arg(long)]
        no_wait: bool,
    },
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
        Err(CliError::Rpc(status)) => {
            // gRPC error: surface the code + message, plus the
            // coordinator's `engram-error-slug` metadata key when present.
            let slug = status
                .metadata()
                .get("engram-error-slug")
                .and_then(|v| v.to_str().ok())
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            eprintln!(
                "engram-cli: {:?}: {}{slug}",
                status.code(),
                status.message()
            );
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
    /// A gRPC RPC failed — carries the full `tonic::Status` (code +
    /// message + metadata).
    Rpc(tonic::Status),
    Other(String),
}

impl From<tonic::Status> for CliError {
    fn from(s: tonic::Status) -> Self {
        Self::Rpc(s)
    }
}

impl From<tonic::transport::Error> for CliError {
    fn from(e: tonic::transport::Error) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.to_string())
    }
}

/// `Authorization: Bearer <token>` interceptor. tonic requires
/// `Result<_, Status>`. Cloned per-service so the three clients share one
/// channel + one bearer.
#[derive(Clone)]
struct BearerFn {
    token: Option<String>,
}

impl tonic::service::Interceptor for BearerFn {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.token {
            req.metadata_mut().insert(
                "authorization",
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| tonic::Status::invalid_argument("bearer token is not ASCII"))?,
            );
        }
        Ok(req)
    }
}

type SessClient = SessionServiceClient<InterceptedService<Channel, BearerFn>>;
type FleetClient = FleetServiceClient<InterceptedService<Channel, BearerFn>>;
type ImgClient = ImageServiceClient<InterceptedService<Channel, BearerFn>>;

/// The three app-gRPC service clients sharing one channel + bearer.
struct Clients {
    sess: SessClient,
    fleet: FleetClient,
    image: ImgClient,
}

/// Resolve the bearer from the precedence: explicit `--token` flag (the
/// clap `token` field, which also reads `ENGRAM_APP_GRPC_TOKEN`) > the
/// first token of the comma/space-separated `ENGRAM_APP_GRPC_TOKENS` env.
/// `None` when nothing is configured (dev mode, empty accepted set).
fn resolve_bearer(flag: Option<&str>) -> Option<String> {
    if let Some(t) = flag {
        return Some(t.to_string());
    }
    let tokens = std::env::var("ENGRAM_APP_GRPC_TOKENS").ok()?;
    tokens
        .split([',', ' '])
        .map(str::trim)
        .find(|t| !t.is_empty())
        .map(str::to_string)
}

/// Ensure the endpoint carries an `http://`/`https://` scheme so
/// `tonic::transport::Endpoint::from_shared` parses it. The stack's
/// `ENGRAM_APP_GRPC_ADDR` is set scheme-less in places (e.g. the Tiltfile's
/// `127.0.0.1:50061`); default to plain `http` when no scheme is present.
fn normalize_endpoint(endpoint: &str) -> String {
    if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    }
}

async fn build_clients(endpoint: &str, token: Option<&str>) -> Result<Clients, CliError> {
    let bearer = resolve_bearer(token);
    let endpoint = normalize_endpoint(endpoint);
    let endpoint = endpoint.as_str();
    let channel = tonic::transport::Endpoint::from_shared(endpoint.to_string())
        .map_err(|e| CliError::Other(format!("invalid --grpc-endpoint {endpoint:?}: {e}")))?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .map_err(|e| CliError::Other(format!("connect to coordinator at {endpoint}: {e}")))?;
    let interceptor = BearerFn { token: bearer };
    Ok(Clients {
        sess: SessionServiceClient::with_interceptor(channel.clone(), interceptor.clone()),
        fleet: FleetServiceClient::with_interceptor(channel.clone(), interceptor.clone()),
        image: ImageServiceClient::with_interceptor(channel, interceptor),
    })
}

async fn run(cli: &Cli) -> Result<(), CliError> {
    let mut c = build_clients(&cli.grpc_endpoint, cli.token.as_deref()).await?;
    match &cli.cmd {
        Cmd::Session { cmd } => match cmd {
            SessionCmd::Create {
                image,
                dev_vm,
                harness,
                prompt,
            } => {
                session_create(
                    &mut c,
                    image,
                    *dev_vm,
                    prompt.as_deref(),
                    harness.as_deref(),
                    cli.json,
                )
                .await
            }
            SessionCmd::List => session_list(&mut c, cli.json).await,
            SessionCmd::Get { id } => session_get(&mut c, id, cli.json).await,
            SessionCmd::Exec {
                id,
                cmd: shell,
                timeout_secs,
            } => session_exec(&mut c, id, shell, *timeout_secs, cli.json).await,
            SessionCmd::Delete { id } => session_delete(&mut c, id).await,
            SessionCmd::Logs { id, since } => session_logs(&mut c, id, *since).await,
            SessionCmd::Log { id, limit } => session_log(&mut c, id, *limit, cli.json).await,
            SessionCmd::Resume { id } => session_resume(&mut c, id, cli.json).await,
            SessionCmd::Prompt { id, text } => session_prompt(&mut c, id, text, cli.json).await,
        },
        Cmd::Image { cmd } => match cmd {
            ImageCmd::List => image_list(&mut c, cli.json).await,
            ImageCmd::Enable {
                uri,
                config,
                no_wait,
            } => image_enable(&mut c, uri, config.as_deref(), cli.json, *no_wait).await,
            ImageCmd::Update {
                uri,
                config,
                allow_recapture,
                no_wait,
            } => image_update(&mut c, uri, config, *allow_recapture, cli.json, *no_wait).await,
            ImageCmd::Disable { uri } => image_disable(&mut c, uri).await,
            ImageCmd::Refresh { uri, recapture } => {
                image_refresh(&mut c, uri, *recapture, cli.json).await
            }
            ImageCmd::Jobs => image_jobs(&mut c, cli.json).await,
            ImageCmd::Job { id } => image_job(&mut c, id, cli.json).await,
            ImageCmd::PollJob { id } => poll_enable_job(&mut c, id, cli.json).await,
            ImageCmd::RetryJob { id, no_wait } => {
                image_retry_job(&mut c, id, cli.json, *no_wait).await
            }
        },
        Cmd::Host { cmd } => match cmd {
            HostCmd::List => host_list(&mut c, cli.json).await,
            HostCmd::Get { id } => host_get(&mut c, id, cli.json).await,
            HostCmd::Drain { id } => host_drain(&mut c, id).await,
            HostCmd::Delete { id } => host_delete(&mut c, id).await,
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
                    &mut c,
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
            RegistryCmd::List => registry_list(&mut c, cli.json).await,
            RegistryCmd::Rm { host } => registry_rm(&mut c, host).await,
        },
        // ADR 0021 P1.7 retired the `Cmd::Harness` arm.
        Cmd::Admin { cmd } => match cmd {
            AdminCmd::Flush { id } => admin_flush(&mut c, id, cli.json).await,
            AdminCmd::EvictIdle { id } => admin_evict_idle(&mut c, id, cli.json).await,
            AdminCmd::Gc { apply, grace_secs } => {
                admin_gc(&mut c, *apply, *grace_secs, cli.json).await
            }
        },
    }
}

// ---- admin subcommands -------------------------------------------------

async fn admin_flush(c: &mut Clients, id: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .fleet
        .flush_session(app::FlushSessionRequest {
            session_id: id.to_string(),
        })
        .await?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "outcome": resp.outcome,
            "manifest_version": resp.manifest_version,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    match resp.manifest_version {
        Some(v) => println!("{}: manifest_version={v}", resp.outcome),
        None => println!("{}", resp.outcome),
    }
    Ok(())
}

async fn admin_evict_idle(c: &mut Clients, id: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .sess
        .evict_idle(app::EvictIdleRequest {
            session_id: id.to_string(),
        })
        .await?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "status": resp.status,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("{}: {}", resp.session_id, resp.status);
    Ok(())
}

async fn admin_gc(
    c: &mut Clients,
    apply: bool,
    grace_secs: Option<u64>,
    json: bool,
) -> Result<(), CliError> {
    // The proto's `dry_run: false` means a LIVE sweep with deletions, so
    // the CLI inverts the polarity: dry-run unless --apply is explicit.
    let dry_run = !apply;
    let mode = if dry_run { "dry-run" } else { "applied" };
    // The sweeps are independent — run all three and report each result
    // as it lands (one sweep's failure must not swallow the others'
    // reports), then fail the command if any errored.
    let mut failures: Vec<String> = Vec::new();
    let mut out = serde_json::json!({ "dry_run": dry_run });

    match c
        .fleet
        .bundle_gc(app::BundleGcRequest {
            dry_run,
            grace_secs,
        })
        .await
    {
        Ok(resp) => {
            let bundle = resp.into_inner();
            if json {
                out["bundle"] = serde_json::json!({
                    "listed": bundle.listed,
                    "pinned": bundle.pin_set_size,
                    "candidates_marked": bundle.candidates_marked,
                    "promoted_deletes": bundle.promoted_deletes,
                    "promote_delete_errors": bundle.promote_delete_errors,
                });
            } else {
                println!(
                    "bundle-gc ({mode}): listed={} pinned={} candidates={} deleted={} errors={}",
                    bundle.listed,
                    bundle.pin_set_size,
                    bundle.candidates_marked,
                    bundle.promoted_deletes,
                    bundle.promote_delete_errors
                );
            }
        }
        Err(e) => failures.push(format!("bundle-gc: {}", e.message())),
    }

    match c
        .fleet
        .snapshot_blob_gc(app::SnapshotBlobGcRequest {
            dry_run,
            grace_secs,
        })
        .await
    {
        Ok(resp) => {
            let snap = resp.into_inner();
            if json {
                out["snapshot_blob"] = serde_json::json!({
                    "listed": snap.listed,
                    "pinned": snap.pin_set_size,
                    "candidates_marked": snap.candidates_marked,
                    "promoted_deletes": snap.promoted_deletes,
                    "promote_repinned_skips": snap.promote_repinned_skips,
                    "promote_delete_errors": snap.promote_delete_errors,
                });
            } else {
                println!(
                    "snapshot-blob-gc ({mode}): listed={} pinned={} candidates={} deleted={} repinned_skips={} errors={}",
                    snap.listed,
                    snap.pin_set_size,
                    snap.candidates_marked,
                    snap.promoted_deletes,
                    snap.promote_repinned_skips,
                    snap.promote_delete_errors
                );
            }
        }
        Err(e) => failures.push(format!("snapshot-blob-gc: {}", e.message())),
    }

    match c
        .fleet
        .chunk_gc(app::ChunkGcRequest {
            dry_run,
            grace_secs,
        })
        .await
    {
        Ok(resp) => {
            let chunk = resp.into_inner();
            if json {
                out["chunk"] = serde_json::json!({
                    "listed": chunk.listed_chunks,
                    "pinned": chunk.pin_set_size,
                    "candidates_marked": chunk.candidates_marked,
                    "promoted_deletes": chunk.promoted_deletes,
                    "promote_delete_errors": chunk.promote_delete_errors,
                    "grace_secs": chunk.grace_secs,
                });
            } else {
                println!(
                    "chunk-gc ({mode}): listed={} pinned={} candidates={} deleted={} errors={} grace_secs={}",
                    chunk.listed_chunks,
                    chunk.pin_set_size,
                    chunk.candidates_marked,
                    chunk.promoted_deletes,
                    chunk.promote_delete_errors,
                    chunk.grace_secs
                );
            }
        }
        Err(e) => failures.push(format!("chunk-gc: {}", e.message())),
    }

    if json {
        out["failures"] = serde_json::json!(failures);
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    if let Some(first) = failures.first() {
        for f in &failures[1..] {
            eprintln!("engram-cli: {f}");
        }
        return Err(CliError::Other(first.clone()));
    }
    Ok(())
}

// ---- session subcommands ------------------------------------------------

/// Hand-build a `serde_json::Value` from a proto `Session` (prost types
/// don't derive Serialize). snake_case field names mirror the old REST
/// JSON; the GONE fields (kind / repo / branch / user_id) are simply
/// absent from this shape.
fn session_to_json(s: &app::Session) -> Value {
    serde_json::json!({
        "id": s.id,
        "status": s.status,
        "host_id": s.host_id,
        "sandbox_id": s.sandbox_id,
        "image": s.image,
        "mode": s.mode,
        "created_at": s.created_at,
        "last_active_at": s.last_active_at,
    })
}

async fn session_list(c: &mut Clients, json: bool) -> Result<(), CliError> {
    let resp = c
        .sess
        .list_sessions(app::ListSessionsRequest {})
        .await?
        .into_inner();
    if json {
        let sessions: Vec<Value> = resp
            .sessions
            .iter()
            .filter_map(|item| item.session.as_ref().map(session_to_json))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "sessions": sessions }))?
        );
        return Ok(());
    }
    if resp.sessions.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    // The gRPC `Session` carries no kind/repo/branch (those left the
    // contract with ADR 0051); render the available fields instead.
    println!("{:<36}  {:<10}  {:<8}  IMAGE", "ID", "STATUS", "MODE");
    for item in &resp.sessions {
        let Some(s) = item.session.as_ref() else {
            continue;
        };
        println!(
            "{:<36}  {:<10}  {:<8}  {}",
            s.id,
            s.status,
            s.mode,
            truncate(&s.image, 48),
        );
    }
    Ok(())
}

async fn session_get(c: &mut Clients, id: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .sess
        .get_session(app::GetSessionRequest {
            session_id: id.to_string(),
        })
        .await?
        .into_inner();
    let s = resp
        .session
        .ok_or_else(|| CliError::Other("response carried no session".into()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&session_to_json(&s))?);
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

async fn session_delete(c: &mut Clients, id: &str) -> Result<(), CliError> {
    c.sess
        .delete_session(app::DeleteSessionRequest {
            session_id: id.to_string(),
        })
        .await?;
    println!("deleted");
    Ok(())
}

async fn session_create(
    c: &mut Clients,
    image: &str,
    dev_vm: bool,
    prompt: Option<&str>,
    harness: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let req = app::CreateSessionRequest {
        selected_skills: Vec::new(),
        capabilities: Vec::new(),
        // ADR 0057: network + secrets are session policy now (no manifest
        // fallback). The admin CLI creates a debug session with allow-all egress
        // — it's a trusted operator tool, and there's no profile to source a
        // network from on this direct create. (A `--allow-host` restriction flag
        // can refine this later.)
        integration_policy_json: r#"{"network":{"default":"allow"}}"#.to_string(),
        image_uri: image.to_string(),
        mode: if dev_vm { "dev_vm" } else { "agent" }.to_string(),
        prompt: prompt.map(str::to_string),
        secrets: HashMap::new(),
        harness_env: HashMap::new(),
        // Admin CLI doesn't correlate optimistic UI; let the coord mint.
        prompt_id: None,
        // ADR 0062: the selected catalog harness (required for an agent session;
        // ignored for --dev-vm). `engram harness list` shows the catalog.
        harness: harness.map(str::to_string),
    };
    let resp = c.sess.create_session(req).await?.into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "status": resp.status,
            "image_version": resp.image_version,
            "kind": resp.kind,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{}", resp.session_id);
    }
    Ok(())
}

async fn session_exec(
    c: &mut Clients,
    id: &str,
    cmd: &str,
    timeout_secs: Option<u64>,
    json: bool,
) -> Result<(), CliError> {
    let req = app::ExecRequest {
        session_id: id.to_string(),
        command: Some(cmd.to_string()),
        argv: Vec::new(),
        env: HashMap::new(),
        workdir: None,
        timeout_secs,
    };
    let mut stream = c.sess.exec(req).await?.into_inner();

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut exit_status: Option<i32> = None;
    while let Some(msg) = stream.message().await? {
        match msg.event {
            Some(app::exec_output::Event::Started(_)) => {}
            Some(app::exec_output::Event::Stdout(b)) => {
                if json {
                    stdout.extend_from_slice(&b);
                } else {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(&b);
                }
            }
            Some(app::exec_output::Event::Stderr(b)) => {
                if json {
                    stderr.extend_from_slice(&b);
                } else {
                    use std::io::Write;
                    let _ = std::io::stderr().write_all(&b);
                }
            }
            Some(app::exec_output::Event::Exit(e)) => exit_status = e.exit_status,
            None => {}
        }
    }

    if json {
        let v = serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout),
            "stderr": String::from_utf8_lossy(&stderr),
            "exit_status": exit_status,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }

    // Mirror the remote process's exit so shell pipelines compose. `None`
    // means the process was killed (no exit status) — treat as failure.
    match exit_status {
        Some(0) => Ok(()),
        Some(code) => Err(CliError::Other(format!("exec exited with {code}"))),
        None => Err(CliError::Other("exec was killed (no exit status)".into())),
    }
}

// ---- host subcommands ---------------------------------------------------

async fn host_list(c: &mut Clients, json: bool) -> Result<(), CliError> {
    let resp = c
        .fleet
        .list_hosts(app::ListHostsRequest {})
        .await?
        .into_inner();
    if json {
        let hosts: Vec<Value> = resp.hosts.iter().map(host_to_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "hosts": hosts }))?
        );
        return Ok(());
    }
    if resp.hosts.is_empty() {
        println!("(no hosts registered)");
        return Ok(());
    }
    println!(
        "{:<36}  {:<10}  {:<10}  {:<10}",
        "ID", "STATUS", "USED_MIB", "TOTAL_MIB"
    );
    for h in &resp.hosts {
        println!(
            "{:<36}  {:<10}  {:<10}  {:<10}",
            h.id, h.status, h.capacity_used_mib, h.capacity_total_mib,
        );
    }
    Ok(())
}

fn host_to_json(h: &app::HostView) -> Value {
    serde_json::json!({
        "id": h.id,
        "hostname": h.hostname,
        "status": h.status,
        "capacity_total_mib": h.capacity_total_mib,
        "capacity_used_mib": h.capacity_used_mib,
        "running_sandboxes": h.running_sandboxes,
        "ready_images": h.ready_images,
        "ready_image_digests": h.ready_image_digests,
        "last_heartbeat_at": h.last_heartbeat_at,
    })
}

async fn host_get(c: &mut Clients, id: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .fleet
        .get_host(app::GetHostRequest {
            host_id: id.to_string(),
        })
        .await?
        .into_inner();
    let h = resp
        .host
        .ok_or_else(|| CliError::Other("response carried no host".into()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&host_to_json(&h))?);
        return Ok(());
    }
    println!("id              : {}", h.id);
    println!("hostname        : {}", h.hostname);
    println!("status          : {}", h.status);
    println!("capacity_used   : {} MiB", h.capacity_used_mib);
    println!("capacity_total  : {} MiB", h.capacity_total_mib);
    println!("running_sandboxes: {}", h.running_sandboxes);
    Ok(())
}

async fn host_drain(c: &mut Clients, id: &str) -> Result<(), CliError> {
    c.fleet
        .drain_host(app::DrainHostRequest {
            host_id: id.to_string(),
        })
        .await?;
    println!("draining");
    Ok(())
}

async fn host_delete(c: &mut Clients, id: &str) -> Result<(), CliError> {
    c.fleet
        .delete_host(app::DeleteHostRequest {
            host_id: id.to_string(),
        })
        .await?;
    println!("deleted");
    Ok(())
}

async fn session_logs(c: &mut Clients, id: &str, since: Option<i64>) -> Result<(), CliError> {
    // `--since N` UNSET = from the start (proto3 `since` unset, not the old
    // HTTP -1 sentinel); `--since N` Some(n) replays strictly-after idx n.
    let req = app::StreamEventsRequest {
        session_id: id.to_string(),
        since,
    };
    let mut stream = c.sess.stream_events(req).await?.into_inner();
    while let Some(ev) = stream.message().await? {
        println!("{}", format_event_line(&ev));
    }
    Ok(())
}

async fn session_log(
    c: &mut Clients,
    id: &str,
    limit: Option<i64>,
    json: bool,
) -> Result<(), CliError> {
    let resp = c
        .sess
        .get_log(app::GetLogRequest {
            session_id: id.to_string(),
            kind: Some("conversation".to_string()),
            limit,
        })
        .await?
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
                    "payload": serde_json::from_str::<Value>(&e.payload_json)
                        .unwrap_or(Value::String(e.payload_json.clone())),
                })
            })
            .collect();
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "kind": resp.kind,
            "events": events,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if resp.events.is_empty() {
        println!("(no events)");
        return Ok(());
    }
    println!("{:<6}  {:<28}  {:<25}  PAYLOAD", "IDX", "KIND", "AT");
    for e in &resp.events {
        let payload: Value = serde_json::from_str(&e.payload_json).unwrap_or(Value::Null);
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

async fn session_resume(c: &mut Clients, id: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .sess
        .resume(app::ResumeRequest {
            session_id: id.to_string(),
        })
        .await?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "snapshot_id": resp.snapshot_id,
            "size_bytes": resp.size_bytes,
            "note": resp.note,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if resp.note.is_empty() {
        println!("resumed");
    } else {
        println!("{}", resp.note);
    }
    Ok(())
}

async fn session_prompt(c: &mut Clients, id: &str, text: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .sess
        .send_prompt(app::SendPromptRequest {
            session_id: id.to_string(),
            text: text.to_string(),
            // Empty → coord mints one (admin CLI has no optimistic UI to correlate).
            prompt_id: String::new(),
        })
        .await?
        .into_inner();
    if json {
        let v = serde_json::json!({
            "session_id": resp.session_id,
            "note": resp.note,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("prompt forwarded");
    }
    Ok(())
}

/// Render one `SessionEvent` envelope as a single human-readable line.
/// Mirrors the old SSE line shape: `[{idx}] {kind}: {payload_json}`, with
/// the idx right-aligned and `-` for the (idx-less) `lagged` event.
fn format_event_line(ev: &app::SessionEvent) -> String {
    let idx = ev
        .idx
        .map(|i| i.to_string())
        .unwrap_or_else(|| "-".to_string());
    // Re-render the payload through serde so it prints compactly + so a
    // well-formed object isn't double-quoted; fall back to the raw string.
    let payload: Value =
        serde_json::from_str(&ev.payload_json).unwrap_or(Value::String(ev.payload_json.clone()));
    format!("[{idx:>6}] {}: {payload}", ev.kind)
}

// ---- registry subcommands ---------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn registry_add(
    c: &mut Clients,
    host: &str,
    auth_kind: &str,
    username: Option<&str>,
    password_file: Option<&std::path::Path>,
    password_stdin: bool,
    impersonate_sa: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    // Per-kind validation + auth-payload assembly. Maps to the
    // AddRegistryRequest oneof.
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
            app::add_registry_request::Auth::Static(app::StaticRegistryAuth {
                username: username.to_string(),
                password,
            })
        }
        "gcp-workload-identity" | "gcp_workload_identity" => {
            // No secret material; the host-agent's ambient GCP
            // identity is the credential. `--impersonate-sa` chains
            // identity to a target SA via IAM Credentials API
            // (server-side support deferred — schema is ready).
            app::add_registry_request::Auth::GcpWorkloadIdentity(
                app::GcpWorkloadIdentityRegistryAuth {
                    impersonate_sa: impersonate_sa.map(str::to_string),
                },
            )
        }
        other => {
            return Err(CliError::Other(format!(
                "unknown --auth-kind `{other}` (expected: static | gcp-workload-identity)"
            )));
        }
    };

    let resp = c
        .image
        .add_registry(app::AddRegistryRequest {
            host: host.to_string(),
            auth: Some(auth),
        })
        .await?
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

async fn registry_list(c: &mut Clients, json: bool) -> Result<(), CliError> {
    let resp = c
        .image
        .list_registries(app::ListRegistriesRequest {})
        .await?
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
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "registries": regs }))?
        );
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

async fn registry_rm(c: &mut Clients, host: &str) -> Result<(), CliError> {
    c.image
        .delete_registry(app::DeleteRegistryRequest {
            host: host.to_string(),
        })
        .await?;
    println!("removed");
    Ok(())
}

// ADR 0021 P1.5a + P1.7 retired the entire `engram harness ...`
// surface — the publish primitive moved to its own CI-only crate
// (`engram-publish-builtin-harness`); the registry-CRUD subcommands
// (add/list/rm) went with the `/api/harnesses` endpoint.

// ---- enabled-images subcommands ---------------------------------------

async fn image_list(c: &mut Clients, json: bool) -> Result<(), CliError> {
    let resp = c
        .image
        .list_enabled_images(app::ListEnabledImagesRequest {})
        .await?
        .into_inner();
    if json {
        let images: Vec<Value> = resp
            .images
            .iter()
            .map(|img| {
                serde_json::json!({
                    "id": img.id,
                    "image_uri": img.image_uri,
                    "manifest_digest": img.manifest_digest,
                    "name": img.config.as_ref().map(|c| c.name.clone()),
                    "description": img.config.as_ref().and_then(|c| c.description.clone()),
                    "last_refreshed_at": img.last_refreshed_at,
                    "created_at": img.created_at,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "images": images }))?
        );
        return Ok(());
    }
    if resp.images.is_empty() {
        println!("(no images enabled — `engram image enable --uri <uri>` to add one)");
        return Ok(());
    }
    println!("{:<48}  {:<22}  DIGEST", "URI", "NAME",);
    for img in &resp.images {
        println!(
            "{:<48}  {:<22}  {}",
            img.image_uri,
            img.config.as_ref().map(|c| c.name.as_str()).unwrap_or("?"),
            img.manifest_digest,
        );
    }
    Ok(())
}

/// ADR 0080: read + strictly parse an image-config TOML and convert it
/// to the proto shape. Parse errors (typos, retired manifest sections)
/// fail here with the serde message — same strictness as the server.
// `clippy::result_large_err`: `CliError` embeds `tonic::Status` (~176
// bytes); it's the CLI's one error type and this is a cold path.
#[allow(clippy::result_large_err)]
fn load_image_config(path: &Path) -> Result<app::ImageConfig, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::Other(format!("read {}: {e}", path.display())))?;
    let c: engram_core::types::image::ImageConfig = toml::from_str(&text).map_err(|e| {
        CliError::Other(format!(
            "{} is not a valid image config: {e}",
            path.display()
        ))
    })?;
    c.validate()
        .map_err(|e| CliError::Other(format!("{}: {e}", path.display())))?;
    Ok(image_config_to_proto(&c))
}

/// Core ImageConfig → the app-proto shape (the CLI-side mirror of the
/// coordinator's convert layer).
fn image_config_to_proto(c: &engram_core::types::image::ImageConfig) -> app::ImageConfig {
    use engram_core::types::image::NetworkDefault;
    use engram_core::types::CaptureEnvValue;
    app::ImageConfig {
        name: c.name.clone(),
        description: c.description.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        workdir: c.workdir.clone(),
        resources: Some(app::ImageResources {
            suggested_memory_mib: c.resources.suggested_memory_mib,
            suggested_vcpus: c.resources.suggested_vcpus,
            suggested_disk_gib: c.resources.suggested_disk_gib,
        }),
        warm: c.warm.as_ref().map(|w| app::ImageWarmConfig {
            command: w.command.clone(),
            timeout_secs: w.timeout_secs,
            workdir: w.workdir.clone(),
            env: w
                .env
                .iter()
                .map(|e| app::CaptureEnvEntry {
                    name: e.name.clone(),
                    value: Some(match &e.value {
                        CaptureEnvValue::Literal { value } => {
                            app::capture_env_entry::Value::Literal(value.clone())
                        }
                        CaptureEnvValue::SecretRef { secret_ref } => {
                            app::capture_env_entry::Value::SecretRef(secret_ref.clone())
                        }
                    }),
                })
                .collect(),
            network: w.network.as_ref().map(|n| app::ProfileNetwork {
                default: match n.default {
                    NetworkDefault::Allow => "allow".to_string(),
                    NetworkDefault::Deny => "deny".to_string(),
                },
                allow_hosts: n.allow_hosts.clone(),
                allow_host_patterns: n.allow_host_patterns.clone(),
            }),
        }),
    }
}

async fn image_enable(
    c: &mut Clients,
    uri: &str,
    config: Option<&Path>,
    json: bool,
    no_wait: bool,
) -> Result<(), CliError> {
    // ADR 0036: EnableImage records an enable job; the coordinator's
    // scanner drives the pipeline. Default UX polls the job to a
    // terminal state with a live chunk progress line.
    //
    // ADR 0080: `--config` supplies the image's full runtime config
    // (unset inherits the already-enabled row's; the server rejects a
    // first enable without one).
    let config = config.map(load_image_config).transpose()?;
    let resp = c
        .image
        .enable_image(app::EnableImageRequest {
            image_uri: uri.to_string(),
            config,
        })
        .await?
        .into_inner();
    let job = resp
        .job
        .ok_or_else(|| CliError::Other("enable response carried no job".into()))?;
    if no_wait {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&enable_job_to_json(&job))?
            );
        } else {
            println!(
                "enable job {} accepted; poll with `engram image poll-job {}`",
                job.id, job.id
            );
        }
        return Ok(());
    }
    poll_enable_job(c, &job.id, json).await
}

/// ADR 0080: full-replace config edit. Cheap edits apply immediately (no
/// job in the response); capture-affecting edits (resources/[warm]) come
/// back as an enable job to poll.
async fn image_update(
    c: &mut Clients,
    uri: &str,
    config: &Path,
    allow_recapture: bool,
    json: bool,
    no_wait: bool,
) -> Result<(), CliError> {
    let config = load_image_config(config)?;
    let resp = c
        .image
        .update_image(app::UpdateImageRequest {
            image_uri: uri.to_string(),
            config: Some(config),
            allow_recapture,
        })
        .await?
        .into_inner();
    match resp.job {
        None => {
            if json {
                println!("{}", serde_json::json!({ "applied": "immediate" }));
            } else {
                println!("config updated (no recapture needed; live immediately)");
            }
            Ok(())
        }
        Some(job) => {
            if no_wait {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&enable_job_to_json(&job))?
                    );
                } else {
                    println!("recapture job {} accepted", job.id);
                }
                return Ok(());
            }
            poll_enable_job(c, &job.id, json).await
        }
    }
}

fn enable_job_to_json(job: &app::EnableJob) -> Value {
    serde_json::json!({
        "id": job.id,
        "image_uri": job.image_uri,
        "manifest_digest": job.manifest_digest,
        "state": job.state,
        "chunks_total": job.chunks_total,
        "chunks_done": job.chunks_done,
        "attempts": job.attempts,
        "error": job.error,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "capture_phase": job.capture_phase,
        "warm_stage": job.warm_stage,
        "warm_stage_started_at": job.warm_stage_started_at,
        "output_tail": job.output_tail,
        // ADR 0036 amendment (issue #538): per-host prestage outcome map.
        // Wire-encoded as a JSON string; parse it back to a nested object
        // for `--json` output rather than double-encoding. Malformed
        // (shouldn't happen — the server always writes valid JSON, "{}"
        // by default) falls back to an empty object.
        "prestage_hosts": serde_json::from_str::<Value>(&job.prestage_hosts)
            .unwrap_or_else(|_| serde_json::json!({})),
    })
}

fn enable_job_progress(job: &app::EnableJob) -> String {
    if let Some(total) = job.chunks_total {
        if total > 0 {
            return format!("{}/{total} chunks", job.chunks_done);
        }
    }
    [job.capture_phase.as_deref(), job.warm_stage.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" / ")
}

fn print_enable_job(job: &app::EnableJob) {
    println!("id              : {}", job.id);
    println!("state           : {}", job.state);
    println!("image_uri       : {}", job.image_uri);
    if let Some(digest) = &job.manifest_digest {
        println!("manifest_digest : {digest}");
    }
    let progress = enable_job_progress(job);
    if !progress.is_empty() {
        println!("progress        : {progress}");
    }
    println!("attempts        : {}", job.attempts);
    if let Some(err) = &job.error {
        println!("error           : {err}");
    }
    if let Some(phase) = &job.capture_phase {
        println!("capture_phase   : {phase}");
    }
    if let Some(stage) = &job.warm_stage {
        println!("warm_stage      : {stage}");
    }
    if let Some(started_at) = &job.warm_stage_started_at {
        println!("warm_started_at : {started_at}");
    }
    println!("created_at      : {}", job.created_at);
    println!("updated_at      : {}", job.updated_at);
    if job.prestage_hosts != "{}" && !job.prestage_hosts.is_empty() {
        println!("prestage_hosts  : {}", job.prestage_hosts);
    }
    if let Some(output_tail) = &job.output_tail {
        if !output_tail.is_empty() {
            println!();
            println!("output_tail:");
            println!("{output_tail}");
        }
    }
}

async fn image_jobs(c: &mut Clients, json: bool) -> Result<(), CliError> {
    let resp = c
        .image
        .list_enable_jobs(app::ListEnableJobsRequest {})
        .await?
        .into_inner();
    if json {
        let jobs: Vec<Value> = resp.jobs.iter().map(enable_job_to_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "jobs": jobs }))?
        );
        return Ok(());
    }
    if resp.jobs.is_empty() {
        println!("(no recent enable jobs)");
        return Ok(());
    }
    println!(
        "{:<36}  {:<13}  {:<8}  {:<22}  URI",
        "ID", "STATE", "ATTEMPTS", "DETAIL"
    );
    for job in &resp.jobs {
        let detail = if job.state == "failed" {
            job.error.as_deref().unwrap_or("failed").to_string()
        } else {
            enable_job_progress(job)
        };
        println!(
            "{:<36}  {:<13}  {:<8}  {:<22}  {}",
            job.id,
            job.state,
            job.attempts,
            truncate(&detail, 22),
            truncate(&job.image_uri, 64),
        );
    }
    Ok(())
}

async fn image_job(c: &mut Clients, id: &str, json: bool) -> Result<(), CliError> {
    let resp = c
        .image
        .get_enable_job(app::GetEnableJobRequest {
            job_id: id.to_string(),
        })
        .await?
        .into_inner();
    let job = resp
        .job
        .ok_or_else(|| CliError::Other("get-enable-job response carried no job".into()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&enable_job_to_json(&job))?
        );
    } else {
        print_enable_job(&job);
    }
    Ok(())
}

async fn image_retry_job(
    c: &mut Clients,
    id: &str,
    json: bool,
    no_wait: bool,
) -> Result<(), CliError> {
    let resp = c
        .image
        .retry_enable_job(app::RetryEnableJobRequest {
            job_id: id.to_string(),
        })
        .await?
        .into_inner();
    let job = resp
        .job
        .ok_or_else(|| CliError::Other("retry-enable-job response carried no job".into()))?;
    if no_wait {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&enable_job_to_json(&job))?
            );
        } else {
            println!(
                "enable job {} requeued; poll with `engram image poll-job {}`",
                job.id, job.id
            );
        }
        return Ok(());
    }
    poll_enable_job(c, &job.id, json).await
}

/// Poll an enable job until `ready`/`failed`, rendering progress.
async fn poll_enable_job(c: &mut Clients, job_id: &str, json: bool) -> Result<(), CliError> {
    let mut printed_progress = false;
    loop {
        let resp = c
            .image
            .get_enable_job(app::GetEnableJobRequest {
                job_id: job_id.to_string(),
            })
            .await?
            .into_inner();
        let job = resp
            .job
            .ok_or_else(|| CliError::Other("get-enable-job carried no job".into()))?;
        match job.state.as_str() {
            "ready" => {
                if printed_progress {
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
                if printed_progress {
                    eprintln!();
                }
                let err = job.error.as_deref().unwrap_or("unknown error");
                return Err(CliError::Other(format!(
                    "enable job {job_id} failed: {err} \
                     (retry with `engram image retry-job {job_id}` after fixing the cause)"
                )));
            }
            state => {
                let progress = match job.chunks_total {
                    Some(t) if t > 0 => format!("{}/{t} chunks", job.chunks_done),
                    _ => String::new(),
                };
                eprint!("\r{state:<14} {progress:<24}");
                printed_progress = true;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn image_disable(c: &mut Clients, uri: &str) -> Result<(), CliError> {
    c.image
        .disable_image(app::DisableImageRequest {
            image_uri: uri.to_string(),
        })
        .await?;
    println!("disabled");
    Ok(())
}

async fn image_refresh(
    c: &mut Clients,
    uri: &str,
    recapture: bool,
    json: bool,
) -> Result<(), CliError> {
    let resp = c
        .image
        .refresh_image(app::RefreshImageRequest {
            image_uri: uri.to_string(),
            force_recapture: recapture,
        })
        .await?
        .into_inner();
    // ADR 0036: refresh is an enable job too — poll it like enable.
    let job = resp
        .job
        .ok_or_else(|| CliError::Other("refresh response carried no job".into()))?;
    poll_enable_job(c, &job.id, json).await
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

    /// Port of the old `format_sse_frame_*` tests to the gRPC
    /// `SessionEvent` → line formatter. The wire shape changed (SSE
    /// frames → a typed envelope), but the rendered line contract is the
    /// same: `[{idx}] {kind}: {payload}`, idx right-aligned, `-` when idx
    /// is unset, a well-formed JSON payload printed un-double-quoted, and
    /// a non-JSON payload rendered as a JSON string.
    fn ev(idx: Option<i64>, kind: &str, payload_json: &str) -> app::SessionEvent {
        app::SessionEvent {
            idx,
            kind: kind.to_string(),
            payload_json: payload_json.to_string(),
        }
    }

    #[test]
    fn format_event_line_pretty_prints_idx_kind_payload() {
        let out = format_event_line(&ev(
            Some(7),
            "exec_completed",
            "{\"exec_id\":\"x\",\"exit_status\":0}",
        ));
        assert!(out.contains("exec_completed"), "got {out}");
        assert!(out.contains("\"exec_id\":\"x\""), "got {out}");
        assert!(
            out.contains("[     7]"),
            "idx should be right-aligned: {out}"
        );
    }

    #[test]
    fn format_event_line_uses_dash_for_missing_idx() {
        // The idx-less `lagged` event renders with `-` in the idx column.
        let out = format_event_line(&ev(None, "lagged", "{\"missed\":3}"));
        assert!(out.contains("[     -]"), "got {out}");
        assert!(out.contains("lagged"), "got {out}");
    }

    #[test]
    fn format_event_line_treats_non_json_payload_as_string() {
        let out = format_event_line(&ev(Some(1), "raw", "not-json"));
        assert!(out.contains("\"not-json\""), "got {out}");
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

    #[test]
    fn resolve_bearer_prefers_flag_over_env() {
        assert_eq!(
            resolve_bearer(Some("flag-token")),
            Some("flag-token".to_string())
        );
    }

    #[test]
    fn resolve_bearer_takes_first_of_plural() {
        // Simulate the plural env parsing directly: the helper splits on
        // ',' and ' ' and takes the first non-empty token. Exercise the
        // split logic without mutating process env (which would race
        // parallel tests).
        let parsed: Option<String> = "tok-a, tok-b tok-c"
            .split([',', ' '])
            .map(str::trim)
            .find(|t| !t.is_empty())
            .map(str::to_string);
        assert_eq!(parsed, Some("tok-a".to_string()));
    }

    #[test]
    fn normalize_endpoint_prepends_http_when_scheme_missing() {
        // The stack sets ENGRAM_APP_GRPC_ADDR scheme-less (Tiltfile:
        // 127.0.0.1:50061); tonic's Endpoint::from_shared needs a scheme.
        assert_eq!(
            normalize_endpoint("127.0.0.1:50061"),
            "http://127.0.0.1:50061"
        );
        assert_eq!(
            normalize_endpoint("localhost:50061"),
            "http://localhost:50061"
        );
    }

    #[test]
    fn normalize_endpoint_preserves_existing_scheme() {
        assert_eq!(
            normalize_endpoint("http://localhost:50061"),
            "http://localhost:50061"
        );
        assert_eq!(
            normalize_endpoint("https://coord.example:443"),
            "https://coord.example:443"
        );
    }
}
