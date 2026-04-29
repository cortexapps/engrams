//! Standalone binary for the noop harness.
//!
//! Auto-spawned by `engram-host-agent` (via the `SandboxSpec::agent`
//! field) when `ENGRAM_DEV_AUTO_NOOP=1` is set, so a session created
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
    /// Hub address: `host:port`. The host-agent's harness listener.
    #[arg(long, env = "ENGRAM_HARNESS_ADDR")]
    connect: String,

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

    /// Bytes the harness pretends it appended to the transcript file
    /// for each call. Default = a tiny JSONL line with the call
    /// index — readable in `engram session log` output.
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
        addr = %cli.connect,
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
        cfg.transcript_delta_template = tmpl.into_bytes();
    }

    // Retry attach on rejection: the host-agent's session→sandbox
    // binding is set after `backend.create()` returns, which races
    // with the agent's dial inside that same `create()`. Five
    // attempts at 50ms-1s exponential backoff comfortably covers the
    // race in practice.
    let mut backoff = Duration::from_millis(50);
    for attempt in 1..=5 {
        let stream = match tokio::net::TcpStream::connect(&cli.connect).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, attempt, "noop harness: dial failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
                continue;
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!(error = %e, "noop harness: set_nodelay failed (continuing)");
        }

        match run(stream, cfg.clone()).await {
            Ok(NoopOutcome::AttachRejected) => {
                tracing::warn!(attempt, "noop harness: attach rejected; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
                continue;
            }
            Ok(outcome) => {
                tracing::info!(?outcome, "noop harness done");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                tracing::error!(error = %e, "noop harness: run failed");
                return ExitCode::from(1);
            }
        }
    }
    tracing::error!("noop harness: gave up after 5 attach attempts");
    ExitCode::from(1)
}
