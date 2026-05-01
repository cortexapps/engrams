use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use engram_cloud_static::StaticCloud;
use engram_core::traits::SandboxBackend;
use engram_host_agent::{HostAgent, HostAgentConfig, HostAgentError};

/// Picks which `SandboxBackend` the host-agent wraps. Mirrors the
/// production half of `engram-coordinator`'s `--sandbox-backend`
/// (FC on Linux, VZ on macOS Apple Silicon). The Process backend is
/// a test-only fixture and is intentionally not selectable from the
/// CLI; multi-host deployments always run a real VMM.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendChoice {
    Firecracker,
    Vz,
}

impl BackendChoice {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "firecracker" => Ok(Self::Firecracker),
            "vz" => Ok(Self::Vz),
            other => Err(format!(
                "invalid sandbox backend `{other}` — expected `firecracker` or `vz`"
            )),
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "engram-host-agent", version, about)]
struct Cli {
    /// Working directory for sandbox state, Firecracker sockets, and
    /// per-sandbox cwds. Each sandbox carves out a subdirectory here.
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

    /// Default warm-pool size per active image_version.
    #[arg(long, env = "ENGRAM_WARM_POOL_SIZE", default_value_t = 2)]
    warm_pool_size: u32,

    /// Which sandbox backend to wrap. `firecracker` (Linux+KVM) or
    /// `vz` (macOS Apple Silicon). The Process backend is a test
    /// fixture and is intentionally not selectable here.
    #[arg(
        long,
        env = "ENGRAM_SANDBOX_BACKEND",
        default_value = "firecracker",
        value_parser = BackendChoice::parse,
    )]
    sandbox_backend: BackendChoice,

    /// Path to a kernel image (vmlinux) Firecracker can boot.
    /// Required when `--sandbox-backend=firecracker`. Every microVM
    /// on this host boots the same kernel.
    #[arg(long, env = "ENGRAM_KERNEL_IMAGE_PATH")]
    kernel_image_path: Option<PathBuf>,

    /// Path to an arm64 Linux kernel image VZ can boot. Required
    /// when `--sandbox-backend=vz`. Default points at
    /// `~/.cache/engram-vz-test/vmlinux-arm64` (populated by
    /// `just vz-pull-kernel`).
    #[arg(long, env = "ENGRAM_VZ_KERNEL_PATH")]
    vz_kernel_path: Option<PathBuf>,
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
        coordinator_endpoint: cli.coordinator.clone(),
        coordinator_token: cli.coordinator_token.clone(),
        ..HostAgentConfig::default()
    };

    let sandbox: Arc<dyn SandboxBackend> = match cli.sandbox_backend {
        BackendChoice::Firecracker => {
            let kernel = cli.kernel_image_path.clone().ok_or_else(|| {
                HostAgentError::Config(
                    "ENGRAM_KERNEL_IMAGE_PATH (or --kernel-image-path) is required when \
                     --sandbox-backend=firecracker"
                        .into(),
                )
            })?;
            let fc_cfg = engram_sandbox_firecracker::FirecrackerConfig::with_kernel(kernel);
            Arc::new(engram_sandbox_firecracker::FirecrackerBackend::new(
                cli.work_dir.clone(),
                fc_cfg,
            ))
        }
        BackendChoice::Vz => {
            #[cfg(target_os = "macos")]
            {
                let kernel = cli
                    .vz_kernel_path
                    .clone()
                    .or_else(default_vz_kernel_path)
                    .ok_or_else(|| {
                        HostAgentError::Config(
                            "ENGRAM_VZ_KERNEL_PATH (or --vz-kernel-path) is required when \
                             --sandbox-backend=vz; default location \
                             ~/.cache/engram-vz-test/vmlinux-arm64 does not exist (run \
                             `just vz-pull-kernel`)"
                                .into(),
                        )
                    })?;
                let vz_cfg = engram_sandbox_vz::VzConfig::with_kernel(kernel);
                Arc::new(
                    engram_sandbox_vz::VzBackend::new(cli.work_dir.clone(), vz_cfg)
                        .map_err(|e| HostAgentError::Config(format!("vz backend: {e}")))?,
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = cli.vz_kernel_path;
                return Err(HostAgentError::Config(
                    "--sandbox-backend=vz only runs on macOS Apple Silicon. Use \
                     --sandbox-backend=firecracker on Linux"
                        .into(),
                ));
            }
        }
    };
    let cloud = Arc::new(StaticCloud::detect().map_err(HostAgentError::Backend)?);

    HostAgent::new(cfg, sandbox, cloud).run().await
}

/// Default location for the arm64 Linux kernel `engram-sandbox-vz`
/// boots: `~/.cache/engram-vz-test/vmlinux-arm64`. Returns `None` if
/// `$HOME` isn't set or the file doesn't exist; the caller surfaces
/// a config error pointing at `just vz-pull-kernel`.
#[cfg(target_os = "macos")]
fn default_vz_kernel_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home)
        .join(".cache")
        .join("engram-vz-test")
        .join("vmlinux-arm64");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}
