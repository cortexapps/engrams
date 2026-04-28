use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use engram_cloud_static::StaticCloud;
use engram_host_agent::{HostAgent, HostAgentConfig, HostAgentError};
use engram_sandbox_process::ProcessBackend;

#[derive(Parser, Debug)]
#[command(name = "engram-host-agent", version, about)]
struct Cli {
    /// Working directory for sandbox state, Firecracker sockets, and
    /// per-sandbox cwds (depending on which SandboxBackend is wired in).
    #[arg(
        long,
        env = "ENGRAM_SANDBOX_WORK_DIR",
        default_value = "./var/sandboxes"
    )]
    work_dir: PathBuf,

    /// Coordinator endpoint to dial via WebSocket.
    #[arg(long, env = "ENGRAM_COORDINATOR_ENDPOINT")]
    coordinator: Option<String>,

    /// Bearer token sent on the WS upgrade. Match the coordinator's
    /// `ENGRAM_AUTH_TOKENS`. Omit when the coordinator is in dev mode
    /// (auth disabled).
    #[arg(long, env = "ENGRAM_COORDINATOR_TOKEN")]
    coordinator_token: Option<String>,

    /// Default warm-pool size per active repo.
    #[arg(long, env = "ENGRAM_WARM_POOL_SIZE", default_value_t = 2)]
    warm_pool_size: u32,
}

#[tokio::main]
async fn main() -> Result<(), HostAgentError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,engram=debug")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = HostAgentConfig {
        work_dir: cli.work_dir.clone(),
        warm_pool_size: cli.warm_pool_size,
        coordinator_endpoint: cli.coordinator,
        coordinator_token: cli.coordinator_token,
        ..HostAgentConfig::default()
    };

    // The standalone host-agent binary defaults to the dev-loop process
    // backend. Production deployments run the host agent under
    // `engram-coordinator --mode=host` (Phase 3) which selects the
    // backend explicitly.
    let sandbox = Arc::new(ProcessBackend::new(cli.work_dir));
    let cloud = Arc::new(StaticCloud::detect().map_err(HostAgentError::Backend)?);

    HostAgent::new(cfg, sandbox, cloud).run().await
}
