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

    /// Address the Prometheus `/metrics` exporter listens on.
    /// Doubles as the TCP target for the GCE MIG's autohealing
    /// health check (see fc-host-mig TF — `google_compute_health_check.host_agent`
    /// targets port 9100).
    #[arg(long, env = "ENGRAM_HOST_METRICS_ADDR", default_value = "0.0.0.0:9100")]
    metrics_addr: std::net::SocketAddr,

    /// Bearer token sent on the WS upgrade. Match the coordinator's
    /// `ENGRAM_AUTH_TOKENS`. Omit when the coordinator is in dev mode
    /// (auth disabled).
    #[arg(long, env = "ENGRAM_COORDINATOR_TOKEN")]
    coordinator_token: Option<String>,

    /// ADR 0013: bind address for the gRPC `HostService` server. The
    /// coord dials this from inside the VPC. `0.0.0.0:9101` by
    /// default; set to an empty string or `disabled` to skip the
    /// server entirely (`--mode=all`-style standalone dev where
    /// nothing's dispatching to us).
    #[arg(long, env = "ENGRAM_GRPC_LISTEN_ADDR", default_value = "0.0.0.0:9101")]
    grpc_listen_addr: String,

    /// ADR 0013: externally-routable URL the coord uses to dial the
    /// gRPC server (sent to coord in `POST /api/hosts/register`).
    /// When unset, host-agent auto-derives it: GCE metadata server
    /// → `http://<internal-ip>:<grpc-port>`; falls back to
    /// `http://127.0.0.1:<grpc-port>` for non-GCE dev runs.
    /// Set to an empty string to opt out of registration entirely.
    #[arg(long, env = "ENGRAM_GRPC_ADVERTISE_ADDR")]
    grpc_advertise_addr: Option<String>,

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

    // Bind the metrics port first. It doubles as the GCE MIG
    // autohealing health check's TCP target — without this listener
    // the autohealer fails every instance after the 180s grace
    // period and the MIG rolls in a tight loop.
    engram_host_agent::metrics::init(cli.metrics_addr);

    // ADR 0013: resolve the gRPC listen + advertise addrs.
    //
    // listen_addr: parse the CLI/env-supplied socket addr. Empty
    // string or "disabled" skips the gRPC server (mode=all-style
    // standalone dev where the host-agent serves only its in-proc
    // backend). Default 0.0.0.0:9101 is "always on" in production.
    //
    // advertise_addr: prefer ENGRAM_GRPC_ADVERTISE_ADDR; if unset,
    // try the GCE metadata server for the host's internal IP; if
    // neither yields a value, fall back to 127.0.0.1 with the
    // resolved listen port (covers dev-vm split-mode where the
    // coord and host-agent run on the same box).
    let (grpc_listen_addr, grpc_port) = parse_grpc_listen(&cli.grpc_listen_addr);
    let grpc_advertise_addr =
        resolve_advertise_addr(cli.grpc_advertise_addr.clone(), grpc_port).await;

    let cfg = HostAgentConfig {
        work_dir: cli.work_dir.clone(),
        coordinator_endpoint: cli.coordinator.clone(),
        coordinator_token: cli.coordinator_token.clone(),
        grpc_listen_addr,
        grpc_advertise_addr,
        ..HostAgentConfig::default()
    };

    // ADR 0007 Phase 5: generate a per-startup HostId early so the
    // FC config can stamp it on snapshots' `trace_host_hint` and
    // pass it as `--publish-trace-host` to the UFFD handler.
    // Cross-host trace replay keys off this id — a snapshot taken
    // by host A becomes restoreable on host B with B reusing A's
    // recorded trace.
    let host_id = engram_core::HostId::new();
    // ADR 0009 §6: when the backend is FC, keep a typed Arc on the
    // side so the host-agent's startup live-attach pass can call
    // `reattach_sandbox` (the trait can't downcast `dyn`). `None`
    // for non-FC backends — the live-attach pass becomes a no-op
    // and the host-agent starts clean-slate.
    let fc_for_reattach: Option<Arc<engram_sandbox_firecracker::FirecrackerBackend>>;
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
            // When the egress proxy is enabled, plumb the matching
            // TCP/443 port into the FC config so iptables installs
            // the REDIRECT rule (and the matching default-deny on
            // FORWARD). `egress_dns_port` stays at the default 5353
            // — operators don't need to override unless something
            // else on the host already binds that port.
            if cli.egress_proxy_port > 0 {
                fc_cfg.egress_proxy_port = Some(cli.egress_proxy_port);
            }
            // ADR 0014 follow-up: pin CPUID to a Cascade Lake baseline
            // so warm snapshots stay portable across the bake-host CPU
            // (AMD on Blacksmith runners) vs the prod-host CPU (Intel
            // Cascade Lake n2). Without this, prod 2026-05-21 hit
            // warm-restore guests whose glibc ifunc resolver picked
            // AMD-only AVX-512 paths the prod CPU couldn't execute,
            // segfaulting every shell exit. `ENGRAM_FC_CPU_TEMPLATE`
            // env var overrides (`""` / `"none"` for passthrough, any
            // other value for a custom template name).
            fc_cfg.cpu_template = engram_sandbox_firecracker::cpu_template_from_env();
            // ADR 0014 M1.12: each FC host maintains a 16 MiB empty
            // ext4 stub harness that warm-pool restore points the
            // harness symlink at. Content-identical to the one the
            // bake produces, so no transfer needed — every host
            // mke2fs's its own at startup. `swap_harness_drive`
            // re-points the symlink at the session's real harness
            // ext4 at warm-lease.
            let stub_path = cli.work_dir.join(".stub-harness.ext4");
            let stub_abs = ensure_stub_harness(&stub_path).await.map_err(|e| {
                HostAgentError::Config(format!(
                    "materialize stub harness at {}: {e}",
                    stub_path.display()
                ))
            })?;
            fc_cfg.stub_harness_path = Some(stub_abs);
            let fc = Arc::new(engram_sandbox_firecracker::FirecrackerBackend::new(
                cli.work_dir.clone(),
                fc_cfg,
            ));
            // Apply per-host iptables: inter-VM block, host-LAN drops,
            // proxy REDIRECTs (TCP/443 + DNS), and the
            // engram-default-deny that makes the proxy the only egress.
            // Without this, FC sessions still come up but nothing
            // touches iptables and the guest gets no NAT — every
            // outbound packet from the VM goes nowhere. Mirrors the
            // mode=all wiring in `engram-coordinator::main`.
            if let Err(e) = fc.host_startup().await {
                tracing::warn!(
                    error = %e,
                    "FC host_startup failed; per-VM networking will fail at session create. \
                     Check that the host-agent runs as root (or with CAP_NET_ADMIN) and \
                     iptables/ip are on PATH."
                );
            }
            fc_for_reattach = Some(fc.clone());
            fc
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
                fc_for_reattach = None;
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
    );

    // OCI auth resolver. The standalone host-agent doesn't have
    // direct DB/KEK access, so it asks the coord to resolve
    // credentials over HTTP (ADR 0013).
    // `HttpAuthResolver` POSTs to
    // `/api/hosts/:id/auth/resolve-registry`, the receiving coord
    // pod calls its existing `PgAuthResolver` and returns creds
    // (or `None` for anonymous registries). Plaintext creds
    // traverse the per-request HTTPS hop only at pull time —
    // never persisted on the host.
    let oci_cache_root = cli.work_dir.join("oci-cache");
    let coord_url_for_auth = cfg
        .coordinator_endpoint
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let auth_coord_client = engram_host_agent::coord_client::CoordClient::new(
        coord_url_for_auth,
        cfg.coordinator_token.clone(),
    );
    let http_auth_resolver =
        engram_host_agent::coord_client::HttpAuthResolver::new(auth_coord_client, host_id);
    let oci_client = std::sync::Arc::new(engram_oci::OciClient::new(http_auth_resolver));
    let image_cache =
        engram_host_agent::image_cache::ImageCache::open(oci_cache_root, (*oci_client).clone())
            .await
            .map_err(|e| HostAgentError::Config(format!("oci cache: {e}")))?;

    let mut agent = HostAgent::new(cfg, sandbox, cloud)
        .with_chunk_store(chunk_store, materialize_dir)
        .with_chunk_cache(chunk_cache)
        .with_image_cache(image_cache)
        .with_host_id(host_id);
    if let Some(fc) = fc_for_reattach {
        agent = agent.with_fc_reattach(fc);
    }
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

/// ADR 0014 M1.12: produce / verify the 16 MiB stub harness ext4 at
/// `<work_dir>/.stub-harness.ext4`. Warm-pool restore points the
/// harness symlink at this file so `load_snapshot` can open it as a
/// virtio-blk device; `swap_harness_drive` repoints to the session's
/// real harness at warm-lease. Idempotent — skips when the file is
/// already the expected size. Returns the **absolute** path; the
/// receiver's harness symlinks resolve relative to their own parent
/// directory (the bake's `/tmp/.tmpXXX/harness/`), so a relative
/// `./var/...` target would dangle there.
async fn ensure_stub_harness(path: &std::path::Path) -> Result<PathBuf, String> {
    const STUB_SIZE_BYTES: u64 = 16 * 1024 * 1024;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir stub parent {}: {e}", parent.display()))?;
    }
    let needs_build = match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len() != STUB_SIZE_BYTES,
        Err(_) => true,
    };
    if needs_build {
        let scratch = tempfile::tempdir().map_err(|e| format!("stub tempdir: {e}"))?;
        use engram_image_builder::{Ext4Packer, Mke2fsPacker};
        Mke2fsPacker::default()
            .pack(scratch.path(), path, STUB_SIZE_BYTES)
            .await
            .map_err(|e| format!("mke2fs stub harness: {e}"))?;
    }
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("canonicalize stub harness {}: {e}", path.display()))
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

/// Parse the gRPC listen address from `ENGRAM_GRPC_LISTEN_ADDR`.
/// Returns `(Some(addr), port)` for a normal binding, `(None, 0)`
/// when explicitly disabled by an empty string or `"disabled"`.
/// On a malformed value, logs a warning and disables (same as
/// explicit disable) rather than crashing — the host-agent's other
/// surfaces (heartbeat, register, harness events) can still run.
fn parse_grpc_listen(raw: &str) -> (Option<std::net::SocketAddr>, u16) {
    let s = raw.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("disabled") {
        return (None, 0);
    }
    match s.parse::<std::net::SocketAddr>() {
        Ok(addr) => (Some(addr), addr.port()),
        Err(e) => {
            tracing::warn!(
                value = %s,
                error = %e,
                "invalid ENGRAM_GRPC_LISTEN_ADDR; disabling gRPC server",
            );
            (None, 0)
        }
    }
}

/// Resolve the externally-routable URL the coord uses to dial us.
/// ADR 0013. Precedence:
///   1. The CLI/env `ENGRAM_GRPC_ADVERTISE_ADDR` value, if non-empty.
///      An explicit empty string opts out of registration.
///   2. The GCE metadata server's primary internal IP, with the
///      resolved gRPC port. Times out fast (500 ms) so a non-GCE dev
///      run doesn't pay the wait.
///   3. `http://127.0.0.1:<grpc_port>` — appropriate for the dev-vm
///      split-mode test where the coord runs on the same VM.
///
/// `grpc_port=0` (gRPC server disabled) returns `None` regardless of
/// the env var, since there's nothing to advertise.
async fn resolve_advertise_addr(cli_value: Option<String>, grpc_port: u16) -> Option<String> {
    if grpc_port == 0 {
        if cli_value.as_deref().is_some_and(|s| !s.is_empty()) {
            tracing::warn!("ENGRAM_GRPC_ADVERTISE_ADDR set but gRPC listener disabled; ignoring",);
        }
        return None;
    }
    if let Some(v) = cli_value {
        let v = v.trim().to_string();
        // Explicit empty string = opt out of registration.
        if v.is_empty() {
            tracing::info!("ENGRAM_GRPC_ADVERTISE_ADDR is empty; skipping host registration");
            return None;
        }
        return Some(v);
    }
    // Try GCE metadata server with a short timeout.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .ok()?;
    let metadata_url =
        "http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/ip";
    match client
        .get(metadata_url)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(ip) => {
                let ip = ip.trim();
                if ip.is_empty() {
                    tracing::warn!("GCE metadata returned empty IP; falling back to 127.0.0.1");
                    Some(format!("http://127.0.0.1:{grpc_port}"))
                } else {
                    let addr = format!("http://{ip}:{grpc_port}");
                    tracing::info!(
                        advertise_addr = %addr,
                        "resolved gRPC advertise addr from GCE metadata",
                    );
                    Some(addr)
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "GCE metadata body read failed; falling back");
                Some(format!("http://127.0.0.1:{grpc_port}"))
            }
        },
        _ => {
            tracing::info!(
                "GCE metadata server unreachable; advertising http://127.0.0.1:{grpc_port} \
                 (set ENGRAM_GRPC_ADVERTISE_ADDR explicitly for non-GCE multi-host runs)",
            );
            Some(format!("http://127.0.0.1:{grpc_port}"))
        }
    }
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
