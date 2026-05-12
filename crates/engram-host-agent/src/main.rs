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

    /// TCP port the local egress proxy binds. iptables PREROUTING
    /// REDIRECT on this host sends guest tcp/443 here. `0` (default)
    /// disables egress filtering entirely: no proxy is spawned, no
    /// REDIRECT rules are installed, and guests reach the network
    /// directly. ADR 0006.
    #[arg(long, env = "ENGRAM_EGRESS_PROXY_PORT", default_value_t = 0)]
    egress_proxy_port: u16,

    /// Where the host-agent loads the deployment-wide egress-proxy
    /// CA from. Ignored when `--egress-proxy-port=0`. Production
    /// uses `gcp-secret-manager` with Workload Identity; dev uses
    /// `local-disk` (auto-generates on first boot).
    #[arg(
        long,
        env = "ENGRAM_EGRESS_CA_SOURCE",
        value_parser = parse_ca_source_choice,
        default_value = "local-disk"
    )]
    ca_source: CaSourceChoice,

    /// Name of the env var holding the CA cert PEM when
    /// `--ca-source=env`. Default `ENGRAM_EGRESS_CA_CERT_PEM`.
    #[arg(
        long,
        env = "ENGRAM_EGRESS_CA_CERT_VAR",
        default_value = "ENGRAM_EGRESS_CA_CERT_PEM"
    )]
    ca_cert_var: String,

    /// Name of the env var holding the CA key PEM when
    /// `--ca-source=env`. Default `ENGRAM_EGRESS_CA_KEY_PEM`.
    #[arg(
        long,
        env = "ENGRAM_EGRESS_CA_KEY_VAR",
        default_value = "ENGRAM_EGRESS_CA_KEY_PEM"
    )]
    ca_key_var: String,

    /// Directory the local-disk CA loader generates / reads from.
    /// Only used when `--ca-source=local-disk`. Default
    /// `<work_dir>/egress-ca`.
    #[arg(long, env = "ENGRAM_EGRESS_CA_DIR")]
    ca_dir: Option<PathBuf>,

    /// Fully-qualified Secret Manager path holding the CA cert
    /// PEM. Required when `--ca-source=gcp-secret-manager`.
    /// Example: `projects/cortex-prod/secrets/engram-egress-ca-cert/versions/latest`.
    #[arg(long, env = "ENGRAM_EGRESS_CA_GCP_CERT_SECRET")]
    ca_gcp_cert_secret: Option<String>,

    /// Fully-qualified Secret Manager path holding the CA key PEM.
    /// Required when `--ca-source=gcp-secret-manager`.
    #[arg(long, env = "ENGRAM_EGRESS_CA_GCP_KEY_SECRET")]
    ca_gcp_key_secret: Option<String>,
}

/// Choice of CA-loading backend. Extension points: AWS Secrets
/// Manager, HashiCorp Vault, Azure Key Vault — one variant + one
/// `CaSource` impl per backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaSourceChoice {
    Env,
    LocalDisk,
    GcpSecretManager,
}

fn parse_ca_source_choice(s: &str) -> Result<CaSourceChoice, String> {
    match s {
        "env" => Ok(CaSourceChoice::Env),
        "local-disk" => Ok(CaSourceChoice::LocalDisk),
        "gcp-secret-manager" => Ok(CaSourceChoice::GcpSecretManager),
        other => Err(format!(
            "unknown CA source `{other}` (expected env | local-disk | gcp-secret-manager)"
        )),
    }
}

#[tokio::main]
async fn main() -> Result<(), HostAgentError> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = HostAgentConfig {
        work_dir: cli.work_dir.clone(),
        warm_pool_size: cli.warm_pool_size,
        coordinator_endpoint: cli.coordinator.clone(),
        coordinator_token: cli.coordinator_token.clone(),
        ..HostAgentConfig::default()
    };

    // ADR 0007 Phase 5: generate a per-startup HostId early so the
    // FC config can stamp it on snapshots' `trace_host_hint` and
    // pass it as `--publish-trace-host` to the UFFD handler.
    // Cross-host trace replay keys off this id — a snapshot taken
    // by host A becomes restoreable on host B with B reusing A's
    // recorded trace.
    let host_id = engram_core::HostId::new();
    let sandbox: Arc<dyn SandboxBackend> = match cli.sandbox_backend {
        BackendChoice::Firecracker => {
            let kernel = cli.kernel_image_path.clone().ok_or_else(|| {
                HostAgentError::Config(
                    "ENGRAM_KERNEL_IMAGE_PATH (or --kernel-image-path) is required when \
                     --sandbox-backend=firecracker"
                        .into(),
                )
            })?;
            let mut fc_cfg = engram_sandbox_firecracker::FirecrackerConfig::with_kernel(kernel);
            fc_cfg.host_id = Some(host_id);
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

    // ADR 0007: blob backend + chunk store. The chunk store needs
    // to point at the same `BlobStorage` the coordinator's
    // image-builder writes to (typically a shared GCS bucket in
    // production, a shared `local_path` in single-machine dev).
    // Misconfiguration fails closed at startup — better than
    // pretending to be ready and failing every session create.
    let blob = engram_host_agent::blob::from_env()
        .await
        .map_err(|e| HostAgentError::Config(format!("blob backend: {e}")))?;
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let materialize_dir = cli.work_dir.join("chunked-rootfs");
    // ADR 0007 #3a: NVMe-backed chunk cache. Budget defaults to
    // 200 GiB; operators override via `ENGRAM_CHUNK_CACHE_BUDGET_BYTES`
    // for hosts with smaller NVMe or different sharing ratios.
    let chunk_cache = engram_chunk_store::ChunkCache::new(
        engram_chunk_store::cache::ChunkCacheConfig::from_env_or_default(
            cli.work_dir.join("chunk-cache"),
        ),
        chunk_store.clone(),
    );

    // OCI auth resolver. The standalone host-agent doesn't have
    // direct DB/KEK access, so it asks the coord to resolve
    // credentials via a `ResolveRegistryAuth` RPC over the existing
    // dialer connection. The `SessionHandle` is shared mutable
    // state — empty until the dialer connects, populated for the
    // lifetime of each connection. ADR 0007.
    let (ws_auth_resolver, auth_session_handle) = engram_host_agent::ws_auth::WsAuthResolver::new();
    let oci_cache_root = cli.work_dir.join("oci-cache");
    let oci_client = std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
        ws_auth_resolver,
    )));
    let image_cache =
        engram_host_agent::image_cache::ImageCache::open(oci_cache_root, (*oci_client).clone())
            .await
            .map_err(|e| HostAgentError::Config(format!("oci cache: {e}")))?;

    let mut agent = HostAgent::new(cfg, sandbox, cloud)
        .with_chunk_store(chunk_store, materialize_dir)
        .with_chunk_cache(chunk_cache)
        .with_image_cache(image_cache)
        .with_auth_session_handle(auth_session_handle)
        .with_host_id(host_id);
    // ADR 0007 Phase 4: opt-in NBD daemon. `ENGRAM_NBD_DEVICES`
    // is a comma-separated list of `/dev/nbdN` paths the daemon
    // allocates from. Empty / unset → keep the materialize-to-
    // file path. Production Packer images load `modprobe nbd
    // nbds_max=64` + set this env var to match.
    if let Ok(s) = std::env::var("ENGRAM_NBD_DEVICES") {
        let paths: Vec<std::path::PathBuf> = s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        if !paths.is_empty() {
            match engram_host_agent::disk_daemon::NbdSlotAllocator::from_paths(paths) {
                Ok(pool) => {
                    tracing::info!(
                        slots = pool.capacity(),
                        "NBD daemon enabled; chunked rootfs serves /dev/nbdN",
                    );
                    agent = agent.with_nbd_pool(pool);
                }
                Err(e) => {
                    return Err(HostAgentError::Config(format!(
                        "ENGRAM_NBD_DEVICES misconfigured: {e}"
                    )));
                }
            }
        }
    }
    if cli.egress_proxy_port > 0 {
        match build_host_egress(&cli).await {
            Ok(egress) => agent = agent.with_egress(Arc::new(egress)),
            Err(e) => {
                tracing::error!(error = %e, "egress proxy spawn failed; aborting");
                return Err(HostAgentError::Config(format!("egress: {e}")));
            }
        }
    } else {
        tracing::info!(
            "egress proxy disabled (--egress-proxy-port=0); guests have unfiltered network access"
        );
    }
    agent.run().await
}

async fn build_host_egress(cli: &Cli) -> Result<engram_host_agent::egress::HostEgress, String> {
    use std::sync::Arc;
    let source: Arc<dyn engram_egress_proxy::CaSource> = match cli.ca_source {
        CaSourceChoice::Env => Arc::new(engram_egress_proxy::EnvCaSource::new(
            cli.ca_cert_var.clone(),
            cli.ca_key_var.clone(),
        )),
        CaSourceChoice::LocalDisk => {
            let dir = cli
                .ca_dir
                .clone()
                .unwrap_or_else(|| cli.work_dir.join("egress-ca"));
            Arc::new(engram_egress_proxy::LocalDiskCaSource::new(dir))
        }
        CaSourceChoice::GcpSecretManager => {
            let cert = cli.ca_gcp_cert_secret.clone().ok_or_else(|| {
                "--ca-source=gcp-secret-manager requires --ca-gcp-cert-secret".to_string()
            })?;
            let key = cli.ca_gcp_key_secret.clone().ok_or_else(|| {
                "--ca-source=gcp-secret-manager requires --ca-gcp-key-secret".to_string()
            })?;
            Arc::new(
                engram_secrets_gcp::ca::GcpSecretManagerCaSource::new(cert, key)
                    .map_err(|e| format!("gcp-secret-manager source: {e}"))?,
            )
        }
    };
    let bind: std::net::SocketAddr = format!("0.0.0.0:{}", cli.egress_proxy_port)
        .parse()
        .map_err(|e| format!("parse bind addr: {e}"))?;
    engram_host_agent::egress::HostEgress::spawn(source, bind)
        .await
        .map_err(|e| e.to_string())
}

/// Initialise the global tracing subscriber.
///
/// Honors `ENGRAM_LOG_FORMAT` (`pretty`, the default, or `json` for
/// production Cloud Logging ingestion). Falls back to `RUST_LOG` for
/// the filter, then to `info,engram=debug` as a sensible local
/// default.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,engram=debug"));
    let json = matches!(
        std::env::var("ENGRAM_LOG_FORMAT").as_deref(),
        Ok("json") | Ok("JSON")
    );
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
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
