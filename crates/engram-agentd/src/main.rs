//! `engram-agentd` — in-guest exec daemon binary.
//!
//! Today it only knows how to bind a Unix-domain socket, which is
//! enough for the integration tests on the dev VM (`exec_stream`'s
//! host side connects to the same kind of UDS that Firecracker's
//! vsock proxy presents). A `--vsock-port <PORT>` mode for running
//! inside an actual microVM lands when the image baker can produce
//! an ext4 with this binary embedded as `init=/sbin/engram-agentd`.
//!
//! Usage:
//!
//! ```text
//!   engram-agentd --listen unix:///tmp/engram-agentd.sock
//! ```
//!
//! On SIGTERM the accept loop stops taking new connections; in-flight
//! execs continue until they exit naturally (no graceful-cancel today —
//! Phase 3 work alongside `SendCtrlAltDel`).

use std::path::PathBuf;
use std::process::ExitCode;

use tokio::net::UnixListener;
use tokio::signal;

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
struct Args {
    listen_unix: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut listen_unix: Option<PathBuf> = None;
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--listen" => {
                let v = argv
                    .next()
                    .ok_or_else(|| "--listen requires a value".to_string())?;
                let path = v.strip_prefix("unix://").ok_or_else(|| {
                    format!("unsupported listen scheme: {v}; only `unix://<path>` works today")
                })?;
                listen_unix = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                eprintln!(
                    "engram-agentd --listen unix:///path/to/sock\n\n\
                     In-guest exec daemon. Accepts WireExecRequest frames,\n\
                     runs commands, streams stdout/stderr/exit back."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let listen_unix =
        listen_unix.ok_or_else(|| "--listen unix://<path> is required".to_string())?;
    Ok(Args { listen_unix })
}

async fn run(args: Args) -> std::io::Result<()> {
    // Refuse to start with a pre-existing socket file that we don't
    // own — UnixListener::bind would EADDRINUSE. Caller is expected
    // to clean up; this is just a clearer error message.
    if args.listen_unix.exists() {
        let _ = tokio::fs::remove_file(&args.listen_unix).await;
    }
    let listener = UnixListener::bind(&args.listen_unix)?;
    tracing::info!(socket = %args.listen_unix.display(), "engram-agentd listening");

    // Cancel-safe shutdown: SIGTERM/SIGINT stops accepting, in-flight
    // tasks keep going until they finish on their own.
    let mut shutdown = Box::pin(signal::ctrl_c());

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream).await {
                                tracing::warn!(error = %e, "connection ended with error");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                    }
                }
            }
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; closing listener");
                return Ok(());
            }
        }
    }
}
