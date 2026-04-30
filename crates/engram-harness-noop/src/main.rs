//! Standalone binary for the noop harness.
//!
//! Auto-spawned by `engram-host-agent` (via the `SandboxSpec::agent`
//! field) when `ENGRAM_DEV_AUTO_AGENT=noop` is set, so a session created
//! against `local://hello` immediately starts emitting fake tool
//! calls. Reads its config from CLI flags (which the host-agent
//! plumbs through `AgentSpec::argv` — env vars are reserved for
//! transport secrets like the attach token).
//!
//! On startup: dial the harness-hub TCP address, run the cadence,
//! exit when the peer closes or the host sends `Shutdown`.

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use engram_core::SessionId;
use engram_harness_noop::{run, NoopConfig, NoopOutcome};

#[derive(Parser, Debug)]
#[command(name = "engram-harness-noop", about = "Dev-mode noop harness")]
struct Cli {
    /// Hub address: `host:port`. The host-agent's TCP harness
    /// listener (ProcessBackend dev path). Mutually exclusive with
    /// `--port` — provide exactly one.
    #[arg(long, env = "ENGRAM_HARNESS_ADDR", conflicts_with = "vsock_host")]
    connect: Option<String>,

    /// In-VM transport port on the host. Used when the harness runs
    /// inside a microVM — `engram-transport` reads `ENGRAM_TRANSPORT`
    /// (vsock|console) and dials accordingly. Mutually exclusive
    /// with `--connect`. `--vsock-host` is a deprecated alias kept
    /// for back-compat with existing FC bakes.
    #[arg(long = "port", alias = "vsock-host", env = "ENGRAM_HARNESS_VSOCK_HOST")]
    vsock_host: Option<u32>,

    /// Session id this harness is attached to. The host-agent
    /// validates that this matches the session it expects on the
    /// connection (a future per-attach token will replace this as
    /// the auth primitive).
    #[arg(long, env = "ENGRAM_SESSION_ID")]
    session_id: SessionId,

    /// Number of synthetic tool calls before falling silent.
    /// Default 3 — paired with the default 5s interval and the 60s
    /// idle TTL, idle eviction reliably trips ~30s after the last
    /// completed call.
    #[arg(long, default_value_t = 3)]
    tool_calls: u32,

    /// Wall-clock between successive tool calls. Accepts plain
    /// integer seconds. Default 5.
    #[arg(long, default_value_t = 5)]
    interval_secs: u64,

    /// Synthetic per-tool-call duration reported on Completed.
    #[arg(long, default_value_t = 200)]
    tool_call_duration_ms: u64,

    /// String the noop emits as `result_summary` on each
    /// `ToolCallCompleted`. Renders directly in `engram session log`
    /// output and Slack/web UI consumers.
    #[arg(long)]
    transcript_template: Option<String>,

    /// Send `RunCompleted` after the last tool call. Off by default
    /// so the run looks like "agent finished its prompt and is
    /// waiting" — the typical input the idle evictor sees.
    #[arg(long)]
    send_run_completed: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!(
        connect = ?cli.connect,
        vsock_host = ?cli.vsock_host,
        session = %cli.session_id,
        tool_calls = cli.tool_calls,
        interval_secs = cli.interval_secs,
        "noop harness starting",
    );

    let mut cfg = NoopConfig::for_session(cli.session_id);
    cfg.tool_calls = cli.tool_calls;
    cfg.interval = Duration::from_secs(cli.interval_secs);
    cfg.tool_call_duration_ms = cli.tool_call_duration_ms;
    cfg.send_run_completed = cli.send_run_completed;
    if let Some(tmpl) = cli.transcript_template {
        cfg.result_summary_template = tmpl;
    }

    // Dial the hub. Two flavors:
    //   --connect host:port  → TCP loopback (ProcessBackend dev)
    //   --port <port>        → in-VM transport selected by
    //                          ENGRAM_TRANSPORT (vsock|console)
    // clap rejects "neither" / "both" via `conflicts_with`; the
    // outer match here covers the two valid shapes.
    let outcome = match (cli.connect.as_deref(), cli.vsock_host) {
        (Some(addr), None) => match tokio::net::TcpStream::connect(addr).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                run(s, cfg).await
            }
            Err(e) => {
                tracing::error!(error = %e, addr = %addr, "noop harness: TCP dial failed");
                return ExitCode::from(1);
            }
        },
        (None, Some(port)) => dial_transport_and_run(port, cfg).await,
        _ => {
            tracing::error!("provide exactly one of --connect or --port");
            return ExitCode::from(2);
        }
    };

    // The host-agent splits sandbox creation from agent spawn so
    // routing (`HarnessHub::bind_session`) is in place by the time
    // we dial. A rejected attach here means a real misconfiguration,
    // not a race; bail out instead of retrying.
    match outcome {
        Ok(NoopOutcome::AttachRejected) => {
            tracing::error!("noop harness: attach rejected by host (no session bound?)");
            ExitCode::from(1)
        }
        Ok(outcome) => {
            tracing::info!(?outcome, "noop harness done");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, "noop harness: run failed");
            ExitCode::from(1)
        }
    }
}

/// Dial the host on `port` via the runtime-selected transport (vsock
/// or virtio-console — see `engram-transport`). Linux-only; on
/// non-Linux hosts `engram_transport::from_env` returns
/// `ErrorKind::Unsupported`.
async fn dial_transport_and_run(
    port: u32,
    cfg: NoopConfig,
) -> Result<NoopOutcome, engram_harness_noop::NoopError> {
    let transport = engram_transport::from_env().map_err(engram_harness_noop::NoopError::Io)?;
    let stream = transport.dial(port).await.map_err(|e| {
        tracing::error!(error = %e, port, "transport dial failed");
        engram_harness_noop::NoopError::Io(e)
    })?;
    run(stream, cfg).await
}
