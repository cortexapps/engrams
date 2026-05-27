//! `engram-agentd` — in-guest exec daemon binary.
//!
//! Two listen modes:
//!
//! - `--listen unix:///path/to/sock` — Unix-domain socket. Used by the
//!   integration tests on dev hosts (where the host connects directly
//!   to the same UDS path our process bound).
//! - `--port <PORT>` — host↔guest transport listener (Linux-only),
//!   selected at runtime by [`engram_transport::from_env`] reading
//!   `ENGRAM_TRANSPORT`. Two impls:
//!   - `vsock` (default; Firecracker production path) — listens on
//!     AF_VSOCK port `<PORT>`, host dials via FC's vsock proxy.
//!   - `console` (VZ on Apple Silicon) — opens
//!     `/dev/hvc<N>` for the well-known port mapping; host pairs
//!     bytes via VZVirtioConsoleDevice + NSFileHandle. See
//!     `crates/engram-transport/src/console.rs` for the port→hvc
//!     map.
//!
//! On SIGTERM/SIGINT the accept loop stops taking new connections;
//! in-flight execs continue until they exit naturally (no
//! graceful-cancel today — Phase 3 work alongside `SendCtrlAltDel`).

use std::path::PathBuf;
use std::process::ExitCode;

use engram_agentd::serve_connection;

fn main() -> ExitCode {
    // ADR 0019: the host injects the OTLP collector endpoint + parent trace
    // context into the kernel cmdline on cold boot (BootSource.boot_args).
    // Adopt the endpoint into the env *before* telemetry init so the guest
    // exports to the same Jaeger/collector as the host. Set before the
    // runtime/threads start, so the env write is safe.
    if let Some(ep) = kernel_cmdline_value("engram_otel") {
        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none() {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", ep);
        }
    }

    // Held for the lifetime of `main`; declared before the runtime so it
    // drops *after* the runtime, flushing pending OTLP spans on shutdown.
    // OTLP is inert unless `OTEL_EXPORTER_OTLP_ENDPOINT` ends up set.
    let _telemetry = engram_telemetry::init(engram_telemetry::Config {
        service_name: "engram-agentd",
        default_filter: "info",
    });

    // Root span parented on the host's cold-boot trace (if propagated via
    // `engram_traceparent`). Its start offset in the trace reveals how long
    // the guest spent in kernel boot + ext4 mount + chunked-NBD page-in
    // before agentd ran — the bulk of the `agent_handshake` wait (ADR 0019).
    let span = tracing::info_span!("agentd.run", boot_elapsed_s = tracing::field::Empty);
    if let Some(tp) = kernel_cmdline_value("engram_traceparent") {
        engram_telemetry::set_parent_from_traceparent(&span, &tp);
    }
    // Record the pre-agentd boot duration on the span (and log it), so the
    // cold-boot trace separates "guest booting before agentd ran" (kernel +
    // ext4 mount + page-in) from agentd's own startup. The shim's
    // `engram-init: mark` console lines split this further (firecracker.log).
    if let Some(secs) = boot_uptime_secs() {
        span.record("boot_elapsed_s", secs);
        tracing::info!(
            boot_elapsed_s = secs,
            "agentd starting (pre-agentd boot window)"
        );
    }

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

    match rt.block_on(tracing::Instrument::instrument(run(args), span)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engram-agentd: {e}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)] // Transport(u32) field is only read on Linux; on other
                    // targets the variant just produces a clean
                    // "Unsupported" error from engram_transport::from_env
enum Listen {
    Unix(PathBuf),
    /// `--port <PORT>` — listen via the transport selected by
    /// `ENGRAM_TRANSPORT` (vsock or virtio-console).
    Transport(u32),
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
            "--port" | "--vsock-port" => {
                // `--vsock-port` retained as a deprecated alias for
                // back-compat with existing FC bakes that hardcode
                // it. Prefer `--port` going forward.
                if listen.is_some() {
                    return Err("--listen and --port are mutually exclusive".into());
                }
                let v = argv
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                let port: u32 = v.parse().map_err(|e| format!("{arg} must be a u32: {e}"))?;
                listen = Some(Listen::Transport(port));
            }
            "--token" => {
                let v = argv
                    .next()
                    .ok_or_else(|| "--token requires a value".to_string())?;
                token = Some(v);
            }
            "-h" | "--help" => {
                eprintln!(
                    "engram-agentd [--listen unix:///path/to/sock | --port <PORT>] \\\n  \
                     [--token <T>]\n\n\
                     In-guest exec daemon. Accepts WireExecRequest frames,\n\
                     runs commands, streams stdout/stderr/exit back.\n\n\
                     `--port` listens via ENGRAM_TRANSPORT (vsock|console).\n\
                     `--vsock-port` is a deprecated alias retained for FC bakes.\n\n\
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
    let listen = listen.ok_or_else(|| "one of --listen or --port is required".to_string())?;
    let token = token
        .or_else(|| std::env::var("ENGRAM_AGENT_TOKEN").ok())
        .or_else(token_from_kernel_cmdline);
    Ok(Args { listen, token })
}

/// Read `/proc/cmdline` (Linux only) and pull the value of a `<key>=<v>`
/// kernel arg. Whitespace-delimited per the kernel's own parser. Returns
/// `None` on non-Linux, on read failure, or if the arg isn't there. The
/// host injects these via Firecracker's `BootSource.boot_args` so
/// production guests don't need explicit CLI args. Used for
/// `engram_token`, and (ADR 0019) `engram_otel` / `engram_traceparent`.
fn kernel_cmdline_value(key: &str) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let prefix = format!("{key}=");
    let raw = std::fs::read_to_string("/proc/cmdline").ok()?;
    raw.split_ascii_whitespace()
        .find_map(|part| part.strip_prefix(prefix.as_str()).map(|t| t.to_string()))
}

fn token_from_kernel_cmdline() -> Option<String> {
    kernel_cmdline_value("engram_token")
}

/// Seconds since kernel boot (`/proc/uptime` field 1), Linux only. At
/// agentd's start this is ≈ the whole pre-agentd cold-boot window (kernel
/// boot + rootfs ext4 mount + chunked-NBD page-in + engram-init), which the
/// host can't observe from outside the VM (ADR 0019).
fn boot_uptime_secs() -> Option<f64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let raw = std::fs::read_to_string("/proc/uptime").ok()?;
    raw.split_ascii_whitespace().next()?.parse::<f64>().ok()
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
    // ADR 0015 M1: one supervisor owns the harness child process
    // across the lifetime of the agent. Shared across all
    // serve_connection tasks so a fresh `SpawnHarness` from any
    // connection finds (and kills) the current child before
    // launching the new one.
    let supervisor = engram_agentd::HarnessSupervisor::new();
    match args.listen {
        Listen::Unix(path) => run_unix(path, token, supervisor).await,
        Listen::Transport(port) => run_transport(port, token, supervisor).await,
    }
}

async fn run_unix(
    listen_path: PathBuf,
    token: Option<String>,
    supervisor: std::sync::Arc<engram_agentd::HarnessSupervisor>,
) -> std::io::Result<()> {
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
                Ok((stream, _addr)) => spawn_serve(stream, token.clone(), supervisor.clone()),
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; closing listener");
                return Ok(());
            }
        }
    }
}

/// Listen via the runtime-selected transport (vsock or
/// virtio-console). Each accepted connection is handed to a fresh
/// `serve_connection` task; vsock yields concurrent streams as
/// expected, console reopens `/dev/hvcN` per accept and effectively
/// serializes (one exec at a time per port).
async fn run_transport(
    port: u32,
    token: Option<String>,
    supervisor: std::sync::Arc<engram_agentd::HarnessSupervisor>,
) -> std::io::Result<()> {
    let transport = engram_transport::from_env()?;
    let mut listener = transport.listen(port).await?;
    let kind = std::env::var("ENGRAM_TRANSPORT").unwrap_or_else(|_| "vsock".into());
    tracing::info!(port, transport = %kind, "engram-agentd listening");

    // ADR 0015 M1: dial the host on the readiness port. The host
    // blocks on `accept()` here in its `start_agent` — replacing the
    // pre-M1 boot-race CONNECT-then-retry against port 1024. Order
    // matters: the dial happens AFTER `transport.listen(port)` has
    // succeeded so the host's subsequent SpawnHarness call (also on
    // `port`, 1024 in prod) is guaranteed to reach a live listener.
    //
    // Best-effort: failure to dial means the host won't know we're
    // ready and start_agent will time out. That's a real bug we want
    // to surface, but agentd itself can keep running — it might be a
    // restored sandbox where the host's listener path has already
    // been GC'd. Log loudly; don't die.
    let agent_version = env!("CARGO_PKG_VERSION").to_string();
    match transport
        .dial(engram_agentd::ENGRAM_AGENTD_READY_PORT)
        .await
    {
        Ok(mut conn) => {
            let ready = engram_agentd::AgentReady { agent_version };
            if let Err(e) = engram_agentd::write_msg(&mut conn, &ready).await {
                tracing::warn!(
                    error = %e,
                    port = engram_agentd::ENGRAM_AGENTD_READY_PORT,
                    "AgentReady write failed; host will block on start_agent",
                );
            } else {
                tracing::info!(
                    port = engram_agentd::ENGRAM_AGENTD_READY_PORT,
                    "AgentReady frame written to host",
                );
            }
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut conn).await;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                port = engram_agentd::ENGRAM_AGENTD_READY_PORT,
                "ready-port dial failed; host start_agent will block until its deadline. \
                 Likely cause: snapshot restore (no host listener bound for this sandbox), \
                 or host misconfigured. Proceeding to serve RPCs anyway.",
            );
        }
    }

    let mut shutdown = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok(stream) => spawn_serve(stream, token.clone(), supervisor.clone()),
                Err(e) => tracing::warn!(error = %e, "transport accept failed"),
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
fn spawn_serve<S>(
    stream: S,
    token: Option<String>,
    supervisor: std::sync::Arc<engram_agentd::HarnessSupervisor>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) = serve_connection(stream, token, supervisor).await {
            tracing::warn!(error = %e, "connection ended with error");
        }
    });
}
