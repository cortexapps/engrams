use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use engram_cloud_gcp::GcpCloud;
use engram_cloud_mock::MockCloud;
use engram_cloud_static::StaticCloud;
use engram_coordinator::{
    config::{CloudBackendChoice, RunMode, SandboxBackendChoice, StorageBackendChoice},
    image_registry::ImageRegistry,
    run, CoordinatorConfig, CoordinatorError, Services,
};
use engram_core::traits::{BlobStorage, CloudBackend, SandboxBackend, SecretStore};
use engram_postgres::PostgresStore;
use engram_sandbox_firecracker::FirecrackerBackend;
use engram_sandbox_process::ProcessBackend;
use engram_secrets_dev::EnvSecretStore;
use engram_storage_gcs::GcsStorage;
use engram_storage_local::LocalStorage;
use engram_storage_s3::S3Storage;

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

    #[arg(
        long,
        env = "ENGRAM_STORAGE_BACKEND",
        default_value = "local",
        value_parser = StorageBackendChoice::parse,
    )]
    storage_backend: StorageBackendChoice,

    #[arg(
        long,
        env = "ENGRAM_STORAGE_LOCAL_PATH",
        default_value = "./var/snapshots"
    )]
    storage_local_path: PathBuf,

    #[arg(long, env = "ENGRAM_STORAGE_GCS_BUCKET")]
    storage_gcs_bucket: Option<String>,

    #[arg(long, env = "ENGRAM_STORAGE_S3_BUCKET")]
    storage_s3_bucket: Option<String>,

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
        storage_backend: cli.storage_backend,
        storage_local_path: cli.storage_local_path.clone(),
        storage_gcs_bucket: cli.storage_gcs_bucket.clone(),
        storage_s3_bucket: cli.storage_s3_bucket.clone(),
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

    let blob: Arc<dyn BlobStorage> = match cli.storage_backend {
        StorageBackendChoice::Local => Arc::new(LocalStorage::new(cli.storage_local_path.clone())),
        StorageBackendChoice::Gcs => {
            let bucket = cli.storage_gcs_bucket.clone().ok_or_else(|| {
                CoordinatorError::Config("ENGRAM_STORAGE_GCS_BUCKET required".into())
            })?;
            Arc::new(GcsStorage::new(bucket))
        }
        StorageBackendChoice::S3 => {
            let bucket = cli.storage_s3_bucket.clone().ok_or_else(|| {
                CoordinatorError::Config("ENGRAM_STORAGE_S3_BUCKET required".into())
            })?;
            Arc::new(S3Storage::new(bucket))
        }
    };

    let sandbox: Arc<dyn SandboxBackend> = match cli.sandbox_backend {
        SandboxBackendChoice::Firecracker => {
            Arc::new(FirecrackerBackend::new(cli.sandbox_work_dir))
        }
        SandboxBackendChoice::Process => {
            tracing::warn!(
                "starting with --sandbox-backend=process: commands will run as host \
                 subprocesses with NO isolation. Dev only — production uses --sandbox-backend=firecracker."
            );
            Arc::new(ProcessBackend::new(cli.sandbox_work_dir))
        }
    };

    // Default dev wiring: env-var-backed SecretStore (pulls
    // `$GITHUB_TOKEN` etc. from the host shell), filesystem-backed
    // ImageRegistry under `<storage_local_path>/images`. Production
    // deployments swap these out for `engram-secrets-gcp` / vault /
    // etc. via a config flag (next round).
    let secrets: Arc<dyn SecretStore> = Arc::new(EnvSecretStore::new());
    let images = ImageRegistry::new(cli.storage_local_path.join("images"));

    let services = Services {
        meta: Arc::new(pg),
        blob,
        cloud,
        sandbox,
        secrets,
        images,
    };

    if matches!(cli.mode, RunMode::Host) {
        // Pure host-agent mode is served by the engram-host-agent binary.
        return Err(CoordinatorError::Config(
            "use `engram-host-agent` for --mode=host; this binary serves coordinator/all".into(),
        ));
    }

    // TODO(phase-3): when mode == All, also spawn engram_host_agent::HostAgent
    // on the same process so single-binary single-host dev works.
    run(cfg, services).await
}
