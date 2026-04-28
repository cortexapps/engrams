//! `engram-agentd` — in-guest exec daemon binary.
//!
//! Two listen modes:
//!
//! - `--listen unix:///path/to/sock` — Unix-domain socket. Used by the
//!   integration tests on dev hosts (where the host connects directly
//!   to the same UDS path our process bound).
//! - `--vsock-port <PORT>` — AF_VSOCK listener (Linux-only). The
//!   production deployment: this binary is baked into the rootfs at
//!   `/sbin/engram-agentd`, the boot init exec's it, Firecracker
//!   proxies `<vsock_uds>_<PORT>` ↔ guest port. The host's
//!   `FirecrackerBackend::exec_stream` connects to that proxy.
//!
//! On SIGTERM/SIGINT the accept loop stops taking new connections;
//! in-flight execs continue until they exit naturally (no
//! graceful-cancel today — Phase 3 work alongside `SendCtrlAltDel`).

use std::path::PathBuf;
use std::process::ExitCode;

use engram_agentd::serve_connection;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("engram-agentd: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    match rt.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engram-agentd: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)] // Vsock(u32) field is only read on Linux; on other targets
                    // the variant just produces a clean "Unsupported" error
enum Listen {
    Unix(PathBuf),
    Vsock(u32),
}

#[derive(Debug)]
struct Args {
    listen: Listen,
    /// Optional first-frame token. When `Some`, the agent requires
    /// every connecting host to present it via [`WireHandshake`]
    /// before it'll accept a [`WireExecRequest`]. Resolved at startup
    /// from `--token <T>`, then `ENGRAM_AGENT_TOKEN`, then
    /// `engram_token=<T>` on `/proc/cmdline` (Linux only). `None` =
    /// dev/back-compat path with no auth.
    token: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut listen: Option<Listen> = None;
    let mut token: Option<String> = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--listen" => {
                if listen.is_some() {
                    return Err("--listen and --vsock-port are mutually exclusive".into());
                }
                let v = argv
                    .next()
                    .ok_or_else(|| "--listen requires a value".to_string())?;
                let path = v.strip_prefix("unix://").ok_or_else(|| {
                    format!("unsupported listen scheme: {v}; expected `unix://<path>`")
                })?;
                listen = Some(Listen::Unix(PathBuf::from(path)));
            }
            "--vsock-port" => {
                if listen.is_some() {
                    return Err("--listen and --vsock-port are mutually exclusive".into());
                }
                let v = argv
                    .next()
                    .ok_or_else(|| "--vsock-port requires a value".to_string())?;
                let port: u32 = v
                    .parse()
                    .map_err(|e| format!("--vsock-port must be a u32: {e}"))?;
                listen = Some(Listen::Vsock(port));
            }
            "--token" => {
                let v = argv
                    .next()
                    .ok_or_else(|| "--token requires a value".to_string())?;
                token = Some(v);
            }
            "-h" | "--help" => {
                eprintln!(
                    "engram-agentd [--listen unix:///path/to/sock | --vsock-port <PORT>] \\\n  \
                     [--token <T>]\n\n\
                     In-guest exec daemon. Accepts WireExecRequest frames,\n\
                     runs commands, streams stdout/stderr/exit back.\n\n\
                     With --token, the host must send a WireHandshake with\n\
                     the matching token before WireExecRequest is accepted.\n\
                     Falls back to ENGRAM_AGENT_TOKEN, then engram_token=<T>\n\
                     on /proc/cmdline (Linux only)."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let listen = listen.ok_or_else(|| "one of --listen or --vsock-port is required".to_string())?;
    let token = token
        .or_else(|| std::env::var("ENGRAM_AGENT_TOKEN").ok())
        .or_else(token_from_kernel_cmdline);
    Ok(Args { listen, token })
}

/// Read `/proc/cmdline` (Linux only) and pull the value of an
/// `engram_token=<T>` token-arg if present. Whitespace-delimited per
/// the kernel's own parser. Returns `None` on non-Linux, on read
/// failure, or if the arg isn't there. The host injects this via
/// Firecracker's `BootSource.boot_args` so production guests don't
/// need an explicit `--token` CLI arg.
fn token_from_kernel_cmdline() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let raw = std::fs::read_to_string("/proc/cmdline").ok()?;
    raw.split_ascii_whitespace()
        .find_map(|part| part.strip_prefix("engram_token=").map(|t| t.to_string()))
}

async fn run(args: Args) -> std::io::Result<()> {
    let token = args.token.clone();
    if token.is_some() {
        tracing::info!("first-frame token auth enabled");
    } else {
        tracing::warn!(
            "no token configured (--token / ENGRAM_AGENT_TOKEN / engram_token=); \
             accepting any host that can reach the listener"
        );
    }
    match args.listen {
        Listen::Unix(path) => run_unix(path, token).await,
        #[cfg(target_os = "linux")]
        Listen::Vsock(port) => run_vsock(port, token).await,
        #[cfg(not(target_os = "linux"))]
        Listen::Vsock(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "--vsock-port is Linux-only (AF_VSOCK)",
        )),
    }
}

async fn run_unix(listen_path: PathBuf, token: Option<String>) -> std::io::Result<()> {
    use tokio::net::UnixListener;
    if listen_path.exists() {
        let _ = tokio::fs::remove_file(&listen_path).await;
    }
    let listener = UnixListener::bind(&listen_path)?;
    tracing::info!(socket = %listen_path.display(), "engram-agentd listening (unix)");

    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _addr)) => spawn_serve(stream, token.clone()),
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; closing listener");
                return Ok(());
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn run_vsock(port: u32, token: Option<String>) -> std::io::Result<()> {
    use tokio_vsock::{VsockAddr, VsockListener, VMADDR_CID_ANY};

    let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))?;
    tracing::info!(port, "engram-agentd listening (vsock)");

    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _addr)) => spawn_serve(stream, token.clone()),
                Err(e) => tracing::warn!(error = %e, "vsock accept failed"),
            },
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; closing listener");
                return Ok(());
            }
        }
    }
}

/// Hand an accepted stream to [`serve_connection`] on its own task,
/// logging any per-connection error without tearing the listener down.
fn spawn_serve<S>(stream: S, token: Option<String>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = serve_connection(stream, token).await {
            tracing::warn!(error = %e, "connection ended with error");
        }
    });
}
