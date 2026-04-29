use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use engram_cloud_gcp::GcpCloud;
use engram_cloud_mock::MockCloud;
use engram_cloud_static::StaticCloud;
use engram_coordinator::{
    config::{CloudBackendChoice, RunMode, SandboxBackendChoice},
    image_registry::ImageRegistry,
    CoordinatorConfig, CoordinatorError, HostRegistry, Services,
};
use engram_core::traits::{CloudBackend, SandboxBackend, SecretStore};
use engram_core::HostId;
use engram_postgres::PostgresStore;
use engram_sandbox_firecracker::FirecrackerBackend;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::EnvSecretStore;

#[derive(Parser, Debug)]
#[command(name = "engram-coordinator", version, about)]
struct Cli {
    #[arg(long, env = "ENGRAM_BIND_ADDR", default_value = "0.0.0.0:8080")]
    bind_addr: String,

    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(
        long,
        env = "ENGRAM_MODE",
        default_value = "coordinator",
        value_parser = RunMode::parse,
    )]
    mode: RunMode,

    #[arg(
        long,
        env = "ENGRAM_CLOUD_BACKEND",
        default_value = "static",
        value_parser = CloudBackendChoice::parse,
    )]
    cloud_backend: CloudBackendChoice,

    /// Local-disk root for per-session snapshot directories and the
    /// image registry. Survives process restarts; not durable across
    /// host loss (cross-host durability is git, not snapshots).
    #[arg(long, env = "ENGRAM_LOCAL_PATH", default_value = "./var/engram")]
    local_path: PathBuf,

    #[arg(
        long,
        env = "ENGRAM_SANDBOX_WORK_DIR",
        default_value = "./var/sandboxes"
    )]
    sandbox_work_dir: PathBuf,

    #[arg(
        long,
        env = "ENGRAM_SANDBOX_BACKEND",
        default_value = "firecracker",
        value_parser = SandboxBackendChoice::parse,
    )]
    sandbox_backend: SandboxBackendChoice,

    #[arg(long, env = "ENGRAM_DEFAULT_IMAGE", default_value = "warm-bootstrap")]
    default_image_version: String,

    /// Path to a kernel image (vmlinux) Firecracker can boot. Required
    /// when `--sandbox-backend=firecracker`; ignored otherwise. Every
    /// microVM on this host boots the same kernel.
    #[arg(long, env = "ENGRAM_KERNEL_IMAGE_PATH")]
    kernel_image_path: Option<PathBuf>,

    /// Target warm-pool size per (repo, image_version). 0 = disable.
    #[arg(long, env = "ENGRAM_WARM_POOL_SIZE", default_value_t = 1)]
    warm_pool_size: u32,

    /// Comma-separated list of bearer tokens accepted on protected
    /// endpoints. Empty (the default) disables auth and is intended
    /// for local dev only. Production deployments populate this from
    /// `ENGRAM_AUTH_TOKENS`.
    #[arg(
        long,
        env = "ENGRAM_AUTH_TOKENS",
        value_delimiter = ',',
        default_value = ""
    )]
    auth_tokens: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), CoordinatorError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,engram=debug")),
        )
        .init();

    let cli = Cli::parse();

    let cfg = CoordinatorConfig {
        bind_addr: cli.bind_addr.clone(),
        database_url: cli.database_url.clone(),
        mode: cli.mode,
        cloud_backend: cli.cloud_backend,
        local_path: cli.local_path.clone(),
        sandbox_backend: cli.sandbox_backend,
        default_image_version: cli.default_image_version.clone(),
        default_warm_pool_size: cli.warm_pool_size,
        // clap's value_delimiter splits even an empty default into a
        // single "" entry — strip those so an unset env reads as truly
        // empty (auth-off) rather than "exactly one token: empty string".
        auth_tokens: cli
            .auth_tokens
            .iter()
            .filter(|t| !t.is_empty())
            .cloned()
            .collect(),
    };

    let pg = PostgresStore::connect(&cfg.database_url)
        .await
        .map_err(|e| CoordinatorError::Config(format!("postgres connect: {e}")))?;
    pg.migrate()
        .await
        .map_err(|e| CoordinatorError::Config(format!("migrate: {e}")))?;

    let cloud: Arc<dyn CloudBackend> = match cli.cloud_backend {
        CloudBackendChoice::Static => Arc::new(
            StaticCloud::detect()
                .map_err(|e| CoordinatorError::Config(format!("static cloud: {e}")))?,
        ),
        CloudBackendChoice::Gcp => Arc::new(
            GcpCloud::new().map_err(|e| CoordinatorError::Config(format!("gcp cloud: {e}")))?,
        ),
        CloudBackendChoice::Mock => Arc::new(MockCloud::new()),
    };

    if matches!(cli.mode, RunMode::Host) {
        // Pure host-agent mode is served by the engram-host-agent binary.
        return Err(CoordinatorError::Config(
            "use `engram-host-agent` for --mode=host; this binary serves coordinator/all".into(),
        ));
    }

    // Phase 3a: every coordinator-side SandboxBackend call routes
    // through HostRegistry. For --mode=all we register a local backend
    // synchronously at startup; for --mode=coordinator the registry
    // starts empty and hosts dial in via /api/hosts/connect.
    let host_registry = Arc::new(HostRegistry::new());

    if matches!(cli.mode, RunMode::All) {
        let raw_backend: Arc<dyn SandboxBackend> = match cli.sandbox_backend {
            SandboxBackendChoice::Firecracker => {
                let kernel = cli.kernel_image_path.clone().ok_or_else(|| {
                    CoordinatorError::Config(
                        "ENGRAM_KERNEL_IMAGE_PATH (or --kernel-image-path) is required when \
                         --sandbox-backend=firecracker"
                            .into(),
                    )
                })?;
                let fc_cfg = engram_sandbox_firecracker::FirecrackerConfig::with_kernel(kernel);
                Arc::new(FirecrackerBackend::new(
                    cli.sandbox_work_dir.clone(),
                    fc_cfg,
                ))
            }
            SandboxBackendChoice::Process => {
                tracing::warn!(
                    "starting with --sandbox-backend=process: commands will run as host \
                     subprocesses with NO isolation. Dev only — production uses --sandbox-backend=firecracker."
                );
                Arc::new(ProcessBackend::new(cli.sandbox_work_dir.clone()))
            }
        };
        // Wrap in PooledBackend so warm-pool semantics still apply in
        // --mode=all (the same wrapper that production multi-host
        // deployments run inside their host-agent process). Without
        // this, single-binary dev would lose sub-second session
        // checkout for repeat (repo, image_version) hits.
        let pooled_backend: Arc<dyn SandboxBackend> = Arc::new(
            engram_host_agent::pooled_backend::PooledBackend::new(raw_backend, cli.warm_pool_size),
        );
        let in_proc_host = HostId::new();
        host_registry.register(in_proc_host, pooled_backend);
        tracing::info!(
            host_id = %in_proc_host,
            warm_pool_size = cli.warm_pool_size,
            "registered in-process host (--mode=all bypasses the WS path)",
        );
    } else {
        tracing::info!(
            "coordinator started without a local backend; waiting for hosts on /api/hosts/connect"
        );
    }

    // Default dev wiring: env-var-backed SecretStore (pulls
    // `$GITHUB_TOKEN` etc. from the host shell), filesystem-backed
    // ImageRegistry under `<local_path>/images`. Production
    // deployments swap these out for `engram-secrets-gcp` / vault /
    // etc. via a config flag (next round).
    let secrets: Arc<dyn SecretStore> = Arc::new(EnvSecretStore::new());
    let images = ImageRegistry::new(cli.local_path.join("images"));

    let services = Services {
        meta: Arc::new(pg),
        cloud,
        sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
        secrets,
        images,
    };

    engram_coordinator::run_with_registry(cfg, services, host_registry).await
}
