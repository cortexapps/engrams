//! ADR 0121: the node-local egress daemon, as a library.
//!
//! The binary (`src/main.rs`) is a thin shell: CLI + env → [`DaemonArgs`]
//! → [`run`]. The library shape exists for the host-agent test
//! harnesses, which drive a REAL daemon as an in-process task over a
//! real control UDS — same data plane, no child-process management in
//! unit-shaped tests. Production always runs the binary (the whole
//! point is a process no pod lifecycle owns).
//!
//! Startup order (each step's failure returns non-zero with NO
//! manifest written, so the spawning host-agent fails closed per ADR
//! 0083): cgroup self-migrate → parse CA → build the proxy → bind the
//! control UDS → bind the listeners (`bind_with_retry`) → write the
//! manifest → serve.

mod control;
mod coord;
mod dialback;

use std::path::PathBuf;
use std::sync::Arc;

use engram_egress_proxy::{CertMint, Proxy, ProxyConfig, Registry};

/// The build-time source fingerprint (a content hash of this binary's
/// source dependency closure, injected by the image build). `None`
/// for local builds — which are never adopted across a restart.
/// `option_env!` requires the literal; keep it equal to
/// `engram_egress_proto::BUILD_ENV_SOURCE_FINGERPRINT`.
pub const SOURCE_FINGERPRINT: Option<&str> = option_env!("ENGRAM_EGRESS_PROXYD_FINGERPRINT");

/// Everything the daemon needs, already resolved. The binary fills
/// this from CLI + env; tests construct it directly (no process-global
/// env, so parallel tests can each run their own daemon).
pub struct DaemonArgs {
    /// The host-agent work dir: control socket, dial-back socket,
    /// manifest, and log file all live here.
    pub work_dir: PathBuf,
    pub proxy_port: u16,
    pub dns_port: u16,
    pub gateway_port: u16,
    /// cgroup-v2 leaf to migrate into before binding (the ADR 0044 K2
    /// escape). Best-effort with a loud ERROR on failure — the daemon
    /// still serves; it just won't survive the next roll (degraded to
    /// the pre-ADR-0121 status quo, never worse).
    pub cgroup_dir: Option<PathBuf>,
    /// Upstream the filtering DNS proxy forwards allowed queries to.
    pub dns_upstream: std::net::SocketAddr,
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub coord_url: String,
    pub coord_token: Option<String>,
    pub host_id: engram_core::HostId,
    /// TEST ONLY: extra upstream trust roots for hermetic fixtures.
    pub test_upstream_roots: Option<rustls::RootCertStore>,
    /// TEST ONLY: static SNI resolutions; replaces the system resolver.
    pub test_resolves: Vec<(String, std::net::SocketAddr)>,
}

impl DaemonArgs {
    /// Production defaults around the required inputs.
    pub fn new(
        work_dir: PathBuf,
        ca_cert_pem: String,
        ca_key_pem: String,
        coord_url: String,
        coord_token: Option<String>,
        host_id: engram_core::HostId,
    ) -> Self {
        Self {
            work_dir,
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            cgroup_dir: None,
            dns_upstream: "1.1.1.1:53".parse().expect("default dns upstream parses"),
            ca_cert_pem,
            ca_key_pem,
            coord_url,
            coord_token,
            host_id,
            test_upstream_roots: None,
            test_resolves: Vec::new(),
        }
    }
}

/// Run the daemon to completion. Returns the process exit code: `0`
/// only for a graceful control-socket `Shutdown` (the upgrade
/// replace); any startup failure or a dead accept loop is non-zero so
/// the supervising host-agent's exit event respawns it.
pub async fn run(args: DaemonArgs) -> i32 {
    // Escape the pod cgroup FIRST — before any listener exists, so a
    // daemon that a pod kill can still reach never holds the ports.
    if let Some(dir) = &args.cgroup_dir {
        migrate_to_cgroup(dir);
    }

    let ca = match engram_egress_proxy::Ca::from_pem(&args.ca_cert_pem, &args.ca_key_pem) {
        Ok(ca) => ca,
        Err(e) => {
            tracing::error!(error = %e, "egress CA parse failed; exiting (fail-closed)");
            return 1;
        }
    };
    let ca_fingerprint = control::sha256_hex(args.ca_cert_pem.as_bytes());

    // One rustls provider per process. `install_default` errors when a
    // co-resident caller already set it (the in-process test harness);
    // that is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Port 0 would bind an ephemeral port while iptables REDIRECTs the
    // configured literal — the ADR 0083 split-brain. There is no
    // `0 = off` sentinel (egress is mandatory, issue #240).
    if args.proxy_port == 0 || args.dns_port == 0 || args.gateway_port == 0 {
        tracing::error!(
            proxy = args.proxy_port,
            dns = args.dns_port,
            gateway = args.gateway_port,
            "egress ports must be non-zero; exiting (fail-closed)"
        );
        return 1;
    }

    let registry = Arc::new(Registry::new());
    let mint = Arc::new(CertMint::new(Arc::new(ca)));
    let coord = coord::SlimCoordClient::new(args.coord_url.clone(), args.coord_token.clone());

    let cloud_sql_pool = Arc::new(engram_egress_proxy::TunnelPool::new(
        coord::CloudSqlEndpointFactory::new(coord.clone(), args.host_id),
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
    let mut proxy_cfg = ProxyConfig::new(bind(args.proxy_port), registry.clone(), mint);
    proxy_cfg.dns_bind_addr = Some(bind(args.dns_port));
    proxy_cfg.guest_gateway_bind_addr = Some(bind(args.gateway_port));
    proxy_cfg.dns_upstream = args.dns_upstream;
    proxy_cfg.guest_gateway = guest_gateway.clone();
    proxy_cfg.observe_sink = Some(coord::observe_sink(coord.clone()));
    proxy_cfg.inject_refresher = Some(Arc::new(coord::CoordInjectRefresher::new(
        coord.clone(),
        args.host_id,
    )));
    proxy_cfg.guest_port_dialer = Some(Arc::new(dialback::DialbackGuestPortDialer::new(
        args.work_dir.join(engram_egress_proto::DIALBACK_SOCK_NAME),
    )));
    proxy_cfg.upstream_test_roots = args.test_upstream_roots.clone();
    if !args.test_resolves.is_empty() {
        let mut resolver = engram_egress_proxy::StaticResolver::new();
        for (host, addr) in &args.test_resolves {
            resolver = resolver.with(host.clone(), *addr);
        }
        proxy_cfg.resolver = Arc::new(resolver);
    }

    let proxy = Proxy::new(proxy_cfg);

    // Control UDS before the listeners: the spawning host-agent's
    // readiness gate is a Hello round-trip + the accept-loop probe,
    // and both need the socket up. A stale socket file from a dead
    // predecessor is just unlinked — the manifest/identity checks
    // already decided nothing live owns it.
    let control_sock = args.work_dir.join(engram_egress_proto::CONTROL_SOCK_NAME);
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
        proxy_port: args.proxy_port,
        dns_port: args.dns_port,
        gateway_port: args.gateway_port,
        ca_fingerprint,
        coord_url: args.coord_url.clone(),
    };
    let manifest = engram_egress_proto::manifest::ProxydManifest {
        schema_version: engram_egress_proto::manifest::MANIFEST_SCHEMA_VERSION,
        pid: identity.pid,
        start_time_jiffies: identity.start_time_jiffies,
        comm: identity.comm,
        source_fingerprint: hello.source_fingerprint.clone(),
        proxy_port: args.proxy_port,
        dns_port: args.dns_port,
        gateway_port: args.gateway_port,
        control_sock: control_sock.clone(),
    };
    if let Err(e) = engram_egress_proto::manifest::write_manifest(&args.work_dir, &manifest) {
        tracing::error!(error = %e, "manifest write failed; exiting (an unadoptable daemon must not serve)");
        return 1;
    }

    tracing::info!(
        pid = identity.pid,
        proxy_port = args.proxy_port,
        dns_port = args.dns_port,
        gateway_port = args.gateway_port,
        fingerprint = ?SOURCE_FINGERPRINT,
        "engram-egress-proxyd serving"
    );

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let control_state = Arc::new(control::ControlState {
        registry,
        gateway: guest_gateway,
        hello,
        shutdown: shutdown_tx,
    });
    tokio::spawn(control::serve(control_listener, control_state));

    tokio::select! {
        _ = proxy.serve(listeners) => {
            // `serve` loops forever on accept. If it returns, the data
            // plane is dead while iptables still REDIRECTs here: exit
            // non-zero so the supervising host-agent's exit event
            // respawns us — never linger as a control plane over a
            // dead accept loop.
            tracing::error!("egress proxy serve loop exited; exiting for respawn");
            1
        }
        _ = shutdown_rx.changed() => {
            tracing::info!("graceful shutdown (control-socket Shutdown op)");
            0
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
