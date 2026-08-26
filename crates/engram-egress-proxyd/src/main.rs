//! The daemon binary: CLI + env → [`engram_egress_proxyd::DaemonArgs`]
//! → [`engram_egress_proxyd::run`]. All daemon logic lives in the
//! library so the host-agent test harnesses can drive a real daemon
//! in-process; this shell only parses inputs and owns the tracing
//! setup (log file in the work dir — pod-log capture dies with the
//! pod; the daemon's diagnostics must not).

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

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
    /// escape: out of the pod's kill domain).
    #[arg(long)]
    cgroup_dir: Option<PathBuf>,

    /// Upstream the filtering DNS proxy forwards allowed queries to.
    #[arg(long, default_value = "1.1.1.1:53")]
    dns_upstream: std::net::SocketAddr,

    /// Log file. Defaults to `<work_dir>/egress-proxyd.log`.
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

    let args = match daemon_args(&cli) {
        Ok(args) => args,
        Err(e) => {
            tracing::error!(error = %e, "egress-proxyd config invalid; exiting (fail-closed)");
            std::process::exit(1);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime builds from static settings");
    let code = runtime.block_on(engram_egress_proxyd::run(args));
    std::process::exit(code);
}

fn daemon_args(cli: &Cli) -> Result<engram_egress_proxyd::DaemonArgs, String> {
    // CA + coord config from spawn-time env (the `EnvCaSource` pattern
    // — argv leaks on /proc/*/cmdline, env does not).
    let ca_cert_pem = std::env::var(engram_egress_proto::ENV_CA_CERT_PEM)
        .map_err(|_| format!("env var `{}` not set", engram_egress_proto::ENV_CA_CERT_PEM))?;
    let ca_key_pem = std::env::var(engram_egress_proto::ENV_CA_KEY_PEM)
        .map_err(|_| format!("env var `{}` not set", engram_egress_proto::ENV_CA_KEY_PEM))?;
    let coord_url = std::env::var(engram_egress_proto::ENV_COORD_URL)
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let coord_token = std::env::var(engram_egress_proto::ENV_COORD_TOKEN)
        .ok()
        .filter(|t| !t.is_empty());
    let host_id: engram_core::HostId = std::env::var(engram_egress_proto::ENV_HOST_ID)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .ok_or_else(|| {
            format!(
                "env var `{}` missing or unparseable",
                engram_egress_proto::ENV_HOST_ID
            )
        })?;

    let mut args = engram_egress_proxyd::DaemonArgs::new(
        cli.work_dir.clone(),
        ca_cert_pem,
        ca_key_pem,
        coord_url,
        coord_token,
        host_id,
    );
    args.proxy_port = cli.proxy_port;
    args.dns_port = cli.dns_port;
    args.gateway_port = cli.gateway_port;
    args.cgroup_dir = cli.cgroup_dir.clone();
    args.dns_upstream = cli.dns_upstream;
    if let Some(pem_path) = &cli.test_upstream_root_pem {
        args.test_upstream_roots = Some(
            load_test_roots(pem_path)
                .map_err(|e| format!("test upstream roots {}: {e}", pem_path.display()))?,
        );
    }
    for entry in &cli.test_resolve {
        let (host, addr) = entry
            .split_once('=')
            .ok_or_else(|| format!("--test-resolve `{entry}` wants host=ip:port"))?;
        let addr = addr
            .parse()
            .map_err(|e| format!("--test-resolve `{entry}` addr unparseable: {e}"))?;
        args.test_resolves.push((host.to_string(), addr));
    }
    Ok(args)
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
