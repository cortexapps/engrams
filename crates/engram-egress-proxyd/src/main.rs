//! ADR 0121: the node-local egress daemon.
//!
//! Owns the ADR 0006 proxy listeners (proxy tcp, DNS udp+tcp, guest
//! gateway) so they outlive host-agent pod rolls: host-agent spawns
//! this process without `kill_on_drop`, it migrates itself out of the
//! pod cgroup, and the successor pod adopts it over the control UDS.
//! Established guest streams survive a deploy because this process
//! survives it.
//!
//! Startup order (each step's failure exits non-zero with NO manifest
//! written, so the spawning host-agent fails closed per ADR 0083):
//! cgroup self-migrate → load CA from env → build the proxy → bind the
//! control UDS → bind the listeners (`bind_with_retry`) → write the
//! manifest → serve.

mod control;
mod coord;
mod dialback;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use engram_egress_proxy::{CaSource, CertMint, EnvCaSource, Proxy, ProxyConfig, Registry};

/// The build-time source fingerprint (a content hash of this binary's
/// source dependency closure, injected by the image build). `None`
/// for local builds — which are never adopted across a restart.
/// `option_env!` requires the literal; keep it equal to
/// `engram_egress_proto::BUILD_ENV_SOURCE_FINGERPRINT`.
pub(crate) const SOURCE_FINGERPRINT: Option<&str> = option_env!("ENGRAM_EGRESS_PROXYD_FINGERPRINT");

#[derive(Parser, Debug)]
#[command(
    name = "engram-egress-proxyd",
    about = "engrams node-local egress daemon (ADR 0121)"
)]
struct Cli {
    /// The host-agent work dir. The control socket, dial-back socket,
    /// manifest, and log file all live here.
    #[arg(long)]
    work_dir: PathBuf,

    /// Proxy listener port (the iptables `:443 ->` REDIRECT target).
    #[arg(long, default_value_t = 8443)]
    proxy_port: u16,

    /// Filtering-DNS port (the `:53 ->` REDIRECT target), udp+tcp.
    #[arg(long, default_value_t = 5353)]
    dns_port: u16,

    /// Guest-gateway port (the metadata/tunnel REDIRECT target).
    #[arg(long, default_value_t = 13338)]
    gateway_port: u16,

    /// cgroup-v2 leaf to migrate into before binding (the ADR 0044 K2
    /// escape: out of the pod's kill domain). Best-effort with a loud
    /// ERROR on failure — the daemon still serves; it just won't
    /// survive the next roll (degraded to the pre-ADR-0121 status
    /// quo, never worse).
    #[arg(long)]
    cgroup_dir: Option<PathBuf>,

    /// Upstream the filtering DNS proxy forwards allowed queries to.
    #[arg(long, default_value = "1.1.1.1:53")]
    dns_upstream: std::net::SocketAddr,

    /// Log file. Defaults to `<work_dir>/egress-proxyd.log` — pod-log
    /// capture dies with the pod; the daemon's diagnostics must not.
    #[arg(long)]
    log_file: Option<PathBuf>,

    /// TEST ONLY: extra upstream trust-root PEM for hermetic fixtures
    /// (mirrors `ProxyConfig::upstream_test_roots`).
    #[arg(long, hide = true)]
    test_upstream_root_pem: Option<PathBuf>,

    /// TEST ONLY: static SNI resolution `host=ip:port` (repeatable);
    /// replaces the system resolver entirely.
    #[arg(long, hide = true)]
    test_resolve: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    init_tracing(&cli);

    // Escape the pod cgroup FIRST — before any listener exists, so a
    // daemon that a pod kill can still reach never holds the ports.
    if let Some(dir) = &cli.cgroup_dir {
        migrate_to_cgroup(dir);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime builds from static settings");
    let code = runtime.block_on(run(cli));
    std::process::exit(code);
}

async fn run(cli: Cli) -> i32 {
    // CA from spawn-time env (the `EnvCaSource` pattern — argv leaks
    // on /proc/*/cmdline, env does not). Fail-closed: no CA, no bind,
    // no manifest.
    let ca_source = EnvCaSource::new(
        engram_egress_proto::ENV_CA_CERT_PEM,
        engram_egress_proto::ENV_CA_KEY_PEM,
    );
    let ca = match ca_source.load().await {
        Ok(ca) => ca,
        Err(e) => {
            tracing::error!(error = %e, "egress CA load failed; exiting (fail-closed)");
            return 1;
        }
    };
    let ca_fingerprint = control::sha256_hex(ca.cert_pem.as_bytes());

    let coord_url = std::env::var(engram_egress_proto::ENV_COORD_URL)
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let coord_token = std::env::var(engram_egress_proto::ENV_COORD_TOKEN)
        .ok()
        .filter(|t| !t.is_empty());
    let host_id: engram_core::HostId = match std::env::var(engram_egress_proto::ENV_HOST_ID)
        .ok()
        .and_then(|raw| raw.parse().ok())
    {
        Some(id) => id,
        None => {
            tracing::error!(
                var = engram_egress_proto::ENV_HOST_ID,
                "host id env missing or unparseable; exiting (fail-closed)"
            );
            return 1;
        }
    };

    // One rustls provider per process (the host-agent installed this
    // before ADR 0121 moved the proxy here).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let registry = Arc::new(Registry::new());
    let mint = Arc::new(CertMint::new(Arc::new(ca)));
    let coord = coord::SlimCoordClient::new(coord_url.clone(), coord_token);

    let cloud_sql_pool = Arc::new(engram_egress_proxy::TunnelPool::new(
        coord::CloudSqlEndpointFactory::new(coord.clone(), host_id),
        engram_egress_proxy::PoolConfig::default(),
    ));
    let _cloud_sql_reaper = cloud_sql_pool.spawn_reaper(std::time::Duration::from_secs(30));
    let cloud_sql_connector: Arc<dyn engram_egress_proxy::TunnelUpstream> = cloud_sql_pool;
    let guest_gateway = Arc::new(engram_egress_proxy::GuestGatewayRegistry::new(
        [Arc::new(engram_egress_proxy::GceMetadataService)
            as Arc<dyn engram_egress_proxy::GuestServiceAdapter>],
        [cloud_sql_connector],
    ));

    let bind = |port: u16| -> std::net::SocketAddr {
        std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port))
    };
    // Port 0 would bind an ephemeral port while iptables REDIRECTs the
    // configured literal — the ADR 0083 split-brain. There is no
    // `0 = off` sentinel (egress is mandatory, issue #240).
    if cli.proxy_port == 0 || cli.dns_port == 0 || cli.gateway_port == 0 {
        tracing::error!(
            proxy = cli.proxy_port,
            dns = cli.dns_port,
            gateway = cli.gateway_port,
            "egress ports must be non-zero; exiting (fail-closed)"
        );
        return 1;
    }

    let mut proxy_cfg = ProxyConfig::new(bind(cli.proxy_port), registry.clone(), mint);
    proxy_cfg.dns_bind_addr = Some(bind(cli.dns_port));
    proxy_cfg.guest_gateway_bind_addr = Some(bind(cli.gateway_port));
    proxy_cfg.dns_upstream = cli.dns_upstream;
    proxy_cfg.guest_gateway = guest_gateway.clone();
    proxy_cfg.observe_sink = Some(coord::observe_sink(coord.clone()));
    proxy_cfg.inject_refresher = Some(Arc::new(coord::CoordInjectRefresher::new(
        coord.clone(),
        host_id,
    )));
    proxy_cfg.guest_port_dialer = Some(Arc::new(dialback::DialbackGuestPortDialer::new(
        cli.work_dir.join(engram_egress_proto::DIALBACK_SOCK_NAME),
    )));
    if let Some(pem_path) = &cli.test_upstream_root_pem {
        match load_test_roots(pem_path) {
            Ok(roots) => proxy_cfg.upstream_test_roots = Some(roots),
            Err(e) => {
                tracing::error!(path = %pem_path.display(), error = %e, "test upstream roots unreadable");
                return 1;
            }
        }
    }
    if !cli.test_resolve.is_empty() {
        let mut resolver = engram_egress_proxy::StaticResolver::new();
        for entry in &cli.test_resolve {
            let Some((host, addr)) = entry.split_once('=') else {
                tracing::error!(entry, "--test-resolve wants host=ip:port");
                return 1;
            };
            let Ok(addr) = addr.parse() else {
                tracing::error!(entry, "--test-resolve addr unparseable");
                return 1;
            };
            resolver = resolver.with(host, addr);
        }
        proxy_cfg.resolver = Arc::new(resolver);
    }

    let proxy = Proxy::new(proxy_cfg);

    // Control UDS before the listeners: the spawning host-agent's
    // readiness gate is a Hello round-trip + the accept-loop probe,
    // and both need the socket up. A stale socket file from a dead
    // predecessor is just unlinked — the manifest/identity checks
    // already decided nothing live owns it.
    let control_sock = cli.work_dir.join(engram_egress_proto::CONTROL_SOCK_NAME);
    let _ = std::fs::remove_file(&control_sock);
    let control_listener = match tokio::net::UnixListener::bind(&control_sock) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(sock = %control_sock.display(), error = %e, "control socket bind failed");
            return 1;
        }
    };

    let listeners = match control::bind_with_retry(&proxy).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "egress listeners bind failed; exiting (fail-closed, ADR 0083)");
            return 1;
        }
    };

    // The listeners are live: record the fact (atomic write — the
    // durable-at-the-point-it's-known rule, PR #1383 / ADR 0121).
    let identity = engram_egress_proto::manifest::self_identity();
    let hello = engram_egress_proto::HelloInfo {
        proto_version: engram_egress_proto::PROTO_VERSION,
        source_fingerprint: SOURCE_FINGERPRINT.map(str::to_string),
        proxy_port: cli.proxy_port,
        dns_port: cli.dns_port,
        gateway_port: cli.gateway_port,
        ca_fingerprint,
        coord_url,
    };
    let manifest = engram_egress_proto::manifest::ProxydManifest {
        schema_version: engram_egress_proto::manifest::MANIFEST_SCHEMA_VERSION,
        pid: identity.pid,
        start_time_jiffies: identity.start_time_jiffies,
        comm: identity.comm,
        source_fingerprint: hello.source_fingerprint.clone(),
        proxy_port: cli.proxy_port,
        dns_port: cli.dns_port,
        gateway_port: cli.gateway_port,
        control_sock: control_sock.clone(),
    };
    if let Err(e) = engram_egress_proto::manifest::write_manifest(&cli.work_dir, &manifest) {
        tracing::error!(error = %e, "manifest write failed; exiting (an unadoptable daemon must not serve)");
        return 1;
    }

    tracing::info!(
        pid = identity.pid,
        proxy_port = cli.proxy_port,
        dns_port = cli.dns_port,
        gateway_port = cli.gateway_port,
        fingerprint = ?SOURCE_FINGERPRINT,
        "engram-egress-proxyd serving"
    );

    let control_state = Arc::new(control::ControlState {
        registry,
        gateway: guest_gateway,
        hello,
    });
    tokio::spawn(control::serve(control_listener, control_state));

    proxy.serve(listeners).await;
    // `serve` loops forever on accept. If it returns, the data plane
    // is dead while iptables still REDIRECTs here: exit non-zero so
    // the supervising host-agent's exit event respawns us — never
    // linger as a control plane over a dead accept loop.
    tracing::error!("egress proxy serve loop exited; exiting for respawn");
    1
}

fn init_tracing(cli: &Cli) {
    let path = cli
        .log_file
        .clone()
        .unwrap_or_else(|| cli.work_dir.join(engram_egress_proto::LOG_FILE_NAME));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(Arc::new(file))
                .init();
        }
        Err(e) => {
            // Stderr fallback: the spawn wiring points stderr at the
            // same log file anyway; worst case the pod log gets it.
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::io::stderr)
                .init();
            tracing::warn!(path = %path.display(), error = %e, "log file unopenable; logging to stderr");
        }
    }
}

/// cgroup-v2 self-migration (the ADR 0044 K2 escape, self-service
/// because the daemon knows the exact moment before its first bind).
fn migrate_to_cgroup(dir: &std::path::Path) {
    let migrate = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
    };
    match migrate() {
        Ok(()) => {
            tracing::info!(dir = %dir.display(), "migrated into node cgroup (detached from pod scope)")
        }
        Err(e) => tracing::error!(
            dir = %dir.display(),
            error = %e,
            "FAILED to migrate into the node cgroup; this daemon will NOT survive a pod \
             restart (in-flight egress dies with the next roll, the pre-ADR-0121 behavior)"
        ),
    }
}

fn load_test_roots(path: &std::path::Path) -> std::io::Result<rustls::RootCertStore> {
    let pem = std::fs::read(path)?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        let cert = cert?;
        roots
            .add(cert)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    }
    Ok(roots)
}
