//! Harness adapter for the `claude` CLI.
//!
//! Lives at `/sbin/engram-harness-claude` inside the rootfs (or
//! cargo-target/.../engram-harness-claude on the dev-mode
//! ProcessBackend path). Exec'd by `engram-agentd`'s harness
//! supervisor after the host pushes a `SpawnHarness` request
//! describing this binary.
//!
//! Strategy: a single **persistent streaming** `claude` process per
//! session. The wrapper spawns `claude --print --input-format
//! stream-json --output-format stream-json --verbose
//! --dangerously-skip-permissions [--resume <claude_id>]` ONCE, holds
//! its stdin open, and writes one newline-delimited `user` message per
//! prompt. Turn boundaries come from claude's explicit `result` line on
//! the stream — NOT from process EOF — so a clean turn-end and a crash
//! are now unambiguous (EOF mid-turn = a real crash, auto-recovered by
//! respawning `--resume`). This is the root-cause fix for the
//! child-per-prompt EOF-inference wedge (ADR 0052).
//!
//! `--dangerously-skip-permissions` is mandatory: there's no human in
//! the VM to answer permission prompts, and `--print` mode aborts
//! with exit 1 the first time a tool needs approval otherwise. The
//! sandbox is the safety boundary, not Claude's per-tool consent.
//!
//! Each turn's `system`/`init` line carries Claude's session id, which
//! we stash in `/workspace/.engram/claude-session-id` so a respawn
//! (idle-resume or crash recovery) can `--resume` into the same
//! conversation. Lost on cold death (Dead status); a forked session
//! gets a fresh Claude conversation.

// Cross-platform stub — vsock dialing is Linux-only, and the
// adapter only ships inside FC rootfs / Linux ProcessBackend.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("engram-harness-claude is Linux-only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod adapter {
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitCode, ExitStatus, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use clap::Parser;
    use engram_core::SessionId;
    use engram_harness_proto::{
        read_msg, write_msg, AgentRole, HarnessAttach, HarnessAttachAck, HarnessCommand,
        HarnessEvent, HarnessFrame,
    };
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
    use tokio::process::Command;
    use tokio::sync::{mpsc, Notify};
    use tokio::time::{timeout, Instant};

    pub const CLAUDE_SESSION_ID_FILE: &str = "/workspace/.engram/claude-session-id";

    /// Truncation budgets used when building summary fields. Adapter-
    /// local enforcement of the wire docs.
    pub const MAX_ARGS_SUMMARY_BYTES: usize = 1024;
    pub const MAX_RESULT_SUMMARY_BYTES: usize = 4096;
    pub const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;

    /// Abnormal-exit diagnostics: how much of `claude`'s stderr to retain
    /// for the crash artifact. Bounded so a chatty child can't grow the
    /// tail without limit — the drain task always reads the pipe, so it
    /// never wedges (the reason stderr was historically `inherit`ed).
    pub const MAX_STDERR_TAIL_LINES: usize = 64;
    pub const MAX_STDERR_TAIL_BYTES: usize = 4096;

    #[derive(Parser, Debug, Clone)]
    #[command(
        name = "engram-harness-claude",
        about = "Engram adapter for Claude Code"
    )]
    pub struct Cli {
        /// TCP host:port of the harness hub (ProcessBackend dev path).
        /// Mutually exclusive with `--vsock-host`.
        #[arg(long, env = "ENGRAM_HARNESS_ADDR", conflicts_with = "vsock_host")]
        pub connect: Option<String>,

        /// In-VM transport port on the host. Used when the harness
        /// runs inside a microVM — `engram-transport` reads
        /// `ENGRAM_TRANSPORT` (vsock|console) and dials accordingly.
        /// Mutually exclusive with `--connect`. `--vsock-host` is a
        /// deprecated alias kept for back-compat with FC bakes.
        #[arg(long = "port", alias = "vsock-host", env = "ENGRAM_HARNESS_VSOCK_HOST")]
        pub vsock_host: Option<u32>,

        /// Engram session id (from `ENGRAM_SESSION_ID`). Sent in
        /// `HarnessAttach`.
        #[arg(long, env = "ENGRAM_SESSION_ID")]
        pub session_id: SessionId,

        /// Cap on tool calls per run. Adapter logs and stops on
        /// excess; hard cost backstop. Default is intentionally
        /// permissive — the VM is the safety boundary, and a long
        /// autonomous run can easily make thousands of tool calls
        /// across a multi-hour task. Tighten per-deployment via
        /// `--max-tool-calls` if you need a stricter ceiling.
        #[arg(long, default_value_t = 100_000)]
        pub max_tool_calls: u32,

        /// Per-tool-call wall-clock cap (seconds). Reserved.
        #[arg(long, default_value_t = 600)]
        pub max_tool_call_secs: u64,

        /// Whole-run wall-clock cap (seconds). After this the
        /// adapter SIGTERMs `claude` and emits
        /// `RunCompleted{ok:false}`. Default is 24h: an autonomous
        /// agent in an isolated VM is expected to be able to grind
        /// on a task for many hours without the wrapper killing it
        /// out from under it. Tighten per-deployment if needed.
        #[arg(long, default_value_t = 86_400)]
        pub max_run_secs: u64,

        /// Override the `claude` binary path. Default: the sidecar
        /// next to this wrapper (`<argv[0] dir>/claude`), populated
        /// by the harness pack's `install-harnesses` step. Override
        /// via `ENGRAM_CLAUDE_BIN` for dev tweaks (e.g. point at a
        /// freshly-built `claude` outside the pack).
        #[arg(long, env = "ENGRAM_CLAUDE_BIN")]
        pub claude_bin: Option<String>,
    }

    /// Resolve the `claude` binary path. If the user supplied
    /// `--claude-bin` / `ENGRAM_CLAUDE_BIN`, honour it verbatim.
    /// Otherwise look for a sibling named `claude` next to this
    /// wrapper (the harness pack convention) — falls back to the
    /// bare name `claude` so $PATH lookup still works in dev shells.
    fn resolve_claude_bin(override_value: Option<&str>) -> String {
        if let Some(path) = override_value {
            return path.to_string();
        }
        if let Ok(self_exe) = std::env::current_exe() {
            if let Some(parent) = self_exe.parent() {
                let sibling = parent.join("claude");
                if sibling.exists() {
                    return sibling.to_string_lossy().into_owned();
                }
            }
        }
        // Last-resort fallback: the wrapper might be running outside
        // a pack (dev-mode `cargo run`). Trust $PATH.
        "claude".into()
    }

    pub async fn entry() -> ExitCode {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();

        let mut cli = Cli::parse();
        // Materialise the resolved claude binary path so the rest of
        // the code can treat `cli.claude_bin` as a known value.
        cli.claude_bin = Some(resolve_claude_bin(cli.claude_bin.as_deref()));
        tracing::info!(
            connect = ?cli.connect,
            vsock_host = ?cli.vsock_host,
            session = %cli.session_id,
            claude_bin = ?cli.claude_bin,
            "claude harness starting",
        );

        // CLI dispatch: TCP loopback (Process backend) vs in-VM
        // transport (FC vsock or VZ virtio-console — selected by
        // ENGRAM_TRANSPORT).
        if !((cli.connect.is_some()) ^ (cli.vsock_host.is_some())) {
            tracing::error!("provide exactly one of --connect or --port");
            return ExitCode::from(2);
        }

        // The claude run is decoupled from the host connection. The
        // engine task owns the `claude` child + the run state and is
        // spawned ONCE; it outlives every connection. A dropped host
        // link (FC snapshot/restore, host-agent restart, a slow
        // checkpoint that severs vsock) must NOT abort an in-flight
        // run — connections come and go around the engine.
        //
        //   cmd_tx/cmd_rx : host commands → engine. `cmd_tx` is held
        //       here for the whole process and only *cloned* into each
        //       connection's forwarder, so a connection drop never
        //       closes the channel — the engine never mistakes a
        //       disconnect for a "stop". THIS is the core of the fix.
        //   evt_tx/evt_rx : engine events → the current connection's
        //       writer. Bounded: while disconnected the engine
        //       backpressures (claude's stdout pipe fills) so the run
        //       *pauses* rather than losing events, resuming on reconnect.
        //   held : the one event a connection pulled but hadn't finished
        //       writing when it dropped — re-sent first on the next
        //       connection (at-least-once, no loss).
        //   reattach : pulsed when a fresh connection attaches; the
        //       engine re-announces `Idle` iff it's idle (so the host's
        //       soft idle-TTL stays armed for a genuinely-idle session),
        //       but a mid-run reconnect emits NO `Idle`.
        let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(16);
        let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(1024);
        let reattach = Arc::new(Notify::new());
        let held: HeldEvent = Arc::new(Mutex::new(None));
        let initial_prompt: Option<String> = std::env::var("ENGRAM_INITIAL_PROMPT").ok();

        let engine = tokio::spawn(run_engine(
            cli.clone(),
            cmd_rx,
            reattach.clone(),
            evt_tx,
            initial_prompt,
        ));

        // Connection loop: dial → handshake → splice (forward host
        // commands / pump engine events) until the link drops, then
        // re-dial. The engine drives termination.
        //
        // ADR 0045 C1: SIGUSR1 = "drop the connection and re-dial NOW",
        // sent by agentd's SpawnHarness-reattach arm right after a live
        // move / snapshot restore. The restore rebuilds the vsock
        // device, but this side's established connection never EOFs —
        // the splice would block forever on a read the peer can no
        // longer answer, and the new host would never see an attach.
        let mut reconnect_nudge =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                .expect("install SIGUSR1 handler");
        let mut consecutive_failures: u32 = 0;
        const MAX_BACKOFF_SECS: u64 = 10;
        // `None` → the engine finished on its own; await it below for the
        // real exit code. `Some` → we're bailing for our own reason
        // (gave up reaching the host / host rejected attach) and must
        // abort the still-running engine.
        let our_code: Option<ExitCode> = loop {
            // The engine owns shutdown: once it returns (Shutdown / all
            // command senders gone) stop reconnecting and reap its code.
            if engine.is_finished() {
                break None;
            }
            let stream = match dial(&cli).await {
                Some(s) => s,
                None => {
                    // Couldn't reach the host at all — distinct from a
                    // mid-session drop. Back off and KEEP TRYING,
                    // forever. The harness is the session's only event
                    // channel: a live move / snapshot restore rebuilds
                    // the vsock device while the guest is CPU-starved
                    // for tens of seconds, and the old give-up budget
                    // (10 failures) expired exactly then — the harness
                    // exited silently, the host kept a binding to a
                    // corpse, and every later prompt 500'd ("sandbox
                    // not found", prod canary 1f64052e). Dying never
                    // helps: a genuinely-orphaned harness is reaped by
                    // agentd's next SpawnHarness, and an idle retry at
                    // the backoff cap costs nothing.
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff =
                        std::cmp::min(MAX_BACKOFF_SECS, 1u64 << consecutive_failures.min(4));
                    if consecutive_failures.is_power_of_two() {
                        tracing::warn!(
                            consecutive_failures,
                            backoff_secs = backoff,
                            "host unreachable; retrying indefinitely"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    continue;
                }
            };
            let outcome = tokio::select! {
                outcome = run_one_connection(stream, &cli, &cmd_tx, &mut evt_rx, &held, &reattach) => outcome,
                _ = reconnect_nudge.recv() => {
                    tracing::info!(
                        "SIGUSR1 reconnect nudge (live move / restore); \
                         dropping the connection and re-dialing"
                    );
                    ConnOutcome::Dropped { reason: "SIGUSR1 reconnect nudge" }
                }
            };
            match outcome {
                ConnOutcome::EngineDone => break None,
                ConnOutcome::Rejected => break Some(ExitCode::from(1)),
                ConnOutcome::HandshakeFailed { reason } => {
                    // Same indefinite-retry posture as the dial arm: a
                    // handshake can fail transiently for as long as the
                    // host side is mid-rebind (live move), and exiting
                    // strands the session.
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff =
                        std::cmp::min(MAX_BACKOFF_SECS, 1u64 << consecutive_failures.min(4));
                    tracing::warn!(
                        reason,
                        consecutive_failures,
                        backoff_secs = backoff,
                        "harness handshake failed; reconnecting"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
                ConnOutcome::Dropped { reason } => {
                    // The connection was established and later dropped —
                    // progress, not a failure to reach the host. Reset
                    // the give-up counter so a long-lived session that
                    // reconnects many times (across checkpoints) never
                    // exhausts it; settle briefly so a flapping link
                    // doesn't hot-loop.
                    consecutive_failures = 0;
                    tracing::warn!(reason, "harness connection dropped; reconnecting");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        };

        match our_code {
            // Engine finished — reap its exit code.
            None => engine.await.unwrap_or_else(|e| {
                tracing::error!(error = %e, "engine task panicked");
                ExitCode::from(1)
            }),
            // We bailed; stop the still-running engine (process teardown
            // drops the claude child too, via kill_on_drop).
            Some(code) => {
                engine.abort();
                code
            }
        }
    }

    /// Boxed stream half-pair so the outer reconnect loop can hold the
    /// halves regardless of whether the transport was TCP or vsock.
    type BoxedReader = Box<dyn AsyncRead + Unpin + Send>;
    type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;

    /// Dial the harness hub. Returns `None` on transport failure
    /// (logged at error). The outer loop turns that into a backoff
    /// retry.
    async fn dial(cli: &Cli) -> Option<(BoxedReader, BoxedWriter)> {
        match (cli.connect.as_deref(), cli.vsock_host) {
            (Some(addr), None) => match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    let (r, w) = tokio::io::split(s);
                    Some((Box::new(r), Box::new(w)))
                }
                Err(e) => {
                    tracing::error!(error = %e, addr, "TCP dial failed");
                    None
                }
            },
            (None, Some(port)) => match engram_transport::from_env() {
                Ok(transport) => match transport.dial(port).await {
                    Ok(stream) => {
                        let (r, w) = tokio::io::split(stream);
                        Some((Box::new(r), Box::new(w)))
                    }
                    Err(e) => {
                        tracing::error!(error = %e, port, "transport dial failed");
                        None
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "build transport from ENGRAM_TRANSPORT failed");
                    None
                }
            },
            _ => unreachable!("validated in entry()"),
        }
    }

    /// The single event a connection pulled from the engine but hadn't
    /// finished writing when it dropped. Re-sent first on the next
    /// connection so a transient drop never loses an event. Only ever
    /// touched by `pump_events` (one connection at a time), under a sync
    /// lock so there's no await between pulling an event and parking it.
    type HeldEvent = Arc<Mutex<Option<HarnessEvent>>>;

    /// Outcome of one host connection's life.
    enum ConnOutcome {
        /// The engine finished (Shutdown / all command senders gone).
        /// Stop reconnecting and reap the engine's exit code.
        EngineDone,
        /// Host explicitly rejected the attach — fatal, don't retry.
        Rejected,
        /// Handshake didn't complete (transport flake at/ before attach).
        HandshakeFailed { reason: &'static str },
        /// An established connection later dropped — re-dial.
        Dropped { reason: &'static str },
    }

    /// Push an event to the connection pump. Bounded send: while
    /// disconnected the pump can't drain, so this backpressures —
    /// pausing the engine (and, transitively, claude via its stdout
    /// pipe) rather than losing events. The run resumes when a
    /// connection reattaches. Errors only if the receiver is gone,
    /// i.e. the process is tearing down; treat as a debug no-op.
    async fn emit(evt_tx: &mpsc::Sender<HarnessEvent>, ev: HarnessEvent) {
        if evt_tx.send(ev).await.is_err() {
            tracing::debug!("event channel closed; dropping event");
        }
    }

    /// The long-lived run engine. Owns the persistent `claude` process
    /// and the pending-prompt queue; spawned once and never torn down by
    /// a connection drop. Each iteration runs ONE `claude` process
    /// (`run_claude_session`); on an unexpected death it respawns with
    /// `--resume` (bounded fast-crash backoff). A clean `Shutdown` or a
    /// closed command channel exits the engine.
    ///
    /// Because `cmd_rx`'s senders outlive any single connection, a
    /// dropped link never closes it — so the engine never sees a
    /// disconnect as a reason to stop a run. This is the heart of the
    /// fix: a checkpoint pause that severs vsock mid-turn now pauses
    /// the run (via `emit` backpressure) instead of killing it and
    /// emitting a spurious `Idle`.
    async fn run_engine(
        cli: Cli,
        mut cmd_rx: mpsc::Receiver<HarnessCommand>,
        reattach: Arc<Notify>,
        evt_tx: mpsc::Sender<HarnessEvent>,
        initial_prompt: Option<String>,
    ) -> ExitCode {
        // The pending queue is owned here so it survives a respawn: a
        // prompt queued mid-turn (type-ahead) or one that hadn't started
        // when claude crashed isn't lost across the process restart.
        // Phase 1b: this IS the user-visible editable queue — each entry
        // carries its `prompt_id` (the env-seeded initial prompt has none).
        let mut pending: VecDeque<QueuedPrompt> = VecDeque::new();
        if let Some(p) = initial_prompt {
            pending.push_back(QueuedPrompt {
                prompt_id: None,
                text: p,
            });
        }

        // Bounded fast-crash backoff: a `claude` that dies within
        // FAST_CRASH_WINDOW of spawning is "failing to start" — back off
        // (capped) and, after MAX_FAST_CRASHES in a row, give up rather
        // than hot-loop spawns. A process that ran longer than the window
        // resets the counter (a healthy long session that later crashes
        // respawns immediately).
        const FAST_CRASH_WINDOW: Duration = Duration::from_secs(10);
        const MAX_FAST_CRASHES: u32 = 5;
        const MAX_RESPAWN_BACKOFF_SECS: u64 = 10;
        let mut fast_crashes: u32 = 0;

        loop {
            let spawned_at = Instant::now();
            match run_claude_session(&cli, &mut cmd_rx, &reattach, &evt_tx, &mut pending).await {
                // Clean shutdown, or all command senders gone (the
                // connection loop exited = process teardown).
                SessionOutcome::Shutdown | SessionOutcome::ChannelClosed => {
                    return ExitCode::SUCCESS;
                }
                // claude died unexpectedly (crash / interrupt / per-turn
                // timeout) or couldn't be spawned. Any in-flight run's
                // terminal event was already emitted by the session.
                // Respawn with `--resume`, backing off on fast crashes.
                SessionOutcome::Respawn | SessionOutcome::SpawnFailed => {
                    if spawned_at.elapsed() < FAST_CRASH_WINDOW {
                        fast_crashes += 1;
                        if fast_crashes > MAX_FAST_CRASHES {
                            tracing::error!(
                                fast_crashes,
                                "claude keeps dying on startup; giving up — agentd will decide"
                            );
                            return ExitCode::from(1);
                        }
                        let backoff =
                            std::cmp::min(MAX_RESPAWN_BACKOFF_SECS, 1u64 << fast_crashes.min(4));
                        tracing::warn!(
                            fast_crashes,
                            backoff_secs = backoff,
                            "fast claude death; backing off before respawn"
                        );
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                    } else {
                        fast_crashes = 0;
                    }
                    // loop → respawn with --resume
                }
            }
        }
    }

    /// Drive one host connection: handshake, then splice the transport
    /// to the long-lived engine until the link drops. The two halves
    /// run concurrently and either ending ends the connection. Both are
    /// cancellation-safe: `forward_commands` only loses an in-flight
    /// command on a dying link (the host retries), and `pump_events`
    /// parks its un-acked event in `held` *before* awaiting the write.
    async fn run_one_connection(
        stream: (BoxedReader, BoxedWriter),
        cli: &Cli,
        cmd_tx: &mpsc::Sender<HarnessCommand>,
        evt_rx: &mut mpsc::Receiver<HarnessEvent>,
        held: &HeldEvent,
        reattach: &Arc<Notify>,
    ) -> ConnOutcome {
        let (mut reader, mut writer) = stream;

        // Handshake: announce who we are, await the host's ack.
        if let Err(e) = write_msg(
            &mut writer,
            &HarnessAttach {
                session_id: cli.session_id,
                harness_version: format!("engram-harness-claude/{}", env!("CARGO_PKG_VERSION")),
            },
        )
        .await
        {
            tracing::error!(error = %e, "attach write failed");
            return ConnOutcome::HandshakeFailed {
                reason: "attach_write",
            };
        }
        match read_msg::<_, HarnessAttachAck>(&mut reader).await {
            Ok(ack) if ack.ok => {}
            Ok(ack) => {
                tracing::error!(message = ?ack.message, "host rejected attach");
                return ConnOutcome::Rejected;
            }
            Err(e) => {
                tracing::error!(error = %e, "attach ack read failed");
                return ConnOutcome::HandshakeFailed { reason: "ack_read" };
            }
        }

        // Live. Tell the engine a connection attached so it re-announces
        // `Idle` if (and only if) it's currently idle.
        reattach.notify_one();

        // Splice: forward host→guest commands and pump guest→host
        // events concurrently. Whichever side dies first ends the
        // connection; the engine keeps running regardless.
        tokio::select! {
            reason = forward_commands(&mut reader, cmd_tx) => ConnOutcome::Dropped { reason },
            outcome = pump_events(&mut writer, evt_rx, held) => outcome,
        }
    }

    /// Read host→guest frames and shovel commands onto the engine's
    /// long-lived command channel. Returns the drop reason when the
    /// read side dies. A `send` failure means the engine is gone, which
    /// the pump reports as `EngineDone`; here we just stop reading.
    async fn forward_commands<R>(
        reader: &mut R,
        cmd_tx: &mpsc::Sender<HarnessCommand>,
    ) -> &'static str
    where
        R: AsyncRead + Unpin,
    {
        loop {
            match read_msg::<_, HarnessFrame>(reader).await {
                // Track A: a re-handshake is handled entirely at the
                // connection layer — drop the link so the outer loop
                // re-dials and the re-attach re-emits `Idle`. It is never
                // forwarded to the engine (the running agent is untouched).
                // This is the in-band twin of the SIGUSR1 reconnect nudge.
                Ok(HarnessFrame::Command(HarnessCommand::Rehandshake)) => {
                    tracing::info!("rehandshake command; dropping the connection and re-dialing");
                    return "rehandshake";
                }
                Ok(HarnessFrame::Command(c)) => {
                    if cmd_tx.send(c).await.is_err() {
                        return "engine_gone";
                    }
                }
                Ok(HarnessFrame::Event(_)) => {} // hosts don't send events
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return "eof",
                Err(_) => return "read_error",
            }
        }
    }

    /// Drain engine events to the current connection's writer. The
    /// event being written is parked in `held` *before* the await, so
    /// if this future is cancelled (the read half died) or the write
    /// fails, the event survives and is re-sent on the next connection
    /// — at-least-once delivery across a reconnect, no loss.
    async fn pump_events<W>(
        writer: &mut W,
        evt_rx: &mut mpsc::Receiver<HarnessEvent>,
        held: &HeldEvent,
    ) -> ConnOutcome
    where
        W: AsyncWrite + Unpin,
    {
        loop {
            // Prefer an event parked by a prior dropped connection.
            let parked = held.lock().expect("held poisoned").clone();
            let ev = match parked {
                Some(e) => e,
                None => match evt_rx.recv().await {
                    // Park BEFORE the write. The lock is synchronous, so
                    // there is no await between pulling and parking — a
                    // cancellation here can't drop the event.
                    Some(e) => {
                        *held.lock().expect("held poisoned") = Some(e.clone());
                        e
                    }
                    None => return ConnOutcome::EngineDone,
                },
            };
            match write_msg(writer, &HarnessFrame::Event(ev)).await {
                Ok(()) => {
                    *held.lock().expect("held poisoned") = None;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "event write failed; parked for reconnect");
                    return ConnOutcome::Dropped {
                        reason: "write_error",
                    };
                }
            }
        }
    }

    /// Outcome of one persistent `claude` process's life, as seen by the
    /// outer `run_engine` loop. The terminal `HarnessEvent` for any
    /// in-flight run is emitted by `run_claude_session` itself (so it's
    /// always paired with its `RunStarted`); this enum only tells the
    /// engine whether to exit or respawn.
    enum SessionOutcome {
        /// A `Shutdown` command drained claude cleanly (stdin closed,
        /// final `result` flushed, process exited). Exit the engine.
        Shutdown,
        /// All command senders dropped (connection loop exited = process
        /// teardown). Exit the engine.
        ChannelClosed,
        /// claude died unexpectedly (crash, operator interrupt, or a
        /// per-turn timeout). Respawn with `--resume`.
        Respawn,
        /// claude could not be spawned at all. Back off and retry.
        SpawnFailed,
    }

    /// The single in-flight turn, owned by the session loop. `run_id` is
    /// minted on prompt-accept — BEFORE any claude output — so no event
    /// can be emitted without a valid `run_id`. The bare-event desync
    /// that wedged `bf3dbbcb` is therefore impossible by construction.
    struct TurnState {
        run_id: String,
        tool_calls: u32,
        /// Wall-clock cap for THIS turn (`max_run_secs`); on expiry we
        /// kill the process (heavy backstop, default 24h) and respawn.
        deadline: Instant,
    }

    /// A prompt waiting in the harness-owned queue (Phase 1b — type-ahead
    /// / steering). The harness is the single-writer owner: it buffers
    /// these in memory and writes one to claude's stdin only at the
    /// consumption boundary (the running turn's `result`). `prompt_id` is
    /// `Some` for prompts delivered via `HarnessCommand::Prompt` (carrying
    /// a client/coord-minted id used to correlate the queue events and the
    /// eventual `RunStarted{prompt_id}`); `None` only for the env-seeded
    /// initial prompt, which is consumed immediately and never actually
    /// waits in the queue.
    struct QueuedPrompt {
        prompt_id: Option<String>,
        text: String,
    }

    /// SIGINT a running `claude` child — the graceful "stop the current
    /// turn" signal (mirrors a Ctrl-C / ESC). Claude flushes its
    /// conversation file per message synchronously, so the session stays
    /// cleanly `--resume`-able after this. We escalate to SIGKILL only as
    /// a grace-timeout fallback in the reap below.
    fn sigint_child(child: &tokio::process::Child) {
        if let Some(pid) = child.id() {
            if let Err(e) = kill(Pid::from_raw(pid as i32), Signal::SIGINT) {
                tracing::warn!(pid, error = %e, "SIGINT to claude child failed");
            }
        }
    }

    /// Run ONE persistent `claude` process: spawn it, hold its stdin
    /// open, drive turns from the host's `Prompt` commands, and stream
    /// every JSONL line back as `HarnessEvent`s. Returns when the process
    /// exits (cleanly via `Shutdown`, or unexpectedly = crash/interrupt)
    /// or the command channel closes. The outer `run_engine` loop maps
    /// the returned `SessionOutcome` to exit-vs-respawn.
    ///
    /// Turn boundaries are read off claude's explicit `result` line — NOT
    /// process EOF — so a clean turn-end and a crash are unambiguous. The
    /// in-flight turn's terminal event is always emitted here (paired
    /// with the `RunStarted` we synthesized on prompt-accept), even when
    /// the process dies mid-turn.
    async fn run_claude_session(
        cli: &Cli,
        cmd_rx: &mut mpsc::Receiver<HarnessCommand>,
        reattach: &Arc<Notify>,
        evt_tx: &mpsc::Sender<HarnessEvent>,
        pending: &mut VecDeque<QueuedPrompt>,
    ) -> SessionOutcome {
        let resume_id = read_claude_session_id().await;
        let argv = build_claude_argv(&resume_id);
        tracing::info!(
            ?argv,
            resume = resume_id.is_some(),
            "spawning persistent claude"
        );

        let claude_bin: &str = cli
            .claude_bin
            .as_deref()
            .expect("claude_bin resolved at entry()");
        let mut child = match Command::new(claude_bin)
            .args(&argv)
            // Long-run knobs for unattended Claude inside a VM.
            // Bash defaults (2min default / 10min cap) silently
            // kill long builds and tests; bump to 30min/2h. The
            // nonessential-traffic toggle disables the autoupdater,
            // bug-command, and telemetry background calls — the VM
            // typically has no outbound to those endpoints anyway,
            // and they cause spurious failures on long runs.
            // `IS_SANDBOX=1` is the documented escape hatch for
            // Claude's root-check: with --dangerously-skip-permissions
            // the CLI otherwise refuses to start as root, which is
            // exactly how it runs inside our VM. The VM itself is
            // the security boundary.
            .env("BASH_DEFAULT_TIMEOUT_MS", "1800000")
            .env("BASH_MAX_TIMEOUT_MS", "7200000")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("IS_SANDBOX", "1")
            // Held open for the whole session: we write one newline-
            // delimited `user` message per prompt and close it (drop) to
            // signal a clean drain on Shutdown.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Pipe stderr (was `inherit`) so we can retain a tail for the
            // abnormal-exit diagnostic below. The historical wedge risk —
            // a chatty child blocking on write(2) once the 64KiB pipe
            // buffer fills because nobody reads it — is avoided by the
            // dedicated drain task that ALWAYS reads the pipe. That task
            // re-echoes each line to our own stderr, which agentd still
            // points at the in-guest harness log
            // (`/var/log/engram/harness.log`), so the prior `/exec`-
            // visible behavior is preserved.
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                // No run is in flight, so we deliberately do NOT emit a
                // bare AgentMessage (that would be exactly the run_id-less
                // desync this rewrite eliminates). The outer loop backs
                // off and retries; a hard failure trips the fast-crash
                // budget and exits the engine.
                tracing::error!(error = %e, bin = %claude_bin, "spawn claude failed");
                return SessionOutcome::SpawnFailed;
            }
        };

        // `stdin` is an Option so Shutdown can DROP it (closing the FD =
        // EOF for claude) while everything else keeps borrowing the child.
        let mut stdin: Option<tokio::process::ChildStdin> =
            Some(child.stdin.take().expect("piped stdin"));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();

        // Drain claude's stderr into a bounded rolling tail (see the
        // spawn comment). On an abnormal exit we attach this tail to a
        // System message so the failure cause survives VM teardown.
        let stderr = child.stderr.take().expect("piped stderr");
        let stderr_task: tokio::task::JoinHandle<Vec<String>> = tokio::spawn(async move {
            let mut tail: VecDeque<String> = VecDeque::with_capacity(MAX_STDERR_TAIL_LINES + 1);
            let mut elines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = elines.next_line().await {
                eprintln!("{line}");
                if tail.len() >= MAX_STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            tail.into_iter().collect()
        });

        let mut turn: Option<TurnState> = None;
        let mut shutting_down = false;
        let mut shutdown_deadline: Option<Instant> = None;
        // Set when an operator Interrupt SIGINT'd the child mid-turn; the
        // reap path closes that run as `RunInterrupted` (not a crash).
        let mut interrupted_run: Option<String> = None;

        // Kick off the first queued prompt (an initial prompt, or a queue
        // that survived a respawn) with no leading Idle; otherwise
        // announce Idle so the host's soft TTL arms.
        match pending.pop_front() {
            Some(qp) => {
                if let Some(s) = stdin.as_mut() {
                    turn = Some(start_turn(evt_tx, s, cli, qp.prompt_id, &qp.text).await);
                }
            }
            None => emit(evt_tx, HarnessEvent::Idle).await,
        }

        loop {
            // The relevant deadline this iteration: the in-flight turn's
            // wall-clock cap and/or the shutdown grace window, whichever
            // is sooner. `None` → park forever (idle, no shutdown).
            let next_deadline = [turn.as_ref().map(|t| t.deadline), shutdown_deadline]
                .into_iter()
                .flatten()
                .min();
            let sleeper = async {
                match next_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(sleeper);

            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            if let Some(marker) = detect_result_marker(&line) {
                                // Explicit turn-end. Close the in-flight
                                // run, then run the next queued prompt
                                // back-to-back (no Idle gap) or announce Idle.
                                if let Some(t) = turn.take() {
                                    let run_id = t.run_id;
                                    tracing::info!(
                                        %run_id,
                                        subtype = %marker.subtype,
                                        is_error = marker.is_error,
                                        "turn result"
                                    );
                                    if interrupted_run.as_deref() == Some(run_id.as_str()) {
                                        // control_request-style abort that
                                        // surfaced as a result (Phase 3);
                                        // the process is still alive.
                                        interrupted_run = None;
                                        emit(evt_tx, HarnessEvent::RunInterrupted { run_id }).await;
                                    } else {
                                        emit(
                                            evt_tx,
                                            HarnessEvent::RunCompleted {
                                                run_id,
                                                ok: !marker.is_error,
                                            },
                                        )
                                        .await;
                                    }
                                    match pending.pop_front() {
                                        Some(qp) => {
                                            if let Some(s) = stdin.as_mut() {
                                                turn = Some(
                                                    start_turn(
                                                        evt_tx, s, cli, qp.prompt_id, &qp.text,
                                                    )
                                                    .await,
                                                );
                                            }
                                        }
                                        None => emit(evt_tx, HarnessEvent::Idle).await,
                                    }
                                } else {
                                    tracing::warn!("result line with no in-flight turn; ignoring");
                                }
                            } else if let Some(t) = turn.as_mut() {
                                if let Some(translated) = translate_jsonl(
                                    &line,
                                    &t.run_id,
                                    &mut t.tool_calls,
                                    cli.max_tool_calls,
                                ) {
                                    for ev in translated {
                                        emit(evt_tx, ev).await;
                                    }
                                }
                            } else {
                                // A line outside any turn (e.g. claude's
                                // init banner before the first prompt):
                                // parse only for the session-id capture
                                // side effect; emit nothing.
                                let mut sink = 0u32;
                                let _ = translate_jsonl(&line, "", &mut sink, cli.max_tool_calls);
                            }
                        }
                        Ok(None) => break, // stdout EOF: claude is exiting
                        Err(e) => {
                            tracing::warn!(error = %e, "claude stdout read error");
                            break;
                        }
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(HarnessCommand::Prompt { prompt_id, text }) => {
                            if turn.is_none() {
                                // Idle: start the run immediately.
                                if let Some(s) = stdin.as_mut() {
                                    turn = Some(
                                        start_turn(evt_tx, s, cli, Some(prompt_id), &text).await,
                                    );
                                }
                            } else {
                                // Type-ahead: a prompt arriving mid-turn is
                                // QUEUED (not written to claude yet, so it
                                // stays editable). The harness owns the
                                // queue; `PromptQueued` reflects it up so the
                                // web renders a greyed/editable composer item.
                                emit(
                                    evt_tx,
                                    HarnessEvent::PromptQueued {
                                        prompt_id: prompt_id.clone(),
                                        summary: Some(truncate_str(&text, MAX_ARGS_SUMMARY_BYTES)),
                                    },
                                )
                                .await;
                                pending.push_back(QueuedPrompt {
                                    prompt_id: Some(prompt_id),
                                    text,
                                });
                            }
                        }
                        Some(HarnessCommand::EditQueued { prompt_id, text }) => {
                            // Single-writer: mutate only while still queued
                            // (not yet consumed). A consumed prompt already
                            // emitted `RunStarted{prompt_id}`, which won.
                            let summary = Some(truncate_str(&text, MAX_ARGS_SUMMARY_BYTES));
                            let found = match pending
                                .iter_mut()
                                .find(|q| q.prompt_id.as_deref() == Some(prompt_id.as_str()))
                            {
                                Some(qp) => {
                                    qp.text = text;
                                    true
                                }
                                None => false,
                            };
                            if found {
                                emit(
                                    evt_tx,
                                    HarnessEvent::PromptEdited { prompt_id, summary },
                                )
                                .await;
                            } else {
                                tracing::debug!(%prompt_id, "edit for a non-queued prompt; ignoring");
                            }
                        }
                        Some(HarnessCommand::DequeueQueued { prompt_id }) => {
                            let before = pending.len();
                            pending
                                .retain(|q| q.prompt_id.as_deref() != Some(prompt_id.as_str()));
                            if pending.len() != before {
                                emit(evt_tx, HarnessEvent::PromptDequeued { prompt_id }).await;
                            } else {
                                tracing::debug!(%prompt_id, "dequeue for a non-queued prompt; ignoring");
                            }
                        }
                        Some(HarnessCommand::Interrupt) => {
                            if let Some(t) = turn.as_ref() {
                                // Phase 1: SIGINT the persistent child to
                                // abort the turn. claude exits; the reap
                                // path emits RunInterrupted and the engine
                                // respawns `--resume`. Phase 3 upgrades
                                // this to an in-band control_request that
                                // leaves the process alive (no respawn).
                                tracing::info!(run_id = %t.run_id, "interrupt mid-turn; SIGINT-ing claude");
                                interrupted_run = Some(t.run_id.clone());
                                sigint_child(&child);
                            } else {
                                tracing::debug!("interrupt while idle; nothing to stop");
                            }
                        }
                        Some(HarnessCommand::Shutdown { grace_secs }) => {
                            // Clean shutdown: close claude's stdin so it
                            // drains the in-flight turn, emits its final
                            // `result`, and exits 0. Bound by grace_secs;
                            // on expiry the deadline arm kills it.
                            tracing::info!(grace_secs, "shutdown; closing claude stdin to drain");
                            shutting_down = true;
                            shutdown_deadline =
                                Some(Instant::now() + Duration::from_secs(grace_secs as u64));
                            stdin = None;
                        }
                        Some(HarnessCommand::Checkpoint { .. }) => {
                            // claude flushes its transcript per-message
                            // synchronously; nothing to force here.
                        }
                        // Track A (ADR 0034): a re-handshake is intercepted
                        // at the connection layer (`forward_commands` drops
                        // the link to force a re-dial) and never forwarded
                        // to the engine. This arm exists only for
                        // exhaustiveness over `HarnessCommand`.
                        Some(HarnessCommand::Rehandshake) => {}
                        // All command senders gone = the connection loop
                        // exited = process teardown.
                        None => return SessionOutcome::ChannelClosed,
                    }
                }
                // A fresh connection attached: re-announce `Idle` iff idle
                // so the host re-arms its soft TTL. A mid-turn reattach
                // emits none.
                _ = reattach.notified() => {
                    if turn.is_none() {
                        emit(evt_tx, HarnessEvent::Idle).await;
                    }
                }
                _ = &mut sleeper => {
                    if shutting_down {
                        tracing::warn!("shutdown grace elapsed; killing claude");
                    } else {
                        tracing::warn!("max_run_secs elapsed; killing claude");
                    }
                    let _ = child.start_kill();
                    break;
                }
            }
        }

        // Bounded reap. Capture the ExitStatus (the signal/code is the
        // crash signal — SIGKILL ≈ OOM-killer, SIGSEGV ≈ crash). All exit
        // paths (clean drain, SIGINT, kill, EOF) should let the child go
        // promptly; if a signalled child lingers, escalate to SIGKILL.
        let exit_status: Option<ExitStatus> =
            match timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => Some(status),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "reaping claude child failed");
                    None
                }
                Err(_) => {
                    tracing::warn!("claude didn't exit within grace window; SIGKILL");
                    let _ = child.start_kill();
                    child.wait().await.ok()
                }
            };

        // Collect the stderr tail (drain task ends when the child closes
        // stderr; the reap above just ensured that). Bound the wait.
        let stderr_tail: Vec<String> = match timeout(Duration::from_secs(2), stderr_task).await {
            Ok(Ok(tail)) => tail,
            _ => Vec::new(),
        };

        // Close out any turn that was still in flight when the process
        // died — always paired with the `RunStarted` we already emitted,
        // so the run never dangles open (the wedge we're eliminating).
        if let Some(t) = turn.take() {
            let run_id = t.run_id;
            if interrupted_run.as_deref() == Some(run_id.as_str()) {
                // Operator interrupt: the SIGINT tore the process down
                // mid-turn. Report it as interrupted (not a crash) and let
                // the engine respawn `--resume` for the next prompt.
                emit(evt_tx, HarnessEvent::RunInterrupted { run_id }).await;
                emit(evt_tx, HarnessEvent::Idle).await;
            } else if shutting_down {
                // Grace expired mid-turn before claude could drain.
                emit(evt_tx, HarnessEvent::RunCompleted { run_id, ok: false }).await;
            } else {
                // Unexpected crash mid-turn. Surface the durable artifact
                // (bracketed by the run's RunStarted), close the run, and
                // go Idle; the engine respawns `--resume`.
                let detail = describe_abnormal_exit(exit_status, &stderr_tail);
                tracing::error!(detail, "claude crashed mid-turn");
                emit(
                    evt_tx,
                    HarnessEvent::AgentMessage {
                        run_id: run_id.clone(),
                        message_id: format!("abnormal-{}", uuid::Uuid::new_v4()),
                        role: AgentRole::System,
                        text: detail,
                    },
                )
                .await;
                emit(evt_tx, HarnessEvent::RunCompleted { run_id, ok: false }).await;
                emit(evt_tx, HarnessEvent::Idle).await;
            }
        } else {
            tracing::info!(
                status = ?exit_status,
                shutting_down,
                "claude process ended (no in-flight turn)"
            );
        }

        if shutting_down {
            SessionOutcome::Shutdown
        } else {
            SessionOutcome::Respawn
        }
    }

    /// Start a turn: mint the `run_id`, emit `RunStarted` (BEFORE any
    /// claude output, so the run owns its id), and write the prompt as a
    /// newline-delimited `user` message to claude's held-open stdin.
    async fn start_turn(
        evt_tx: &mpsc::Sender<HarnessEvent>,
        stdin: &mut tokio::process::ChildStdin,
        cli: &Cli,
        prompt_id: Option<String>,
        text: &str,
    ) -> TurnState {
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        // `RunStarted{prompt_id}` is the "queued prompt consumed" signal:
        // a UI that drew a greyed type-ahead item with this id moves it
        // into the conversation now. `None` for the env-seeded initial
        // prompt (which never went through the editable queue).
        emit(
            evt_tx,
            HarnessEvent::RunStarted {
                run_id: run_id.clone(),
                prompt_id,
                prompt_summary: Some(truncate_str(text, MAX_ARGS_SUMMARY_BYTES)),
            },
        )
        .await;
        if let Err(e) = write_user_message(stdin, text).await {
            // The only way this fails is claude's stdin already gone (it
            // died); the stdout-EOF reap path will close this run.
            tracing::warn!(error = %e, "writing prompt to claude stdin failed");
        }
        TurnState {
            run_id,
            tool_calls: 0,
            deadline: Instant::now() + Duration::from_secs(cli.max_run_secs),
        }
    }

    /// Write one prompt to claude's stdin as a stream-json `user` message.
    /// `serde_json` handles all escaping.
    async fn write_user_message(
        stdin: &mut tokio::process::ChildStdin,
        text: &str,
    ) -> std::io::Result<()> {
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        });
        let mut line = serde_json::to_string(&msg).expect("serialize user message");
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await
    }

    /// Build the argv for the persistent streaming `claude`. `--input-
    /// format stream-json` holds stdin open for newline-delimited `user`
    /// messages (one per turn); `--output-format stream-json --verbose`
    /// gives us the per-turn `result` terminator on stdout. No trailing
    /// prompt arg — prompts are written to stdin via `write_user_message`.
    fn build_claude_argv(resume_id: &Option<String>) -> Vec<String> {
        let mut argv = vec![
            "--print".to_string(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // No human in the VM to approve tool calls; `--print`
            // aborts with exit 1 the first time a tool needs
            // approval otherwise. The VM itself is the safety
            // boundary.
            "--dangerously-skip-permissions".into(),
        ];
        if let Some(id) = resume_id {
            argv.push("--resume".into());
            argv.push(id.clone());
        }
        argv
    }

    async fn read_claude_session_id() -> Option<String> {
        match tokio::fs::read_to_string(CLAUDE_SESSION_ID_FILE).await {
            Ok(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            Err(_) => None,
        }
    }

    async fn write_claude_session_id(id: &str) {
        let _ = tokio::fs::create_dir_all("/workspace/.engram").await;
        if let Err(e) = tokio::fs::write(CLAUDE_SESSION_ID_FILE, id).await {
            tracing::warn!(error = %e, "couldn't persist claude session id");
        }
    }

    /// The terminal `result` line claude emits at the end of every
    /// completed turn. `subtype` is `success` or a handled-error variant
    /// (`error_max_turns`, an API error it surfaced, …); `is_error`
    /// distinguishes the two. We retain only that we saw it (plus those
    /// fields, for the diagnostic) — its presence means the turn ended in
    /// an orderly way, even if unhappily.
    #[derive(Debug, Clone)]
    pub struct ResultMarker {
        pub subtype: String,
        pub is_error: bool,
    }

    /// Detect claude's terminal `result` line. Substring-gated so we
    /// don't double-parse every JSONL line — claude's stream-json is
    /// compact, so `"type":"result"` appears verbatim only on the result
    /// line (a `tool_result` block lives inside a `type":"user"` line and
    /// does not match).
    pub fn detect_result_marker(line: &str) -> Option<ResultMarker> {
        if !line.contains("\"type\":\"result\"") {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        if v.get("type")?.as_str()? != "result" {
            return None;
        }
        Some(ResultMarker {
            subtype: v
                .get("subtype")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            is_error: v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false),
        })
    }

    /// Human-readable hint for the common fatal signals, so an operator
    /// reading session_events doesn't have to remember signal numbers.
    fn signal_hint(sig: i32) -> &'static str {
        match sig {
            9 => " (SIGKILL — typically the in-guest OOM-killer or an external kill)",
            11 => " (SIGSEGV — segfault)",
            6 => " (SIGABRT — abort, e.g. a panic or assertion)",
            15 => " (SIGTERM)",
            _ => "",
        }
    }

    /// Build the System-message body for an abnormal claude exit: the
    /// exit code/signal plus the tail of claude's stderr. This is the
    /// durable crash artifact — it rides the event stream into
    /// `session_events`, surviving the VM teardown that erases the
    /// in-guest harness log. SIGKILL here is the OOM-killer signature.
    pub fn describe_abnormal_exit(status: Option<ExitStatus>, stderr_tail: &[String]) -> String {
        let exit = match status {
            None => "could not be reaped (status unavailable)".to_string(),
            Some(s) => {
                if let Some(code) = s.code() {
                    format!("exited with code {code}")
                } else if let Some(sig) = s.signal() {
                    format!("was killed by signal {sig}{}", signal_hint(sig))
                } else {
                    "exited (status unavailable)".to_string()
                }
            }
        };
        let tail = if stderr_tail.is_empty() {
            "(no stderr captured)".to_string()
        } else {
            truncate_str(&stderr_tail.join("\n"), MAX_STDERR_TAIL_BYTES)
        };
        format!(
            "engram-harness: claude exited abnormally mid-turn — it {exit} without emitting a \
             terminal `result`, so the turn did not complete. This is a harness/agent crash, \
             not an engram platform error. claude stderr tail:\n{tail}"
        )
    }

    /// Translate one JSONL line from Claude's `--output-format
    /// stream-json` into zero or more HarnessEvents, tagging them with
    /// the caller-owned `run_id` (minted on prompt-accept). The
    /// `system`/`init` line — re-emitted per turn in streaming mode — is
    /// NOT turned into a `RunStarted` here (the session loop owns run
    /// lifecycle); it only captures claude's session id for `--resume`.
    /// The terminal `result` line is handled by the session loop via
    /// `detect_result_marker`, so it's a no-op here.
    pub fn translate_jsonl(
        line: &str,
        run_id: &str,
        tool_calls: &mut u32,
        max_tool_calls: u32,
    ) -> Option<Vec<HarnessEvent>> {
        let v: Value = serde_json::from_str(line).ok()?;
        let ty = v.get("type")?.as_str()?;
        let mut out: Vec<HarnessEvent> = Vec::new();
        match ty {
            "system" => {
                let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                if subtype == "init" {
                    // Capture + persist claude's session id so a respawn
                    // can `--resume` the same conversation. Fire-and-forget
                    // the disk write only when a tokio runtime is present
                    // (sync unit tests call this outside one).
                    if let Some(sid) = v
                        .get("session_id")
                        .and_then(|s| s.as_str())
                        .map(str::to_string)
                    {
                        if let Ok(handle) = tokio::runtime::Handle::try_current() {
                            handle.spawn(async move { write_claude_session_id(&sid).await });
                        }
                    }
                }
            }
            "assistant" => {
                let rid = run_id.to_string();
                let msg = v.get("message")?;
                let msg_id = msg
                    .get("id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("msg-?")
                    .to_string();
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    let mut text_buf = String::new();
                    for b in blocks {
                        let bt = b.get("type").and_then(|s| s.as_str()).unwrap_or("");
                        match bt {
                            "text" => {
                                if let Some(t) = b.get("text").and_then(|s| s.as_str()) {
                                    text_buf.push_str(t);
                                }
                            }
                            "tool_use" => {
                                *tool_calls += 1;
                                if *tool_calls > max_tool_calls {
                                    tracing::warn!(
                                        tool_calls = *tool_calls,
                                        max_tool_calls,
                                        "max_tool_calls exceeded"
                                    );
                                }
                                let tcid = b
                                    .get("id")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("toolu-?")
                                    .to_string();
                                let name = b
                                    .get("name")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let args_summary = b
                                    .get("input")
                                    .map(|v| truncate_str(&v.to_string(), MAX_ARGS_SUMMARY_BYTES));
                                out.push(HarnessEvent::ToolCallStarted {
                                    run_id: rid.clone(),
                                    tool_call_id: tcid,
                                    tool_name: name,
                                    args_summary,
                                });
                            }
                            _ => {}
                        }
                    }
                    if !text_buf.is_empty() {
                        out.push(HarnessEvent::AgentMessage {
                            run_id: rid,
                            message_id: msg_id,
                            role: AgentRole::Assistant,
                            text: truncate_str(&text_buf, MAX_AGENT_MESSAGE_BYTES),
                        });
                    }
                }
            }
            "user" => {
                let rid = run_id.to_string();
                let msg = v.get("message")?;
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for b in blocks {
                        let bt = b.get("type").and_then(|s| s.as_str()).unwrap_or("");
                        if bt == "tool_result" {
                            let tcid = b
                                .get("tool_use_id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("toolu-?")
                                .to_string();
                            let is_error =
                                b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                            let result_text = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(arr)) => arr
                                    .iter()
                                    .filter_map(|c| {
                                        c.get("text").and_then(|t| t.as_str()).map(str::to_string)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                _ => String::new(),
                            };
                            out.push(HarnessEvent::ToolCallCompleted {
                                run_id: rid.clone(),
                                tool_call_id: tcid,
                                tool_name: String::new(),
                                ok: !is_error,
                                duration_ms: 0,
                                result_summary: Some(truncate_str(
                                    &result_text,
                                    MAX_RESULT_SUMMARY_BYTES,
                                )),
                            });
                        }
                    }
                }
            }
            "result" => {
                // The session loop closes the turn on this line via
                // `detect_result_marker`; nothing to translate here.
            }
            _ => {}
        }
        Some(out)
    }

    pub fn truncate_str(s: &str, max_bytes: usize) -> String {
        if s.len() <= max_bytes {
            return s.to_string();
        }
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut truncated = s[..end].to_string();
        truncated.push_str("…[truncated]");
        truncated
    }

    #[cfg(test)]
    mod engine_tests {
        use super::*;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        /// A writer whose every write fails — stands in for a host
        /// connection that dropped underneath the pump.
        struct FailingWriter;
        impl tokio::io::AsyncWrite for FailingWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "connection dropped",
                )))
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        // A connection drop mid-write must PARK the event, not lose it.
        #[tokio::test]
        async fn pump_parks_event_when_write_fails() {
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(4);
            let held: HeldEvent = Arc::new(Mutex::new(None));
            evt_tx.send(HarnessEvent::Idle).await.unwrap();

            let mut w = FailingWriter;
            let outcome = pump_events(&mut w, &mut evt_rx, &held).await;

            assert!(matches!(outcome, ConnOutcome::Dropped { .. }));
            assert_eq!(
                *held.lock().unwrap(),
                Some(HarnessEvent::Idle),
                "the un-acked event must be parked for the next connection",
            );
        }

        // The next connection re-sends the parked event first (at-least-once).
        #[tokio::test]
        async fn pump_resends_parked_event() {
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(4);
            // Pre-populate `held` as if a prior connection dropped mid-write,
            // and close the event channel so the pump finishes after the
            // re-send.
            let held: HeldEvent = Arc::new(Mutex::new(Some(HarnessEvent::Idle)));
            drop(evt_tx);

            let (mut w, mut r) = tokio::io::duplex(4096);
            let outcome = pump_events(&mut w, &mut evt_rx, &held).await;

            assert!(matches!(outcome, ConnOutcome::EngineDone));
            assert_eq!(
                *held.lock().unwrap(),
                None,
                "delivered event must be cleared"
            );
            let frame: HarnessFrame = read_msg(&mut r).await.unwrap();
            assert!(
                matches!(frame, HarnessFrame::Event(HarnessEvent::Idle)),
                "parked event must be the first thing the new connection sees",
            );
        }

        /// A persistent fake `claude`: emits an `init` banner once, then
        /// blocks reading newline-delimited stdin (our `user` messages),
        /// emitting `per_turn` lines for each one (one turn per prompt).
        /// Exits when stdin closes — mirroring claude draining on a clean
        /// Shutdown (stdin drop = EOF).
        async fn write_persistent_fake_claude(per_turn: &[&str]) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let mut body = String::from("#!/bin/sh\n");
            // init has no session_id, so the engine's session-id persist
            // (which would touch /workspace) stays a no-op in the test.
            body.push_str("printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\"}'\n");
            body.push_str("while IFS= read -r _line; do\n");
            for l in per_turn {
                body.push_str(&format!("  printf '%s\\n' '{l}'\n"));
            }
            body.push_str("done\n");
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        async fn expect_run_started(rx: &mut mpsc::Receiver<HarnessEvent>) -> String {
            match rx.recv().await {
                Some(HarnessEvent::RunStarted { run_id, .. }) => run_id,
                other => panic!("expected RunStarted, got {other:?}"),
            }
        }

        async fn expect_run_completed(rx: &mut mpsc::Receiver<HarnessEvent>) -> String {
            match rx.recv().await {
                Some(HarnessEvent::RunCompleted { run_id, ok }) => {
                    assert!(ok, "expected a clean RunCompleted");
                    run_id
                }
                other => panic!("expected RunCompleted, got {other:?}"),
            }
        }

        async fn expect_agent_message(rx: &mut mpsc::Receiver<HarnessEvent>, want: &str) {
            match rx.recv().await {
                Some(HarnessEvent::AgentMessage { text, .. }) => assert_eq!(text, want),
                other => panic!("expected AgentMessage, got {other:?}"),
            }
        }

        fn test_cli(claude_bin: String) -> Cli {
            Cli {
                connect: None,
                vsock_host: Some(1),
                session_id: SessionId::new(),
                max_tool_calls: 100_000,
                max_tool_call_secs: 600,
                max_run_secs: 86_400,
                claude_bin: Some(claude_bin),
            }
        }

        /// Like `write_persistent_fake_claude`, but sleeps `sleep_ms`
        /// before emitting each turn's lines — so a turn stays IN FLIGHT
        /// long enough for the test to inject type-ahead/queue commands
        /// before the `result` arrives.
        async fn write_slow_fake_claude(per_turn: &[&str], sleep_ms: u64) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let secs = format!("{}.{:03}", sleep_ms / 1000, sleep_ms % 1000);
            let mut body = String::from("#!/bin/sh\n");
            body.push_str("printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\"}'\n");
            body.push_str("while IFS= read -r _line; do\n");
            body.push_str(&format!("  sleep {secs}\n"));
            for l in per_turn {
                body.push_str(&format!("  printf '%s\\n' '{l}'\n"));
            }
            body.push_str("done\n");
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        async fn expect_run_started_id(
            rx: &mut mpsc::Receiver<HarnessEvent>,
        ) -> (String, Option<String>) {
            match rx.recv().await {
                Some(HarnessEvent::RunStarted {
                    run_id, prompt_id, ..
                }) => (run_id, prompt_id),
                other => panic!("expected RunStarted, got {other:?}"),
            }
        }

        // Phase 1b: a prompt arriving mid-turn is QUEUED (PromptQueued),
        // stays editable (PromptEdited) / cancellable (PromptDequeued)
        // until the turn's `result` consumes the next queued prompt —
        // whose `RunStarted` carries that prompt_id. A Dequeue after
        // consumption is a no-op (single-writer).
        #[tokio::test]
        async fn queue_holds_edits_and_consumes_type_ahead() {
            // Each turn: sleep, then one assistant line + result.
            let script = write_slow_fake_claude(
                &[
                    r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"ok"}]}}"#,
                    r#"{"type":"result","subtype":"success","is_error":false}"#,
                ],
                300,
            )
            .await;

            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(16);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(
                test_cli(script.clone()),
                cmd_rx,
                reattach.clone(),
                evt_tx,
                Some("first".into()),
            ));

            // Turn 1 (env-seeded initial prompt → no prompt_id). It is now
            // in flight (the fake sleeps 300ms before its result).
            let (r1, pid1) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(pid1, None, "initial prompt has no prompt_id");

            // Queue p2, edit it, queue p3, then cancel p3 — all while turn
            // 1 is still sleeping. None of these are written to claude yet.
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p2".into(),
                    text: "second".into(),
                })
                .await
                .unwrap();
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::PromptQueued { prompt_id, .. }) if prompt_id == "p2"
            ));
            cmd_tx
                .send(HarnessCommand::EditQueued {
                    prompt_id: "p2".into(),
                    text: "second-edited".into(),
                })
                .await
                .unwrap();
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::PromptEdited { prompt_id, .. }) if prompt_id == "p2"
            ));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p3".into(),
                    text: "third".into(),
                })
                .await
                .unwrap();
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::PromptQueued { prompt_id, .. }) if prompt_id == "p3"
            ));
            cmd_tx
                .send(HarnessCommand::DequeueQueued {
                    prompt_id: "p3".into(),
                })
                .await
                .unwrap();
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::PromptDequeued { prompt_id }) if prompt_id == "p3"
            ));

            // Turn 1 completes; p2 (edited) is consumed back-to-back as a
            // new run carrying prompt_id "p2".
            expect_agent_message(&mut evt_rx, "ok").await;
            let c1 = expect_run_completed(&mut evt_rx).await;
            assert_eq!(r1, c1);
            let (r2, pid2) = expect_run_started_id(&mut evt_rx).await;
            assert_ne!(r1, r2);
            assert_eq!(pid2.as_deref(), Some("p2"), "consumed queued prompt id");
            expect_agent_message(&mut evt_rx, "ok").await;
            let c2 = expect_run_completed(&mut evt_rx).await;
            assert_eq!(r2, c2);
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // A dequeue for an already-consumed prompt is a silent no-op,
            // then shutdown. The next (and only) thing on the event channel
            // is its CLOSE as the engine exits — NOT a spurious
            // PromptDequeued for the consumed p2.
            cmd_tx
                .send(HarnessCommand::DequeueQueued {
                    prompt_id: "p2".into(),
                })
                .await
                .unwrap();
            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 5 })
                .await
                .unwrap();
            match evt_rx.recv().await {
                None => {} // channel closed = engine exited cleanly, no stray event
                Some(ev) => panic!("unexpected event after a consumed-prompt dequeue: {ev:?}"),
            }
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine should exit on shutdown")
                .expect("engine task should not panic");
            let _ = tokio::fs::remove_file(&script).await;
        }

        // The core Phase-1 contract: TWO prompts over ONE persistent
        // process, each turn cleanly bracketed by RunStarted→…→
        // RunCompleted→Idle with a DISTINCT run_id (no inference from
        // EOF). A reattach while idle re-announces Idle; Shutdown drains
        // the process and the engine exits.
        #[tokio::test]
        async fn engine_runs_two_prompts_over_one_process() {
            let script = write_persistent_fake_claude(&[
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hello"}]}}"#,
                r#"{"type":"result","subtype":"success","is_error":false}"#,
            ])
            .await;

            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(8);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(
                test_cli(script.clone()),
                cmd_rx,
                reattach.clone(),
                evt_tx,
                Some("first".into()),
            ));

            // Turn 1 (the initial prompt): RunStarted, AgentMessage,
            // RunCompleted, Idle — the run_id shared start-to-end.
            let r1 = expect_run_started(&mut evt_rx).await;
            expect_agent_message(&mut evt_rx, "hello").await;
            let c1 = expect_run_completed(&mut evt_rx).await;
            assert_eq!(r1, c1, "RunStarted and RunCompleted share the run_id");
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // Turn 2 over the SAME process — a fresh, distinct run_id.
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p-second".into(),
                    text: "second".into(),
                })
                .await
                .unwrap();
            let r2 = expect_run_started(&mut evt_rx).await;
            assert_ne!(r1, r2, "each turn gets a fresh run_id");
            expect_agent_message(&mut evt_rx, "hello").await;
            let c2 = expect_run_completed(&mut evt_rx).await;
            assert_eq!(r2, c2);
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // A reconnect while idle re-announces Idle so the host's soft
            // TTL re-arms; a mid-run reattach emits none.
            reattach.notify_one();
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // Shutdown closes claude's stdin; the fake drains and exits,
            // and the engine returns.
            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 5 })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine should exit on shutdown")
                .expect("engine task should not panic");
            let _ = tokio::fs::remove_file(&script).await;
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    adapter::entry().await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::adapter::*;
    use engram_harness_proto::HarnessEvent;

    #[test]
    fn system_init_emits_nothing_and_assistant_text_carries_run_id() {
        // `system`/`init` no longer synthesizes RunStarted (the session
        // loop owns run lifecycle) — it yields no events. Assistant text
        // is tagged with the caller-provided run_id.
        let init = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        let asst = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"hi there"}]}}"#;
        let mut tc = 0u32;
        let evs = translate_jsonl(init, "run-1", &mut tc, 50).unwrap();
        assert!(evs.is_empty(), "init emits no HarnessEvent");

        let evs = translate_jsonl(asst, "run-1", &mut tc, 50).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            HarnessEvent::AgentMessage { text, run_id, .. } => {
                assert_eq!(text, "hi there");
                assert_eq!(run_id, "run-1");
            }
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn translates_tool_use_and_tool_result_pair() {
        let asst = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls /workspace"}}]}}"#;
        let user = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"file1.txt\nfile2.py","is_error":false}]}}"#;

        let mut tc = 0u32;
        let evs = translate_jsonl(asst, "run-1", &mut tc, 50).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], HarnessEvent::ToolCallStarted { .. }));
        assert_eq!(tc, 1);

        let evs = translate_jsonl(user, "run-1", &mut tc, 50).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            HarnessEvent::ToolCallCompleted {
                tool_call_id,
                ok,
                result_summary,
                ..
            } => {
                assert_eq!(tool_call_id, "toolu_1");
                assert!(ok);
                assert_eq!(result_summary.as_deref(), Some("file1.txt\nfile2.py"));
            }
            other => panic!("expected ToolCallCompleted, got {other:?}"),
        }
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        let s = "🦀".repeat(100);
        let t = truncate_str(&s, 10);
        assert!(t.ends_with("…[truncated]"));
    }

    #[test]
    fn detects_terminal_result_line() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        let m = detect_result_marker(line).expect("should detect terminal result");
        assert_eq!(m.subtype, "success");
        assert!(!m.is_error);

        // A handled error (e.g. the e2e 401) still emits a result line —
        // it must be detected so we DON'T flag it as an abnormal exit.
        let err = r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#;
        let m = detect_result_marker(err).expect("error result still detected");
        assert!(m.is_error);
    }

    #[test]
    fn non_result_lines_are_not_markers() {
        // assistant text, and a tool_result nested in a `user` line, must
        // NOT be mistaken for the terminal result marker.
        let asst =
            r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"hi"}]}}"#;
        let tool = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"out","is_error":false}]}}"#;
        assert!(detect_result_marker(asst).is_none());
        assert!(detect_result_marker(tool).is_none());
    }

    #[test]
    fn abnormal_exit_detail_flags_oom_signal() {
        use std::os::unix::process::ExitStatusExt;
        // raw wait-status `9` == terminated by signal 9 (SIGKILL).
        let killed = std::process::ExitStatus::from_raw(9);
        let detail =
            describe_abnormal_exit(Some(killed), &["fatal: JS heap out of memory".to_string()]);
        assert!(detail.contains("signal 9"), "got: {detail}");
        assert!(
            detail.contains("OOM"),
            "should hint OOM for SIGKILL: {detail}"
        );
        assert!(detail.contains("out of memory"), "should carry stderr tail");
        assert!(detail.contains("did not complete"));
    }

    #[test]
    fn abnormal_exit_detail_reports_nonzero_code() {
        use std::os::unix::process::ExitStatusExt;
        // raw status `1 << 8` == exited with code 1.
        let exited = std::process::ExitStatus::from_raw(1 << 8);
        let detail = describe_abnormal_exit(Some(exited), &[]);
        assert!(detail.contains("code 1"), "got: {detail}");
        assert!(detail.contains("no stderr captured"));
    }
}
