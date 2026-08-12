use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

// #1003: jemalloc as the global allocator on Linux (prod is
// musl-static, and musl mallocng retained 23 GB of ~48 MiB groups
// under chunk-buffer churn). The exported `_rjem_malloc_conf` bakes
// the profiler defaults in: sampled at ~512 KiB (`lg_prof_sample:19`,
// negligible overhead), active from boot. Override at deploy with the
// `_RJEM_MALLOC_CONF` env if a roll ever needs it off.
#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(target_os = "linux")]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";
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

    /// Root for per-sandbox sparse dirty files.
    #[arg(
        long,
        env = "ENGRAM_DIRTY_ROOT",
        default_value = "/var/lib/engram/dirty"
    )]
    dirty_root: PathBuf,

    /// Coordinator endpoint to dial via WebSocket.
    #[arg(long, env = "ENGRAM_COORDINATOR_ENDPOINT")]
    coordinator: Option<String>,

    /// Address the Prometheus `/metrics` exporter listens on. This is
    /// the Prometheus scrape target (k8s ServiceMonitor scrapes port 9100).
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
    /// On K8s the chart injects this (the pod's routable address);
    /// when unset, host-agent falls back to
    /// `http://127.0.0.1:<grpc-port>` for dev runs (coord and
    /// host-agent on the same box).
    /// Set to an empty string to opt out of registration entirely.
    #[arg(long, env = "ENGRAM_GRPC_ADVERTISE_ADDR")]
    grpc_advertise_addr: Option<String>,

    /// ADR 0045 C2: bind address for the post-copy page-server
    /// listener. The DEST sandbox's uffd-handler dials it during a
    /// live migration; per-export token auth keeps it inert
    /// otherwise. `0.0.0.0:9102` by default; empty string or
    /// `disabled` skips it.
    #[arg(
        long,
        env = "ENGRAM_MIGRATE_PEER_LISTEN_ADDR",
        default_value = "0.0.0.0:9102"
    )]
    migrate_peer_listen_addr: String,

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
    /// `just pull-kernel`).
    #[arg(long, env = "ENGRAM_VZ_KERNEL_PATH")]
    vz_kernel_path: Option<PathBuf>,

    /// TCP port the local egress proxy binds. iptables PREROUTING
    /// REDIRECT on this host sends guest tcp/443 here. The egress
    /// proxy is **mandatory** — it is the only path a guest reaches
    /// the network (TCP/443 SNI allow-listing + DNS filtering, ADR
    /// 0006), and a host that cannot stand it up refuses to serve
    /// sessions. This flag exists only to avoid a bind collision when
    /// something else on the host already holds the default port;
    /// there is **no `0 = off` sentinel** — the proxy cannot be
    /// disabled. ADR 0006 / issue #240.
    #[arg(long, env = "ENGRAM_EGRESS_PROXY_PORT", default_value_t = 8443)]
    egress_proxy_port: u16,

    /// UDP+TCP port the filtering DNS proxy binds. iptables REDIRECTs
    /// guest `{udp,tcp}/53` here, so this MUST match the value baked
    /// into the FC iptables rules — both are wired from this one flag.
    /// Defaults to 5353 (avoids the systemd-resolved bind on
    /// 127.0.0.53:53). Like `--egress-proxy-port`, this exists to avoid
    /// a bind collision — notably when TWO host-agents share a netns
    /// (the `ENGRAM_INTEG_TWO_HOSTS` e2e stack): each needs a distinct
    /// DNS port or the second fails closed on `Address already in use`.
    #[arg(long, env = "ENGRAM_EGRESS_DNS_PORT", default_value_t = 5353)]
    egress_dns_port: u16,

    /// TCP port for session-scoped host services. Firecracker and VZ steer
    /// 169.254.169.254:80 here. Compatibility metadata adapters and native
    /// tunnel routes share this gateway.
    #[arg(long, env = "ENGRAM_GUEST_GATEWAY_PORT", default_value_t = 13338)]
    guest_gateway_port: u16,

    /// Where the host-agent loads the deployment-wide egress-proxy
    /// CA from. Production uses `gcp-secret-manager` with Workload
    /// Identity; dev uses `local-disk` (auto-generates on first boot).
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
    // Held for the lifetime of `main`; its `Drop` flushes pending OTLP
    // spans on shutdown (ADR 0019).
    let _telemetry = init_tracing();

    // ADR 0022 cold-restore mitigation: raise RLIMIT_MEMLOCK so the image
    // prefetcher can `mlock` per-template base memfiles resident (the
    // host-agent runs as root in prod, so it can raise its own hard limit).
    // Best-effort: a failure just degrades the later mlock attempts to
    // leaving memfiles evictable.
    #[cfg(target_os = "linux")]
    raise_memlock_rlimit();

    let cli = Cli::parse();

    // ADR 0070: dedicated-volume mountpoint gate. When the chart pairs
    // `storage.dedicatedDevices` with `ENGRAM_WORK_DIR_REQUIRE_MOUNTPOINT
    // =true`, `work_dir` MUST resolve to a distinct filesystem from the
    // boot-disk reference path (`ENGRAM_HOST_ROOT_REF_PATH`, default `/`
    // — bare metal only; the chart points this at a read-only hostPath
    // mount of the NODE's `/` so the comparison isn't against the
    // container's own overlayfs, which would always differ and make the
    // gate vacuous) — otherwise the chunk cache, snapshots, and memfiles
    // silently land on the boot disk instead of the dedicated volume an
    // operator provisioned specifically to take that load off the
    // kubelet's nodefs signal (the base-shm-startup-race failure class:
    // a rolled pod starting before node-prep's mount is visible). Fail
    // loud BEFORE touching work_dir or registering with the coordinator
    // — never come up silently wrong.
    require_work_dir_mountpoint_or_exit(&cli.work_dir)?;

    // Bind the metrics port first. It's the Prometheus scrape target
    // (k8s ServiceMonitor scrapes it); binding early means metrics are
    // available as soon as the process is up.
    engram_host_agent::metrics::init(cli.metrics_addr);

    // #1003: SIGUSR2 → symbolized pprof heap dump; allocator stats as
    // Prometheus gauges every 30 s. Dumps land in the work_dir (the
    // node volume — survives the OOM kill the dump is usually for).
    #[cfg(target_os = "linux")]
    engram_host_agent::heap_profile::spawn(cli.work_dir.clone());

    // ADR 0013: resolve the gRPC listen + advertise addrs.
    //
    // listen_addr: parse the CLI/env-supplied socket addr. Empty
    // string or "disabled" skips the gRPC server (mode=all-style
    // standalone dev where the host-agent serves only its in-proc
    // backend). Default 0.0.0.0:9101 is "always on" in production.
    //
    // advertise_addr: prefer ENGRAM_GRPC_ADVERTISE_ADDR (the chart
    // injects the pod's routable address on K8s); if unset, fall back
    // to 127.0.0.1 with the resolved listen port (covers dev-vm
    // split-mode where the coord and host-agent run on the same box).
    let (grpc_listen_addr, grpc_port) = parse_grpc_listen(&cli.grpc_listen_addr);
    let grpc_advertise_addr = resolve_advertise_addr(cli.grpc_advertise_addr.clone(), grpc_port);
    // ADR 0045 C2: same empty/"disabled" semantics as the gRPC listener.
    let (migrate_peer_listen_addr, _) = parse_grpc_listen(&cli.migrate_peer_listen_addr);

    let cfg = HostAgentConfig {
        work_dir: cli.work_dir.clone(),
        coordinator_endpoint: cli.coordinator.clone(),
        coordinator_token: cli.coordinator_token.clone(),
        grpc_listen_addr,
        grpc_advertise_addr,
        migrate_peer_listen_addr,
        // ADR 0068: detection stays here, above the cfg-gated backend
        // match below — `capabilities::probe_all` reads this string
        // rather than re-deriving it from a downcast.
        backend_name: match cli.sandbox_backend {
            BackendChoice::Firecracker => "firecracker".to_string(),
            BackendChoice::Vz => "vz".to_string(),
        },
        ..HostAgentConfig::default()
    };

    // ADR 0007 Phase 5: resolve the HostId early so the FC config can
    // stamp it on snapshots' `trace_host_hint` and pass it as
    // `--publish-trace-host` to the UFFD handler. Cross-host trace replay
    // keys off this id — a snapshot taken by host A becomes restoreable on
    // host B with B reusing A's recorded trace.
    // ADR 0044 K2 (GAP 1): STABLE across restarts (persisted in work_dir,
    // node-name-seeded on K8s) so a DaemonSet pod restart keeps its host
    // identity and the coordinator's session→host binding survives.
    let node_name = std::env::var("NODE_NAME").ok().filter(|s| !s.is_empty());
    let host_id = resolve_host_id(&cli.work_dir, node_name.as_deref());

    // ADR 0045 addendum (2026-07-10): the base-shm sweeper's enabled-image
    // keep-set. One instance shared between the sweeper (FC arm below) and
    // the image-prefetch supervisor (via the HostAgent builder), so the
    // sweep can never delete an enabled image's pre-warmed base file.
    let base_shm_protected = engram_host_agent::base_shm_gc::ProtectedPaths::new();

    // ADR 0007: blob backend + chunk store. Created before the backend
    // match so the inner VZ backend can be wired with its own chunk
    // store (snapshots chunk the rootfs clone and report the manifest
    // ref). Misconfiguration fails closed at startup — better than
    // pretending to be ready and failing every session create. Shares
    // the same `BlobStorage` the coordinator's chunk store (the
    // enable-time materializer) writes to (a shared GCS bucket in
    // prod, a shared `local_path` in dev).
    let blob = engram_blob_client::from_env()
        .await
        .map_err(|e| HostAgentError::Config(format!("blob backend: {e}")))?;

    // ADR 0088 addendum: ONE host-global upload budget shared by every
    // ChunkStore this process builds — materialize chunking, capture
    // memory-seed/diff chunking, and the NBD disk flush all acquire
    // from the same FIFO pool, so concurrent bulk uploads share the
    // NIC instead of stacking on it (`ENGRAM_UPLOAD_BUDGET_PERMITS`).
    let upload_budget = engram_chunk_store::UploadBudget::from_env_or_default();

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
            // ADR 0035/0062: read RO bundles from the same dir the host-agent
            // reports `current_bundles` from — ENGRAM_BUNDLE_DIR in dev/e2e, the
            // fleet-canonical SHARED_DIR in prod (env unset). Mirrors the VZ arm
            // below; `SandboxBackend::bundle_dir` then exposes this one value so
            // the heartbeat reads the same stamp the backend attaches from.
            fc_cfg.bundle_dir = engram_host_agent::bundles::bundle_dir_from_env();
            // ADR 0044 K2 / GAP 2: on K8s the firecracker binary is staged
            // into a pod emptyDir (e.g. /opt/engram/firecracker), not on
            // PATH. Point the backend at it. Defaults to a PATH lookup of
            // `firecracker` (the GCE/Packer hosts + dev).
            if let Some(p) = std::env::var_os("ENGRAM_FIRECRACKER_BIN") {
                fc_cfg.firecracker_bin = PathBuf::from(p);
            }
            // ADR 0028 Fix A: arm KVM dirty tracking fleet-wide so every
            // capture after the chain's first can be a Diff (O(dirty)
            // pause). Tied to the same env knob as the checkpoint driver —
            // tracking has a steady-state write-protect cost that's only
            // worth paying when diffs are actually taken.
            fc_cfg.track_dirty_pages = engram_host_agent::checkpoint::CheckpointConfig::from_env()
                .interval
                .is_some();
            // ADR 0019: a guest-reachable OTLP collector endpoint (e.g. the
            // TAP gateway IP : the collector's port). When set, cold-boot
            // boot_args carry `engram_otel=<this>` so the in-guest agentd
            // exports its boot spans into the host's cold-boot trace. Unset
            // (the default) leaves in-guest tracing off.
            fc_cfg.guest_otel_endpoint = std::env::var("ENGRAM_GUEST_OTEL_ENDPOINT")
                .ok()
                .filter(|s| !s.trim().is_empty());
            // The egress proxy is mandatory (issue #240): always
            // plumb the matching TCP/443 port into the FC config so
            // iptables installs the REDIRECT rule (and the matching
            // default-deny on FORWARD + udp/tcp 53 DNS REDIRECT).
            // The DNS-redirect port is wired from the same `--egress-dns-port`
            // flag the proxy binds (below), so the iptables `:53 -> dns` REDIRECT
            // and the proxy's DNS listener can never drift. Defaults to 5353.
            fc_cfg.egress_proxy_port = Some(cli.egress_proxy_port);
            fc_cfg.egress_dns_port = Some(cli.egress_dns_port);
            fc_cfg.guest_gateway_port = Some(cli.guest_gateway_port);
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
            // ADR 0044 K2: on K8s the chart sets this to a node-level cgroup
            // dir (e.g. /sys/fs/cgroup/engram-vms); the FC backend moves each
            // VM's processes there so a host-agent pod restart's cgroup teardown
            // doesn't kill them. Unset on dev / tests (no pod scope to escape).
            fc_cfg.vm_cgroup_parent = std::env::var("ENGRAM_FC_VM_CGROUP_PARENT")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
            // ADR 0020 Route B: ENGRAM_FC_RESTORE_MODE=uffd flips restore
            // to the chunk-native UFFD handler (lazy memory, no memory.bin
            // materialize). Defaults to File.
            fc_cfg.restore_mode = engram_sandbox_firecracker::restore_mode_from_env();
            // ADR 0092: fresh-create backend override (File-unpinned
            // density path on substrate hosts). Read once at startup.
            fc_cfg.fresh_restore_override =
                engram_sandbox_firecracker::fresh_restore_mode_from_env();
            // Point the UFFD handler's READ-ONLY view at the SAME
            // chunk cache the PooledBackend's restore-prefetch warms
            // (`chunk-cache`, see below). ADR 0075: the handler never
            // writes here — misses populate through THIS process's
            // cache via the substrate socket, so one singleflight /
            // pin set / budget governs the directory. (The old
            // "cross-process sharing is safe" claim was true for
            // BYTES, false for POLICY — the multi-writer arrangement
            // is gone.)
            fc_cfg.uffd_cache_root = Some(cli.work_dir.join("chunk-cache"));
            // ADR 0075: the substrate populate socket this process binds.
            fc_cfg.uffd_substrate_sock = Some(cli.work_dir.join("substrate.sock"));
            // Prod bakes `engram-uffd-handler` to /usr/local/bin (on PATH,
            // the default). Dev/test override via ENGRAM_FC_UFFD_HANDLER_BIN.
            if let Ok(p) = std::env::var("ENGRAM_FC_UFFD_HANDLER_BIN") {
                fc_cfg.uffd_handler_bin = p.into();
            }
            // ADR 0045 unified memory substrate (v2b): when
            // ENGRAM_FC_UFFD_BASE_DIR points at a tmpfs dir, Uffd-mode
            // restores back guest memory MAP_PRIVATE on a per-template
            // base shm file there — canonical pages become one shared
            // page-cache copy per host (the D2 rollout gate; D3/D4
            // parity flips retire the knob).
            fc_cfg.uffd_base_dir = engram_sandbox_firecracker::uffd_base_dir_from_env();
            // ADR 0045 D4: GC unreferenced base shm files (disabled
            // images, pre-D4 session-keyed leftovers). Live files are
            // protected by the handlers' open fds; enabled images' files
            // by the shared keep-set (ADR 0045 addendum); see base_shm_gc.
            let _base_shm_gc = engram_host_agent::base_shm_gc::spawn(
                fc_cfg.uffd_base_dir.clone(),
                base_shm_protected.clone(),
            );
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
                             `just pull-kernel`)"
                                .into(),
                        )
                    })?;
                // ADR 0061: VZ reads skill bundles from the same staged
                // dir the host-agent reports its `current_bundles` from.
                // ADR 0096 D6: pass the egress proxy/DNS ports into every
                // guest — the init shim installs the in-guest DNAT
                // redirect (soft steering; the proxy already binds
                // 0.0.0.0, reachable at the VZ NAT gateway).
                let vz_cfg = engram_sandbox_vz::VzConfig::with_kernel(kernel)
                    .with_bundle_dir(engram_host_agent::bundles::bundle_dir_from_env())
                    .with_egress_ports(
                        cli.egress_proxy_port,
                        cli.egress_dns_port,
                        cli.guest_gateway_port,
                    );
                fc_for_reattach = None;
                // ADR 0007: attach the chunk store so `snapshot()` chunks
                // the rootfs clone and reports the manifest ref. Without
                // this the base-snapshot capture produces a disk_manifest
                // of None, and the coordinator's HEAD-verify rejects the
                // enable ("chunked manifests failed HEAD-verify"). Shares
                // the same `blob` Arc as the PooledBackend wrapper below.
                let cs = engram_chunk_store::ChunkStore::new(blob.clone())
                    .with_upload_budget(upload_budget.clone());
                Arc::new(
                    engram_sandbox_vz::VzBackend::new(cli.work_dir.clone(), vz_cfg)
                        .map_err(|e| HostAgentError::Config(format!("vz backend: {e}")))?
                        .with_chunk_store(cs),
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

    // ADR 0070: NVMe-backed chunk cache with a disk-derived absolute
    // budget by default (min(60% of the cache disk, 80% — the same
    // margin the free-space floor holds below the kubelet eviction
    // line); ~179 GB on the 298.1 GB prod disk that used to grow
    // unbounded). Operators may override via `ENGRAM_CHUNK_CACHE_
    // BUDGET_BYTES` (wins outright) or `ENGRAM_CHUNK_CACHE_DISK_FRACTION`
    // (adjusts the fraction the default derives from). The 20%
    // free-space floor still governs independently and re-probes every
    // sweep.
    let chunk_cache = engram_chunk_store::ChunkCache::new(
        engram_chunk_store::cache::ChunkCacheConfig::from_env_or_default(
            cli.work_dir.join("chunk-cache"),
        ),
    );
    // ADR 0070: periodic enforcement independent of populate traffic —
    // this is the host-agent's ONE cache-eviction policy per host (the
    // UFFD handler shares this directory but builds its own cache with
    // eviction disabled; see engram-uffd-handler). Held for the process
    // lifetime, same pattern as `_base_shm_gc` below.
    let _chunk_cache_sweeper = chunk_cache.spawn_sweeper(std::time::Duration::from_secs(
        engram_chunk_store::cache::resolve_sweep_interval_secs(),
    ));

    // ADR 0075: the substrate populate server — the WRITER side of the
    // single-writer cache discipline. Handlers are read-only clients;
    // their populate requests run against THIS cache instance, so the
    // global singleflight / pin set / budget govern handler traffic too.
    // (Spawned below once the chunk store exists; see substrate_spawn.)

    // ADR 0007: chunk store for the PooledBackend wrapper (materialize +
    // base-snapshot residency). `blob` was created above the backend
    // match so the inner VZ backend could share it; reuse it here. Wire the
    // SAME local cache the UFFD handler + disk daemon read from
    // (`work_dir/chunk-cache`) as a write-through tier, so a chunk this host
    // produces — e.g. an idle-eviction re-chunk of divergent memory — is
    // served locally on the next resume instead of re-fetched from GCS.
    let chunk_store = engram_chunk_store::ChunkStore::new(blob)
        .with_chunk_cache(chunk_cache.clone())
        .with_upload_budget(upload_budget);

    // ADR 0075: spawn the substrate populate server now both halves
    // exist. The uffd base dir mirrors the FC config default (env
    // override first) so the tmpfs probe answers for the dir handlers
    // actually use. FC-only (ADR 0096): its sole client is the UFFD
    // handler, which exists only on the Firecracker backend — on VZ the
    // server just sat on a Linux-shaped `/dev/shm/engram` default that
    // doesn't exist on macOS.
    let _substrate_server = match cli.sandbox_backend {
        BackendChoice::Firecracker => Some(
            engram_host_agent::substrate_server::SubstrateServer::new(
                chunk_cache.clone(),
                std::sync::Arc::new(chunk_store.clone()),
                engram_sandbox_firecracker::uffd_base_dir_from_env()
                    .unwrap_or_else(|| std::path::PathBuf::from("/dev/shm/engram")),
            )
            .spawn(cli.work_dir.join("substrate.sock"))
            .map_err(HostAgentError::Io)?,
        ),
        BackendChoice::Vz => None,
    };
    let materialize_dir = cli.work_dir.join("chunked-rootfs");

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
    let auth_coord_client = engram_host_agent::coord_client::HttpCoordClient::new(
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

    // ADR 0056 Phase 4: capture the coord endpoint + token before `cfg` is
    // moved into HostAgent — the observe sink below needs them.
    let observe_coord_url = cfg
        .coordinator_endpoint
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let observe_token = cfg.coordinator_token.clone();
    // WS4: the egress proxy's inject refresher needs the same coord endpoint +
    // token; capture them here too (before `cfg` moves into `HostAgent`).
    let refresh_coord_url = observe_coord_url.clone();
    let refresh_token = cfg.coordinator_token.clone();
    let cloud_sql_coord_url = observe_coord_url.clone();
    let cloud_sql_token = cfg.coordinator_token.clone();

    let mut agent = HostAgent::new(cfg, sandbox, cloud)
        .with_chunk_store(chunk_store, materialize_dir)
        .with_chunk_cache(chunk_cache)
        .with_dirty_root(cli.dirty_root.clone())
        .with_image_cache(image_cache)
        .with_host_id(host_id)
        .with_base_shm_protected(base_shm_protected);
    if let Some(fc) = fc_for_reattach {
        agent = agent.with_fc_reattach(fc);
    }
    // ADR 0007 Phase 4 / ADR 0049: NBD daemon with a warm-pool slot
    // allocator. The pool sizes itself from the kernel's `nbds_max`
    // (the chart's `modprobe nbd nbds_max=<N>`), capped by
    // `ENGRAM_NBD_MAX_SLOTS`, keeping `ENGRAM_NBD_WARM_SLOTS` slots
    // pre-validated and ready for O(1) `acquire()`. Returns `None`
    // (materialize-to-file fallback) when the nbd module isn't loaded
    // or `ENGRAM_NBD_DISABLE` is set.
    //
    // ADR 0044 K2: stale-binding recovery does NOT run at construction.
    // A device bound to the dead previous generation might be a
    // SURVIVOR's live disk (its FC keeps reading it; rehydrate
    // RECONFIGUREs it) — the old eager pass disconnected one in prod
    // (2026-06-11, /dev/nbd4 → guest rootfs EIO). The sweep runs in
    // `lib.rs` AFTER the registration-time survivor rehydrate, scoped
    // to the slot pool's still-free paths.
    if let Some(pool) = engram_host_agent::disk_daemon::build_nbd_pool_from_kernel() {
        agent = agent.with_nbd_pool(pool);
    }
    // The egress proxy is mandatory (issue #240). A host that cannot
    // stand it up (CA unloadable, port unbindable) must NOT serve a
    // session — there is no "unfiltered access" fallback. Fail closed.
    // ADR 0056 Phase 4: the egress proxy's observed-asset sink — forwards each
    // proxy-built IntegrationAsset to the coord (mirrors the harness-event
    // path). Best-effort fire-and-forget: spawn the POST, log on failure.
    // ADR 0098 D1: wall clock is an injected world input. The binary
    // constructs the production clock once and the observe sink reads the
    // asset timestamp through it.
    let observe_clock: Arc<dyn engram_core::traits::Clock> =
        Arc::new(engram_core::traits::SystemClock::new());
    let observe_sink: engram_egress_proxy::ObserveSink = Arc::new(move |session_id, asset| {
        let cc = engram_host_agent::coord_client::HttpCoordClient::new(
            observe_coord_url.clone(),
            observe_token.clone(),
        );
        let observe_clock = observe_clock.clone();
        tokio::spawn(async move {
            let req = engram_host_agent::coord_client::IntegrationAssetReport {
                provider: asset.provider,
                asset_kind: asset.asset_kind,
                surface: asset.surface,
                data: serde_json::Value::Object(asset.data),
                fetchable_url: asset.fetchable_url,
                at: observe_clock.now_utc(),
            };
            if let Err(e) = cc.integration_asset(session_id, &req).await {
                tracing::debug!(%session_id, error = %e, "forward integration asset to coord failed");
            }
        });
    });
    // WS4: the inject refresher — re-mints a near-expiry minted inject
    // credential via the coord's inject-refresh route (mirrors the observe
    // sink's coord bridge, but request/response since the proxy awaits it).
    let inject_refresher: Arc<dyn engram_egress_proxy::InjectRefresher> =
        Arc::new(engram_host_agent::egress::CoordInjectRefresher::new(
            engram_host_agent::coord_client::HttpCoordClient::new(refresh_coord_url, refresh_token),
            host_id,
        ));
    let cloud_sql_connector: Arc<dyn engram_egress_proxy::TunnelUpstream> =
        Arc::new(engram_host_agent::egress::CoordCloudSqlConnector::new(
            engram_host_agent::coord_client::HttpCoordClient::new(
                cloud_sql_coord_url,
                cloud_sql_token,
            ),
            host_id,
        ));
    match build_host_egress(
        &cli,
        Some(observe_sink),
        Some(inject_refresher),
        Arc::new(engram_egress_proxy::GuestGatewayRegistry::new(
            [Arc::new(engram_egress_proxy::GceMetadataService)
                as Arc<dyn engram_egress_proxy::GuestServiceAdapter>],
            [cloud_sql_connector],
        )),
    )
    .await
    {
        Ok(egress) => agent = agent.with_egress(Arc::new(egress)),
        Err(e) => {
            tracing::error!(error = %e, "egress proxy spawn failed; aborting (egress filtering is mandatory)");
            return Err(HostAgentError::Config(format!("egress: {e}")));
        }
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
async fn build_host_egress(
    cli: &Cli,
    observe_sink: Option<engram_egress_proxy::ObserveSink>,
    inject_refresher: Option<Arc<dyn engram_egress_proxy::InjectRefresher>>,
    guest_gateway: Arc<engram_egress_proxy::GuestGatewayRegistry>,
) -> Result<engram_host_agent::egress::HostEgress, String> {
    use std::sync::Arc;
    // Port 0 would bind an ephemeral port while the iptables REDIRECT
    // still targets the literal configured value — the host boots green
    // and every guest gets ConnectionRefused, the exact split-brain the
    // fail-closed bind (ADR 0083) exists to kill. There is no `0 = off`
    // sentinel (egress is mandatory, issue #240), so reject it here.
    if cli.egress_proxy_port == 0 || cli.egress_dns_port == 0 || cli.guest_gateway_port == 0 {
        return Err(format!(
            "egress ports must be non-zero (got proxy={}, dns={}, guest_gateway={}): the iptables \
             REDIRECT targets the configured port, so an ephemeral (0) bind \
             leaves :443/:53 or the guest gateway redirected at a dead port",
            cli.egress_proxy_port, cli.egress_dns_port, cli.guest_gateway_port,
        ));
    }
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
    // The DNS proxy binds the port the FC iptables `:53 -> dns` REDIRECT
    // targets; both come from `--egress-dns-port` (default 5353) so they
    // can't drift. Configurable so two host-agents sharing a netns (the
    // e2e two-host stack) don't collide on it.
    let dns_bind: std::net::SocketAddr = format!("0.0.0.0:{}", cli.egress_dns_port)
        .parse()
        .map_err(|e| format!("parse dns bind addr: {e}"))?;
    let guest_gateway_bind: std::net::SocketAddr = format!("0.0.0.0:{}", cli.guest_gateway_port)
        .parse()
        .map_err(|e| format!("parse guest gateway bind addr: {e}"))?;
    engram_host_agent::egress::HostEgress::spawn(
        source,
        bind,
        Some(dns_bind),
        Some(guest_gateway_bind),
        observe_sink,
        inject_refresher,
        guest_gateway,
    )
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
///      An explicit empty string opts out of registration. On K8s the
///      chart always injects this (`http://$(POD_IP):<port>` — the node
///      IP under hostNetwork, ADR 0044 K2).
///   2. `http://127.0.0.1:<grpc_port>` — the dev-vm split-mode fallback
///      where the coord runs on the same VM.
///
/// `grpc_port=0` (gRPC server disabled) returns `None` regardless of
/// the env var, since there's nothing to advertise.
fn resolve_advertise_addr(cli_value: Option<String>, grpc_port: u16) -> Option<String> {
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
    // No explicit advertise addr: the dev-vm split-mode fallback (coord on
    // the same VM). On K8s the chart always injects
    // ENGRAM_GRPC_ADVERTISE_ADDR, so this is dev-only — the GCE-metadata-server
    // probe this used to fall back to retired with the MIG (ADR 0044 K5).
    tracing::info!(
        "ENGRAM_GRPC_ADVERTISE_ADDR unset; advertising http://127.0.0.1:{grpc_port} \
         (set it explicitly for a multi-host run)",
    );
    Some(format!("http://127.0.0.1:{grpc_port}"))
}

/// Env var: ADR 0070's dedicated-volume mountpoint gate. See
/// [`require_work_dir_mountpoint_or_exit`].
const WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR: &str = "ENGRAM_WORK_DIR_REQUIRE_MOUNTPOINT";

/// Env var: the "boot disk" reference path the gate compares `work_dir`
/// against. Defaults to `/` — correct bare-metal (or dev/VZ) semantics,
/// where the process's own root IS the boot disk. **On K8s this must be
/// overridden.** The host-agent's `/` is the pod's ephemeral overlayfs,
/// which is on a distinct device from EVERY hostPath mount by
/// construction — comparing `work_dir` (always a hostPath) against the
/// container's `/` therefore always reports "distinct filesystem" and
/// the gate never fires, even when `work_dir` is silently still on the
/// node's boot disk (finding: this made the gate vacuous in the exact
/// DaemonSet it was built for). The chart pairs this with
/// `ENGRAM_WORK_DIR_REQUIRE_MOUNTPOINT=true`, pointing it at
/// `/mnt/host-root` — a read-only hostPath mount of the NODE's `/` — so
/// the comparison is boot-disk-vs-work_dir, not overlayfs-vs-work_dir.
const HOST_ROOT_REF_PATH_ENV_VAR: &str = "ENGRAM_HOST_ROOT_REF_PATH";

/// ADR 0070: when `ENGRAM_WORK_DIR_REQUIRE_MOUNTPOINT` is truthy
/// (`1`/`true`, case-insensitive), hard-fail unless `work_dir` resolves
/// to a distinct filesystem from the boot-disk reference path
/// (`ENGRAM_HOST_ROOT_REF_PATH`, default `/`) — i.e. a dedicated volume
/// is actually mounted there, not just a directory on the boot disk.
/// The chart sets `ENGRAM_WORK_DIR_REQUIRE_MOUNTPOINT` only when
/// `storage.dedicatedDevices` is configured, so this is a paired guard:
/// "you told me to expect a dedicated volume; prove it's mounted before
/// I start writing to it."
///
/// Compares `st_dev` (`stat(2)`'s device id — the same primitive `df`
/// uses to detect a mount boundary), not a mount-table parse, so it
/// works identically whether `work_dir` itself or an ancestor is the
/// actual mountpoint. `work_dir` may not exist yet on a freshly-imaged
/// host (the caller creates it downstream); this walks up to the
/// nearest existing ancestor rather than treating a stat ENOENT as a
/// gate failure — a missing directory says nothing about which
/// filesystem it WOULD land on.
///
/// No-op (returns `Ok`) when the env var is unset/false — today's
/// default, zero behavior change until a chart opts in.
fn require_work_dir_mountpoint_or_exit(work_dir: &std::path::Path) -> Result<(), HostAgentError> {
    use std::os::unix::fs::MetadataExt;

    let required = std::env::var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR)
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false);
    if !required {
        return Ok(());
    }

    let root_path = std::env::var(HOST_ROOT_REF_PATH_ENV_VAR)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let root_dev = std::fs::metadata(&root_path)
        .map_err(|e| {
            HostAgentError::Config(format!(
                "{WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR}=true but could not stat the boot-disk \
                 reference path {} ({HOST_ROOT_REF_PATH_ENV_VAR}): {e}",
                root_path.display(),
            ))
        })?
        .dev();

    let mut probe = work_dir.to_path_buf();
    let work_dev = loop {
        match std::fs::metadata(&probe) {
            Ok(meta) => break meta.dev(),
            Err(_) if probe.pop() => continue,
            Err(_) => {
                return Err(HostAgentError::Config(format!(
                    "{WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR}=true but no ancestor of {} exists to \
                     probe for a mountpoint",
                    work_dir.display(),
                )));
            }
        }
    };

    if work_dev == root_dev {
        return Err(HostAgentError::Config(format!(
            "{WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR}=true but {} is on the SAME filesystem as the \
             boot-disk reference path {} (st_dev {work_dev} == {root_dev}) — the dedicated \
             volume isn't mounted there yet (or storage.dedicatedDevices is misconfigured). \
             Refusing to start: coming up on the boot disk here would silently defeat the whole \
             point of the dedicated volume — cache/snapshot/memfile writes would count against \
             the SAME kubelet nodefs signal ADR 0070's headroom gauge and budget exist to keep \
             clear.",
            work_dir.display(),
            root_path.display(),
        )));
    }
    Ok(())
}

/// Initialise the global tracing subscriber (+ optional OpenTelemetry
/// OTLP export; ADR 0019).
///
/// Honors `ENGRAM_LOG_FORMAT` (`pretty`, the default, or `json` for
/// production Cloud Logging ingestion). Falls back to `RUST_LOG` for
/// the filter, then to `info,engram=debug` as a sensible local
/// default. When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, spans are also
/// exported to that collector; otherwise OTLP is inert.
///
/// The returned guard must be held for the lifetime of `main` so spans
/// flush on shutdown (`TelemetryGuard` is itself `#[must_use]`).
/// ADR 0044 K2 (GAP 1): resolve a STABLE `HostId` so a DaemonSet pod
/// restart keeps the same host identity. K2's detach+reattach leaves the
/// node's VMs running and the successor pod re-adopts them — but only if
/// it re-registers under the SAME id, or the coordinator's session→host
/// binding goes stale (reconcile would migrate a live VM out from under
/// itself). Resolution order:
///   1. `<work_dir>/host_id` if present + parseable — the work_dir is a
///      node hostPath on K8s, so it survives a pod restart on the node.
///   2. else seed deterministically from the K8s node name (so a
///      same-node restart with a wiped work_dir still recovers the id),
///      and persist it.
///   3. else (no node name — non-K8s dev) a fresh random id, persisted.
fn resolve_host_id(work_dir: &std::path::Path, node_name: Option<&str>) -> engram_core::HostId {
    let id_path = work_dir.join("host_id");
    if let Ok(contents) = std::fs::read_to_string(&id_path) {
        if let Ok(id) = contents.trim().parse::<engram_core::HostId>() {
            tracing::info!(%id, path = %id_path.display(), "host_id: reusing persisted id");
            return id;
        }
        tracing::warn!(
            path = %id_path.display(),
            "host_id: file present but unparseable; regenerating"
        );
    }
    let id = match node_name {
        Some(name) if !name.is_empty() => engram_core::HostId::from_node_name(name),
        _ => engram_core::HostId::new(),
    };
    if let Some(parent) = id_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&id_path, id.to_string()) {
        tracing::warn!(
            error = %e,
            path = %id_path.display(),
            "host_id: failed to persist; a work_dir wipe on the same node would change identity"
        );
    }
    tracing::info!(%id, node_name = ?node_name, "host_id: generated + persisted");
    id
}

/// ADR 0022: raise `RLIMIT_MEMLOCK` to unlimited so the image prefetcher can
/// pin (`mlock`) per-template base memfiles resident. Root can raise its own
/// hard limit; best-effort — a failure is logged and the mlock attempts in
/// `image_prefetch` simply fall back to leaving the memfile evictable.
#[cfg(target_os = "linux")]
fn raise_memlock_rlimit() {
    let lim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: `setrlimit` with a valid, fully-initialized `rlimit`.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim) };
    if rc != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "ADR 0022: could not raise RLIMIT_MEMLOCK; base-memfile mlock may hit the cap",
        );
    } else {
        tracing::debug!("ADR 0022: RLIMIT_MEMLOCK raised for base-memfile pinning");
    }
}

fn init_tracing() -> engram_telemetry::TelemetryGuard {
    engram_telemetry::init(engram_telemetry::Config {
        service_name: "engram-host-agent",
        default_filter: "info,engram=debug",
    })
}

/// Default location for the arm64 Linux kernel `engram-sandbox-vz`
/// boots: `~/.cache/engram-vz-test/vmlinux-arm64`. Returns `None` if
/// `$HOME` isn't set or the file doesn't exist; the caller surfaces
/// a config error pointing at `just pull-kernel`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_persists_and_is_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let first = resolve_host_id(tmp.path(), Some("node-a"));
        // A persisted id wins over the node-name seed on the next start.
        let second = resolve_host_id(tmp.path(), Some("a-totally-different-node"));
        assert_eq!(first, second, "persisted host_id must be reused verbatim");
        let on_disk = std::fs::read_to_string(tmp.path().join("host_id")).unwrap();
        assert_eq!(
            on_disk.trim().parse::<engram_core::HostId>().unwrap(),
            first
        );
    }

    #[test]
    fn node_name_seed_is_deterministic_across_wiped_work_dirs() {
        // Same node name → same id even with a fresh work_dir (the
        // disk-wipe-same-node recovery path).
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_host_id(a.path(), Some("node-x")),
            resolve_host_id(b.path(), Some("node-x")),
        );
        assert_ne!(
            engram_core::HostId::from_node_name("node-x"),
            engram_core::HostId::from_node_name("node-y"),
        );
    }

    #[test]
    fn no_node_name_generates_random_then_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let first = resolve_host_id(tmp.path(), None);
        // Persisted, so the next start on the same work_dir reuses it.
        let second = resolve_host_id(tmp.path(), None);
        assert_eq!(first, second);
    }

    // ---- ADR 0070: require_work_dir_mountpoint_or_exit ----

    // Tests poke a process-global env var; serialize (mirrors the
    // ENV_LOCK pattern used elsewhere in this repo, e.g.
    // engram-chunk-store's cache.rs tests).
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn mountpoint_gate_is_a_noop_when_env_unset() {
        let _g = env_guard();
        std::env::remove_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR);
        // A tempdir under the system temp dir is on the SAME filesystem
        // as `/` on every CI/dev box this test runs on — if the gate
        // fired unconditionally this would fail. It must not, since the
        // env var is unset.
        let tmp = tempfile::tempdir().unwrap();
        assert!(require_work_dir_mountpoint_or_exit(tmp.path()).is_ok());
    }

    #[test]
    fn mountpoint_gate_rejects_same_filesystem_as_root_when_required() {
        let _g = env_guard();
        std::env::set_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR, "true");
        let tmp = tempfile::tempdir().unwrap();
        // Both sides live under the same tempdir root, so they're
        // guaranteed to share a filesystem regardless of host layout —
        // comparing against the REAL `/` would be a host-layout
        // assumption (true when TMPDIR sits on the root fs, e.g.
        // ubuntu runners / macOS's APFS firmlinks; false on any box
        // with /tmp on tmpfs, e.g. Fedora/Arch defaults).
        let root_ref = tmp.path().join("root-ref");
        std::fs::create_dir(&root_ref).unwrap();
        let work_dir = tmp.path().join("work");
        std::fs::create_dir(&work_dir).unwrap();
        std::env::set_var(HOST_ROOT_REF_PATH_ENV_VAR, &root_ref);
        let result = require_work_dir_mountpoint_or_exit(&work_dir);
        std::env::remove_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR);
        std::env::remove_var(HOST_ROOT_REF_PATH_ENV_VAR);
        assert!(
            result.is_err(),
            "work_dir and the test-pinned boot-disk reference path share a filesystem by \
             construction (both under the same tempdir); the gate must reject it when required",
        );
    }

    #[test]
    fn mountpoint_gate_walks_up_to_nearest_existing_ancestor() {
        // work_dir itself doesn't exist yet (fresh host, created
        // downstream) — the gate must probe the nearest existing
        // ancestor instead of erroring on ENOENT.
        let _g = env_guard();
        std::env::set_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR, "true");
        let tmp = tempfile::tempdir().unwrap();
        let root_ref = tmp.path().join("root-ref");
        std::fs::create_dir(&root_ref).unwrap();
        std::env::set_var(HOST_ROOT_REF_PATH_ENV_VAR, &root_ref);
        let not_yet_created = tmp.path().join("sandboxes").join("work");
        let result = require_work_dir_mountpoint_or_exit(&not_yet_created);
        std::env::remove_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR);
        std::env::remove_var(HOST_ROOT_REF_PATH_ENV_VAR);
        // Same filesystem as the test-pinned boot-disk reference path
        // (both under `tmp`), so it's still a rejection — the point of
        // this test is that it errors on the FILESYSTEM check, not on a
        // "no such file" stat failure.
        assert!(result.is_err());
    }

    #[test]
    fn mountpoint_gate_accepts_env_var_case_insensitively_and_via_1() {
        let _g = env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let root_ref = tmp.path().join("root-ref");
        std::fs::create_dir(&root_ref).unwrap();
        let work_dir = tmp.path().join("work");
        std::fs::create_dir(&work_dir).unwrap();
        std::env::set_var(HOST_ROOT_REF_PATH_ENV_VAR, &root_ref);
        std::env::set_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR, "TRUE");
        assert!(require_work_dir_mountpoint_or_exit(&work_dir).is_err());
        std::env::set_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR, "1");
        assert!(require_work_dir_mountpoint_or_exit(&work_dir).is_err());
        std::env::set_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR, "false");
        assert!(require_work_dir_mountpoint_or_exit(&work_dir).is_ok());
        std::env::remove_var(WORK_DIR_REQUIRE_MOUNTPOINT_ENV_VAR);
        std::env::remove_var(HOST_ROOT_REF_PATH_ENV_VAR);
    }
}
