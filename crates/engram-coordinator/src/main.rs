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

    /// Path to an arm64 Linux kernel image VZ (Virtualization.framework)
    /// can boot. Required when `--sandbox-backend=vz`; ignored
    /// otherwise. Default points at `~/.cache/engram-vz-test/vmlinux-arm64`,
    /// the location `just vz-pull-kernel` populates.
    #[arg(long, env = "ENGRAM_VZ_KERNEL_PATH")]
    vz_kernel_path: Option<PathBuf>,

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

    /// Local address the harness-channel TCP listener binds to.
    /// Default `127.0.0.1:0` lets the OS pick a free port.
    #[arg(long, env = "ENGRAM_HARNESS_LISTEN_ADDR", default_value = "127.0.0.1:0")]
    harness_listen_addr: std::net::SocketAddr,
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
        harness_listen_addr: cli.harness_listen_addr,
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
            SandboxBackendChoice::Vz => {
                #[cfg(target_os = "macos")]
                {
                    let kernel = cli
                        .vz_kernel_path
                        .clone()
                        .or_else(default_vz_kernel_path)
                        .ok_or_else(|| {
                            CoordinatorError::Config(
                                "ENGRAM_VZ_KERNEL_PATH (or --vz-kernel-path) is required when \
                                 --sandbox-backend=vz; default location \
                                 ~/.cache/engram-vz-test/vmlinux-arm64 does not exist (run \
                                 `just vz-pull-kernel`)"
                                    .into(),
                            )
                        })?;
                    let vz_cfg = engram_sandbox_vz::VzConfig::with_kernel(kernel);
                    Arc::new(
                        engram_sandbox_vz::VzBackend::new(cli.sandbox_work_dir.clone(), vz_cfg)
                            .map_err(|e| CoordinatorError::Config(format!("vz backend: {e}")))?,
                    )
                }
                #[cfg(not(target_os = "macos"))]
                {
                    return Err(CoordinatorError::Config(
                        "--sandbox-backend=vz only runs on macOS Apple Silicon. Use \
                         --sandbox-backend=firecracker on Linux"
                            .into(),
                    ));
                }
            }
            // The Process variant is a test-only fixture (see
            // `SandboxBackendChoice::Process` doc-comment). The CLI
            // parser already rejects `--sandbox-backend=process`, so
            // reaching this arm means a caller constructed the enum
            // directly — which production binaries never do.
            SandboxBackendChoice::Process => {
                return Err(CoordinatorError::Config(
                    "SandboxBackendChoice::Process is a test-only fixture and cannot be \
                     selected by the production binary"
                        .into(),
                ));
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
        // Use a stable HostId for `--mode=all` so a coordinator
        // restart picks up the same `hosts` row (FK-safe — sessions
        // / snapshots inserted in a prior run still reference a valid
        // host row). The hostname column is UNIQUE; without a stable
        // id, every restart would collide on `("in-process")` and
        // the row's id would diverge from the in-memory id we route
        // through, breaking snapshot inserts via FK.
        let in_proc_host: HostId = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000a11")
            .expect("stable in-proc host UUID must parse")
            .into();
        host_registry.register(in_proc_host, pooled_backend);
        // Persist a row in `hosts` so any FK-bearing insert (snapshots
        // record the host that wrote them, sessions track host_id)
        // doesn't trip on a phantom host. The dialer-driven multi-host
        // path inserts via the WS hello frame; --mode=all does it
        // synchronously here.
        let host_record = engram_core::types::HostRecord {
            id: in_proc_host,
            hostname: "in-process".into(),
            cloud_metadata: engram_core::types::host::HostMetadata::default(),
            capacity: engram_core::types::host::HostCapacity {
                total_gb: 0,
                used_gb: 0,
            },
            status: engram_core::types::host::HostStatus::Ready,
            last_heartbeat_at: chrono::Utc::now(),
        };
        if let Err(e) =
            engram_core::traits::MetadataStore::upsert_host(&pg, host_record).await
        {
            return Err(CoordinatorError::Config(format!(
                "register in-process host in postgres: {e}"
            )));
        }
        // Keep the row's `last_heartbeat_at` fresh so the dead-host
        // detector doesn't reap our own in-process host. The
        // multi-host path uses the WS dialer's heartbeat loop for
        // this; --mode=all stamps the timestamp directly.
        let pg_for_hb = pg.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let r = engram_core::types::HostRecord {
                    id: in_proc_host,
                    hostname: "in-process".into(),
                    cloud_metadata: engram_core::types::host::HostMetadata::default(),
                    capacity: engram_core::types::host::HostCapacity {
                        total_gb: 0,
                        used_gb: 0,
                    },
                    status: engram_core::types::host::HostStatus::Ready,
                    last_heartbeat_at: chrono::Utc::now(),
                };
                if let Err(e) =
                    engram_core::traits::MetadataStore::upsert_host(&pg_for_hb, r).await
                {
                    tracing::warn!(error = %e, "in-process heartbeat upsert failed");
                }
            }
        });
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

/// Default location for the arm64 Linux kernel `engram-sandbox-vz`
/// boots: `~/.cache/engram-vz-test/vmlinux-arm64`. Returns `None` if
/// `$HOME` isn't set or the file doesn't exist; the caller surfaces
/// a config error pointing at `just vz-pull-kernel`.
#[cfg(target_os = "macos")]
fn default_vz_kernel_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let candidate = std::path::PathBuf::from(home)
        .join(".cache")
        .join("engram-vz-test")
        .join("vmlinux-arm64");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}
