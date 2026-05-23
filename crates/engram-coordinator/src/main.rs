use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use engram_cloud_gcp::GcpCloud;
use engram_cloud_mock::MockCloud;
use engram_cloud_static::StaticCloud;
use engram_coordinator::{
    config::{CloudBackendChoice, RunMode, SandboxBackendChoice},
    CoordinatorConfig, CoordinatorError, HostRegistry, Services,
};
use engram_core::traits::{CloudBackend, HostClient, SandboxBackend, SecretStore};
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
    #[arg(
        long,
        env = "ENGRAM_HARNESS_LISTEN_ADDR",
        default_value = "127.0.0.1:0"
    )]
    harness_listen_addr: std::net::SocketAddr,

    /// Address the Prometheus `/metrics` exporter listens on.
    /// Separate port from the main API so scrapers reach a
    /// bearer-free endpoint without going through nginx + IAP.
    #[arg(long, env = "ENGRAM_METRICS_ADDR", default_value = "0.0.0.0:9090")]
    metrics_addr: std::net::SocketAddr,

    /// Engram CIDR pool — every Firecracker sandbox gets a unique
    /// /30 carved from this. Defaults to 10.200.0.0/16 (16k slots).
    /// Override if you're already using 10.200.0.0/16 on this host.
    #[arg(long, env = "ENGRAM_FC_NET_CIDR", default_value = "10.200.0.0")]
    fc_net_cidr: std::net::Ipv4Addr,

    /// `--mode=all` only. TCP port the in-process egress proxy
    /// listens on for the locally-attached FC backend. iptables
    /// PREROUTING REDIRECT on the same host sends VM→tcp/443 here.
    /// `0` (default) disables egress filtering for `--mode=all`.
    /// Ignored in `--mode=coordinator` — host-agents own their own
    /// proxies (ADR 0006), configured via the host-agent's own
    /// `--egress-proxy-port` flag.
    #[arg(long, env = "ENGRAM_EGRESS_PROXY_PORT", default_value_t = 0)]
    egress_proxy_port: u16,

    /// KEK provider for envelope-encrypting registry credentials.
    /// `env-var` (default) reads a 32-byte key from `--kek-env-var`
    /// (default name `ENGRAM_KEK_MASTER_KEY`, base64-encoded).
    /// `gcp-kms` defers wrap/unwrap to a GCP KMS key (stub today).
    #[arg(long, env = "ENGRAM_KEK_PROVIDER", value_parser = parse_kek_choice, default_value = "env-var")]
    kek_provider: KekChoice,

    /// Env var name to read the base64-encoded master key from when
    /// `--kek-provider env-var` is in effect.
    #[arg(
        long,
        env = "ENGRAM_KEK_ENV_VAR",
        default_value = "ENGRAM_KEK_MASTER_KEY"
    )]
    kek_env_var: String,

    /// GCP KMS key resource path (e.g.
    /// `projects/p/locations/global/keyRings/r/cryptoKeys/k`) when
    /// `--kek-provider gcp-kms` is in effect. Required for that mode.
    #[arg(long, env = "ENGRAM_KEK_GCP_RESOURCE")]
    kek_gcp_resource: Option<String>,

    /// Per-session SecretStore backend. `env` reads `$NAME` from the
    /// coordinator's host environment (dev / single-tenant). `gcp`
    /// resolves each image manifest's `[secrets.*]` entry via GCP
    /// Secret Manager at session-create time, authenticated through
    /// the instance metadata server (Workload Identity in GKE).
    #[arg(long, env = "ENGRAM_SECRETS_BACKEND", value_parser = parse_secrets_choice, default_value = "env")]
    secrets_backend: SecretsChoice,

    /// GCP project ID for `--secrets-backend=gcp`. Required when that
    /// backend is selected.
    #[arg(long, env = "ENGRAM_GCP_PROJECT_ID")]
    gcp_project_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KekChoice {
    EnvVar,
    GcpKms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecretsChoice {
    Env,
    Gcp,
}

fn parse_secrets_choice(s: &str) -> Result<SecretsChoice, String> {
    match s {
        "env" => Ok(SecretsChoice::Env),
        "gcp" => Ok(SecretsChoice::Gcp),
        other => Err(format!(
            "unknown secrets backend `{other}` (expected env | gcp)"
        )),
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

fn parse_kek_choice(s: &str) -> Result<KekChoice, String> {
    match s {
        "env" | "env-var" | "envvar" => Ok(KekChoice::EnvVar),
        "gcp-kms" | "gcp" => Ok(KekChoice::GcpKms),
        other => Err(format!(
            "unknown KEK provider `{other}` (expected env-var | gcp-kms)"
        )),
    }
}

#[tokio::main]
async fn main() -> Result<(), CoordinatorError> {
    init_tracing();

    let cli = Cli::parse();

    // Bring up the metrics exporter early so any later init step
    // (postgres connect, KEK load, etc.) can record startup
    // counters / timings. Bind failures are logged + swallowed
    // inside `init`; we don't want a busy 9090 to keep the coord
    // from coming up.
    engram_coordinator::metrics::init(cli.metrics_addr);

    let cfg = CoordinatorConfig {
        bind_addr: cli.bind_addr.clone(),
        database_url: cli.database_url.clone(),
        mode: cli.mode,
        cloud_backend: cli.cloud_backend,
        local_path: cli.local_path.clone(),
        sandbox_backend: cli.sandbox_backend,
        default_image_version: cli.default_image_version.clone(),
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
    // Trait-object Arc shared with both the OCI auth resolver
    // (constructed below, before the sandbox backend) and
    // `Services.meta` (further down). PostgresStore wraps a sqlx
    // PgPool that's already cheaply cloneable, so duplicating into
    // the Arc + keeping a local handle for direct trait calls (the
    // host-row heartbeat loop below) is free.
    let meta_arc: Arc<dyn engram_core::traits::MetadataStore> = Arc::new(pg.clone());

    // KEK provider for envelope-encrypted registry credentials.
    // Built early so the OCI auth resolver can reference it.
    // Hard-fail at startup if env-var mode is selected and the env
    // var is missing — encrypting registry passwords with a missing
    // key has worse failure modes than just refusing to start.
    let kek: Arc<dyn engram_crypto::MasterKeyProvider> = match cli.kek_provider {
        KekChoice::EnvVar => Arc::new(
            engram_crypto::EnvVarKeyProvider::from_env(&cli.kek_env_var).map_err(|e| {
                CoordinatorError::Config(format!("KEK env-var `{}`: {e}", cli.kek_env_var))
            })?,
        ),
        KekChoice::GcpKms => {
            let resource = cli.kek_gcp_resource.as_deref().ok_or_else(|| {
                CoordinatorError::Config(
                    "--kek-provider gcp-kms requires --kek-gcp-resource".into(),
                )
            })?;
            Arc::new(
                engram_crypto::GcpKmsProvider::new(resource)
                    .map_err(|e| CoordinatorError::Config(format!("KEK gcp-kms: {e}")))?,
            )
        }
    };
    tracing::info!(provider = ?cli.kek_provider, key_id = %kek.key_id(), "KEK initialised");

    // Build the OCI client up-front. The same instance is shared by
    // (a) the host-agent's image_cache in `--mode=all`, and (b) the
    // coordinator's `/api/enabled-images` enable + refresh handlers.
    // PgAuthResolver dispatches per `auth_kind` — static (decrypt
    // under KEK), GCP Workload Identity, anonymous (short-circuited).
    let auth_resolver: Arc<dyn engram_oci::RegistryAuthResolver> = Arc::new(
        engram_oci_auth::PgAuthResolver::new(meta_arc.clone(), kek.clone()),
    );
    let oci_client = Arc::new(engram_oci::OciClient::new(auth_resolver.clone()));

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

    // ADR 0007 orphan-reap admin endpoint needs to know which
    // local dir the in-process host-agent materializes into. Only
    // populated for `--mode=all`; in `--mode=coordinator` the
    // materialized files live on each host and the reap is a
    // future multi-host RPC.
    let coord_materialize_dir = if matches!(cli.mode, RunMode::All) {
        Some(cli.local_path.join("chunked-rootfs"))
    } else {
        None
    };

    // ADR 0007: blob storage. The Arc backs the chunk store
    // (manifests + content-addressed chunks live here). Hoisting
    // before --mode=all wiring lets the in-process host-agent
    // share one connection. `local` (default) writes under
    // `<local_path>/blobs/`; `gcs` requires `ENGRAM_GCS_BUCKET`
    // and honors `STORAGE_EMULATOR_HOST` for fake-gcs-server in
    // `just dev`.
    let blob = match engram_coordinator::blob::from_env().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "blob backend init failed; aborting");
            std::process::exit(1);
        }
    };

    // Phase 3a: every coordinator-side SandboxBackend call routes
    // through HostRegistry. For --mode=all we register a local backend
    // synchronously at startup; for --mode=coordinator the registry
    // starts empty and hosts dial in via /api/hosts/connect.
    let host_registry = Arc::new(HostRegistry::new(meta_arc.clone()));

    // ADR 0007 Phase 5: stable HostId for `--mode=all`. Hoisted
    // up here (was computed below alongside the host_registry
    // register) so the FC backend can stamp it on its config
    // before construction. Cross-host trace replay (snapshot-on-
    // host-A → restore-on-host-B reuses A's trace) keys off the
    // matching id on both sides.
    let in_proc_host: HostId = uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000a11")
        .expect("stable in-proc host UUID must parse")
        .into();

    // Captured inside the `--mode=all` branch; threaded to
    // `run_with_registry_and_local` below so AppState's `HarnessHub`
    // is in scope when we wrap it as a `LocalHostClient` and register
    // it.
    let mut in_proc_local_backend: Option<Arc<dyn SandboxBackend>> = None;

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
                let mut fc_cfg = engram_sandbox_firecracker::FirecrackerConfig::with_kernel(kernel);
                fc_cfg.net_pool = Some(cli.fc_net_cidr);
                fc_cfg.egress_proxy_port = if cli.egress_proxy_port == 0 {
                    None
                } else {
                    Some(cli.egress_proxy_port)
                };
                fc_cfg.host_id = Some(in_proc_host);
                // ADR 0014 follow-up: pin CPUID to a Cascade Lake
                // baseline so snapshots stay portable across host CPU
                // changes (bake-runner vendor, MIG-driven instance
                // rolls, region expansion). Without this, prod 2026-
                // 05-21 hit AMD-bake → Intel-restore segfaults in
                // every guest shell. `ENGRAM_FC_CPU_TEMPLATE` overrides
                // (`""` / `"none"` disables, anything else passes
                // through verbatim).
                fc_cfg.cpu_template = engram_sandbox_firecracker::cpu_template_from_env();
                let fc = Arc::new(FirecrackerBackend::new(
                    cli.sandbox_work_dir.clone(),
                    fc_cfg,
                ));
                // Apply once-per-host iptables setup: enable IP
                // forwarding + install the inter-VM DROP rule.
                // Idempotent — safe across coord restarts.
                if let Err(e) = fc.host_startup().await {
                    tracing::warn!(
                        error = %e,
                        "FC host_startup failed; per-VM networking will fail at session create. \
                         Check that the coord runs as root (or with CAP_NET_ADMIN) and \
                         iptables/ip are on PATH."
                    );
                }
                fc as Arc<dyn SandboxBackend>
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
                    // ADR 0007: attach the chunk store so snapshots
                    // chunk the rootfs and report the manifest ref.
                    // Shares the same `blob` Arc as the rest of the
                    // process so chunks the bake produced are readable
                    // here (and vice versa).
                    let cs = engram_chunk_store::ChunkStore::new(blob.clone());
                    Arc::new(
                        engram_sandbox_vz::VzBackend::new(cli.sandbox_work_dir.clone(), vz_cfg)
                            .map_err(|e| CoordinatorError::Config(format!("vz backend: {e}")))?
                            .with_chunk_store(cs),
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
        };
        // Wrap in PooledBackend so the chunked-OCI / image-cache /
        // egress / chunk-store wiring is shared between `--mode=all`
        // (single-binary dev) and production host-agents (which build
        // the same wrapper in `engram-host-agent::lib::run`).
        //
        // The cache root lives under `<local_path>/oci-cache` so it
        // doesn't collide with the legacy on-disk image registry tree.
        // Reuses the OCI client built up-front (also shared with
        // `/api/enabled-images`).
        let oci_cache_root = cli.local_path.join("oci-cache");
        let image_cache =
            engram_host_agent::image_cache::ImageCache::open(oci_cache_root, (*oci_client).clone())
                .await
                .map_err(|e| CoordinatorError::Config(format!("oci cache: {e}")))?;
        // ADR 0006: --mode=all gets a local HostEgress so the
        // single-binary dev loop and the multi-host production
        // topology share one egress code path. `egress_proxy_port=0`
        // (default) skips it.
        let host_egress = if cli.egress_proxy_port > 0 {
            let dir = cli.local_path.join("egress-ca");
            let source: Arc<dyn engram_egress_proxy::CaSource> =
                Arc::new(engram_egress_proxy::LocalDiskCaSource::new(dir));
            let bind: std::net::SocketAddr = format!("0.0.0.0:{}", cli.egress_proxy_port)
                .parse()
                .expect("egress-proxy-port maps to a valid SocketAddr");
            match engram_host_agent::egress::HostEgress::spawn(source, bind).await {
                Ok(e) => Some(Arc::new(e)),
                Err(e) => {
                    tracing::error!(error = %e, "--mode=all egress proxy spawn failed; aborting");
                    return Err(CoordinatorError::Config(format!("egress: {e}")));
                }
            }
        } else {
            None
        };
        // ADR 0007: chunked-rootfs materialization root. In
        // `--mode=all`, coordinator + host-agent share one process,
        // so the chunk store wired into the PooledBackend uses the
        // same `blob` Arc the coordinator's Services consumes.
        // Multi-host deployments wire each host-agent's chunk store
        // independently, pointing at the same backing bucket via env.
        let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());
        // The materialize dir lives at `<local_path>/chunked-rootfs/`
        // — defined inside this block but ALSO consumed by the
        // Services wiring outside it (so the admin orphan-reap
        // endpoint knows the path). Pulled out below via the
        // `coord_materialize_dir` binding.
        let materialize_dir = cli.local_path.join("chunked-rootfs");
        // ADR 0007 #3a: NVMe-backed chunk cache. Amortises repeat
        // reads for chunks shared across manifests (canonical-base
        // images, fork lineage). Budget defaults to 200 GiB; smaller
        // hosts (dev VMs, lab boxes) override via
        // `ENGRAM_CHUNK_CACHE_BUDGET_BYTES`.
        let chunk_cache = engram_chunk_store::ChunkCache::new(
            engram_chunk_store::cache::ChunkCacheConfig::from_env_or_default(
                cli.local_path.join("chunk-cache"),
            ),
        );
        // ADR 0007 Phase 4: optional NBD daemon for chunked
        // rootfs. `ENGRAM_NBD_DEVICES` is a comma-separated list
        // of `/dev/nbdN` paths the host-agent allocates from when
        // serving sessions whose image bundle carries a
        // `disk_manifest`. Empty / unset → fall back to the
        // materialize-to-file path (still correct, just slower
        // cold start). Production Packer images load
        // `modprobe nbd nbds_max=64` and populate this env var to
        // match.
        let nbd_pool = match std::env::var("ENGRAM_NBD_DEVICES") {
            Ok(s) if !s.trim().is_empty() => {
                let paths: Vec<std::path::PathBuf> = s
                    .split(',')
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect();
                match engram_host_agent::disk_daemon::NbdSlotAllocator::from_paths(paths) {
                    Ok(pool) => {
                        tracing::info!(
                            slots = pool.capacity(),
                            "NBD daemon enabled; chunked rootfs serves /dev/nbdN",
                        );
                        Some(pool)
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "ENGRAM_NBD_DEVICES misconfigured; aborting");
                        std::process::exit(1);
                    }
                }
            }
            _ => None,
        };

        let pooled_backend: Arc<dyn SandboxBackend> = Arc::new({
            let mut p = engram_host_agent::pooled_backend::PooledBackend::new(raw_backend)
                // ADR 0008 Phase 5: feed the OciClient to the pooled
                // backend so chunked-OCI images can fault chunks from
                // the registry on BlobStorage miss.
                .with_oci_client((*oci_client).clone())
                .with_image_cache(image_cache)
                .with_chunk_store(chunk_store, materialize_dir)
                .with_chunk_cache(chunk_cache);
            if let Some(egress) = host_egress.clone() {
                p = p.with_egress(egress);
            }
            if let Some(pool) = nbd_pool {
                p = p.with_nbd_pool(pool);
            }
            p
        });
        // Use a stable HostId for `--mode=all` so a coordinator
        // restart picks up the same `hosts` row (FK-safe — sessions
        // / snapshots inserted in a prior run still reference a valid
        // host row). The hostname column is UNIQUE; without a stable
        // id, every restart would collide on `("in-process")` and
        // the row's id would diverge from the in-memory id we route
        // through, breaking snapshot inserts via FK.
        //
        // `in_proc_host` is computed at top of the function so the
        // FC config (built earlier in this branch) can stamp the
        // same id on `host_id` for trace replay.
        //
        // We defer the actual `host_registry.register` until after
        // AppState is built (via `run_with_registry_and_local` below),
        // because `LocalHostClient` needs the AppState's `HarnessHub`
        // — and that hub doesn't exist until AppState is constructed.
        in_proc_local_backend = Some(pooled_backend.clone());
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
                total_mib: 0,
                used_mib: 0,
                running_sandboxes: 0,
            },
            status: engram_core::types::host::HostStatus::Ready,
            last_heartbeat_at: chrono::Utc::now(),
            host_addr: None,
        };
        if let Err(e) = engram_core::traits::MetadataStore::upsert_host(&pg, host_record).await {
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
                        total_mib: 0,
                        used_mib: 0,
                        running_sandboxes: 0,
                    },
                    status: engram_core::types::host::HostStatus::Ready,
                    last_heartbeat_at: chrono::Utc::now(),
                    host_addr: None,
                };
                if let Err(e) = engram_core::traits::MetadataStore::upsert_host(&pg_for_hb, r).await
                {
                    tracing::warn!(error = %e, "in-process heartbeat upsert failed");
                }
            }
        });
        tracing::info!(
            host_id = %in_proc_host,
            "registered in-process host as LocalHostClient",
        );
    } else {
        tracing::info!(
            "coordinator started without a local backend; waiting for hosts on /api/hosts/connect"
        );
    }

    // SecretStore selection. `env` is dev-only (reads `$NAME` from
    // the coordinator's host shell); `gcp` resolves each image
    // manifest's `[secrets.*]` entry against GCP Secret Manager at
    // session-create time, authenticating via the metadata server
    // (Workload Identity).
    let secrets: Arc<dyn SecretStore> = match cli.secrets_backend {
        SecretsChoice::Env => Arc::new(EnvSecretStore::new()),
        SecretsChoice::Gcp => {
            let project = cli.gcp_project_id.clone().ok_or_else(|| {
                CoordinatorError::Config(
                    "--secrets-backend=gcp requires --gcp-project-id (or \
                     ENGRAM_GCP_PROJECT_ID)"
                        .into(),
                )
            })?;
            Arc::new(
                engram_secrets_gcp::GcpSecretManager::new(project)
                    .map_err(|e| CoordinatorError::Config(format!("secrets gcp: {e}")))?,
            )
        }
    };

    // KEK + meta_arc + blob were constructed up-front so the OCI
    // auth resolver / chunk store could reference them. They flow
    // through to Services here unchanged.

    // ADR 0007: one ChunkStore per process. Shared between the
    // host-agent-equivalent PooledBackend (built earlier in
    // `--mode=all`) and the coord's GC admin endpoint. Both
    // consume the same `blob` so chunks the bake writes land in
    // the same keyspace the GC sweeps.
    let chunk_store = engram_chunk_store::ChunkStore::new(blob.clone());
    // ADR 0013: per-pod gRPC pool. Empty at construction; populated
    // on `/api/hosts/register` POSTs and (in `run_with_registry`) at
    // startup from already-registered `hosts` rows.
    let host_pool = Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new());
    let services = Services {
        meta: meta_arc.clone(),
        cloud,
        host: host_registry.clone() as Arc<dyn HostClient>,
        host_pool,
        secrets,
        kek,
        oci: oci_client,
        auth_resolver,
        blob,
        chunk_store,
        materialize_dir: coord_materialize_dir,
    };

    engram_coordinator::run_with_registry_and_local(
        cfg,
        services,
        host_registry,
        in_proc_local_backend.map(|b| (in_proc_host, b)),
    )
    .await
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
