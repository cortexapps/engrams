//! Standalone noop engine using the shared harness connection loop.

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use engram_core::SessionId;
use engram_harness_noop::{run_engine, Script, HARNESS_VERSION};
use engram_harness_sdk::{Channels, ConnectionConfig};

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
    /// (vsock) and dials accordingly. Mutually exclusive with
    /// `--connect`. `--vsock-host` is a deprecated alias kept for
    /// back-compat with existing FC bakes.
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
    /// Actual tool delay in seconds; overrides the millisecond option.
    #[arg(long)]
    tool_sleep_secs: Option<u64>,
    /// Wait for a Prompt before starting a run.
    #[arg(long)]
    no_autorun: bool,
    /// Write the harness process id to this file.
    #[arg(long)]
    pid_file: Option<std::path::PathBuf>,
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

    if let Some(path) = cli.pid_file {
        if let Err(e) = std::fs::write(path, std::process::id().to_string()) {
            tracing::error!(error = %e, "write pid file failed");
            return ExitCode::FAILURE;
        }
    }
    let script = Script {
        autorun: !cli.no_autorun,
        tool_calls: cli.tool_calls,
        interval: Duration::from_secs(cli.interval_secs),
        tool_sleep: cli
            .tool_sleep_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_millis(cli.tool_call_duration_ms)),
        result_summary: cli
            .transcript_template
            .unwrap_or_else(|| "ok (noop)".into()),
        send_run_completed: cli.send_run_completed,
        ..Script::default()
    };
    let channels = Channels::new();
    let engine = tokio::spawn(run_engine(
        script,
        channels.command_rx,
        channels.reattach.clone(),
        channels.event_tx,
    ));
    engram_harness_sdk::serve(
        ConnectionConfig {
            connect: cli.connect,
            port: cli.vsock_host,
            session_id: cli.session_id,
            harness_version: HARNESS_VERSION.into(),
        },
        engine,
        channels.command_tx,
        channels.event_rx,
        channels.reattach,
    )
    .await
}
