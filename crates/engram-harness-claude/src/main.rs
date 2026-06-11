//! Harness adapter for the `claude` CLI.
//!
//! Lives at `/sbin/engram-harness-claude` inside the rootfs (or
//! cargo-target/.../engram-harness-claude on the dev-mode
//! ProcessBackend path). Exec'd by `engram-agentd`'s harness
//! supervisor after the host pushes a `SpawnHarness` request
//! describing this binary.
//!
//! Strategy: child-per-prompt. Per `Prompt` command, spawn `claude
//! --print --output-format stream-json --dangerously-skip-permissions
//! [--resume <claude_id>] "<text>"`, parse stdout JSONL line-by-line,
//! translate to `HarnessEvent`s, child exits, await next prompt.
//!
//! `--dangerously-skip-permissions` is mandatory: there's no human in
//! the VM to answer permission prompts, and `--print` mode aborts
//! with exit 1 the first time a tool needs approval otherwise. The
//! sandbox is the safety boundary, not Claude's per-tool consent.
//!
//! The first run captures Claude's auto-generated session id and
//! stashes it in `/workspace/.engram/claude-session-id` so
//! follow-ups can `--resume` into the same conversation. Lost on
//! cold death (Dead status); a forked session gets a fresh
//! Claude conversation.

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
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
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

    /// The long-lived run engine. Owns the `claude` child and the
    /// idle/running state; spawned once and never torn down by a
    /// connection drop. Reads host commands off the long-lived
    /// `cmd_rx`, emits events into `evt_tx`, and re-announces `Idle`
    /// on `reattach` only while idle.
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
        let mut pending_prompt = initial_prompt;
        loop {
            // Resolve the next prompt to run: a queued/initial one, or
            // wait (idle) for the host to send one.
            let text = match pending_prompt.take() {
                Some(t) => t,
                None => {
                    // Entering idle: announce it so the host's soft TTL
                    // arms for a genuinely-idle session.
                    emit(&evt_tx, HarnessEvent::Idle).await;
                    loop {
                        tokio::select! {
                            cmd = cmd_rx.recv() => match cmd {
                                Some(HarnessCommand::Prompt { text }) => break text,
                                Some(HarnessCommand::Shutdown { .. }) => {
                                    tracing::info!("shutdown received while idle; exiting");
                                    return ExitCode::SUCCESS;
                                }
                                // No child to stop; nothing to flush.
                                Some(HarnessCommand::Interrupt) => {
                                    tracing::debug!("interrupt while idle; nothing to stop");
                                }
                                Some(HarnessCommand::Checkpoint { .. }) => {}
                                // All senders gone = the connection loop
                                // exited = process teardown.
                                None => return ExitCode::SUCCESS,
                            },
                            // A fresh connection attached while we're
                            // idle: re-announce so the host (whose hub
                            // state may be freshly rebuilt) re-arms the
                            // soft TTL. A reattach while RUNNING is not
                            // observed here — so it emits no `Idle`.
                            _ = reattach.notified() => {
                                emit(&evt_tx, HarnessEvent::Idle).await;
                            }
                        }
                    }
                }
            };

            let outcome = run_one_claude_prompt(&cli, &evt_tx, &text, &mut cmd_rx).await;

            // Close the run: RunInterrupted for an operator stop,
            // RunCompleted otherwise. The next loop iteration emits the
            // trailing `Idle` (when there's no queued prompt) — so a
            // queued prompt runs back-to-back with no idle gap.
            let run_id = outcome.run_id.clone().unwrap_or_else(|| "unknown".into());
            let end_event = if outcome.interrupted {
                HarnessEvent::RunInterrupted { run_id }
            } else {
                HarnessEvent::RunCompleted {
                    run_id,
                    ok: outcome.ok,
                }
            };
            emit(&evt_tx, end_event).await;

            if let Some(queued) = outcome.queued_prompt {
                pending_prompt = Some(queued);
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

    #[derive(Default)]
    struct RunOutcome {
        ok: bool,
        run_id: Option<String>,
        queued_prompt: Option<String>,
        /// The run was stopped by an operator `HarnessCommand::Interrupt`
        /// (we SIGINT'd the child). The caller emits `RunInterrupted`
        /// rather than `RunCompleted` for this.
        interrupted: bool,
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

    async fn run_one_claude_prompt(
        cli: &Cli,
        evt_tx: &mpsc::Sender<HarnessEvent>,
        text: &str,
        cmd_rx: &mut mpsc::Receiver<HarnessCommand>,
    ) -> RunOutcome {
        let resume_id = read_claude_session_id().await;
        let argv = build_claude_argv(&resume_id, text);
        tracing::info!(?argv, "spawning claude");

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
            .stdin(Stdio::null())
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
                tracing::error!(error = %e, bin = %claude_bin, "spawn claude failed");
                emit(
                    evt_tx,
                    HarnessEvent::AgentMessage {
                        run_id: "spawn-failed".into(),
                        message_id: format!("spawn-{}", uuid::Uuid::new_v4()),
                        role: AgentRole::System,
                        text: format!("failed to spawn `{claude_bin}`: {e}"),
                    },
                )
                .await;
                return RunOutcome::default();
            }
        };

        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();

        // Drain claude's stderr into a bounded rolling tail. The task
        // always reads the pipe (no wedge) and re-echoes each line to our
        // own stderr so it still lands in the in-guest harness log. On an
        // abnormal exit we attach this tail to a System message so the
        // failure cause survives VM teardown (the harness log does not).
        let stderr = child.stderr.take().unwrap();
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

        let deadline = Instant::now() + Duration::from_secs(cli.max_run_secs);
        let mut tool_calls = 0u32;
        let mut run_id: Option<String> = None;
        let mut queued_prompt: Option<String> = None;
        let mut ok = true;
        let mut interrupted = false;
        // The terminal `result` line claude emits at the end of every
        // completed turn (success OR a handled error like our e2e 401).
        // Its ABSENCE at stdout EOF is the crash signature — claude died
        // mid-turn before reporting a result.
        let mut result_marker: Option<ResultMarker> = None;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                tracing::warn!("max_run_secs elapsed; SIGTERM-ing claude");
                ok = false;
                let _ = child.start_kill();
                break;
            }

            tokio::select! {
                res = timeout(remaining, lines.next_line()) => {
                    match res {
                        Ok(Ok(Some(line))) => {
                            if result_marker.is_none() {
                                result_marker = detect_result_marker(&line);
                            }
                            if let Some(translated) = translate_jsonl(
                                &line,
                                &mut run_id,
                                &mut tool_calls,
                                cli.max_tool_calls,
                            ) {
                                for ev in translated {
                                    emit(evt_tx, ev).await;
                                }
                            }
                        }
                        Ok(Ok(None)) => break, // EOF
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "stdout read error");
                            ok = false;
                            break;
                        }
                        Err(_) => {
                            tracing::warn!("max_run_secs elapsed; SIGTERM-ing claude");
                            ok = false;
                            let _ = child.start_kill();
                            break;
                        }
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(HarnessCommand::Prompt { text }) => {
                            if queued_prompt.is_none() {
                                queued_prompt = Some(text);
                            } else {
                                tracing::warn!("dropping prompt — one already queued");
                            }
                        }
                        Some(HarnessCommand::Shutdown { .. }) => {
                            tracing::info!("shutdown mid-run; SIGTERM-ing claude");
                            let _ = child.start_kill();
                            ok = false;
                            break;
                        }
                        Some(HarnessCommand::Interrupt) => {
                            // Operator stop: SIGINT the current claude
                            // child (graceful — it flushes its
                            // conversation file per message, so the
                            // session stays cleanly --resume-able). We
                            // break and let the bounded reap below
                            // escalate to SIGKILL if it doesn't go. The
                            // run closes as RunInterrupted, NOT a kill —
                            // the adapter stays attached for the next
                            // prompt.
                            tracing::info!("interrupt mid-run; SIGINT-ing claude");
                            sigint_child(&child);
                            ok = false;
                            interrupted = true;
                            break;
                        }
                        Some(HarnessCommand::Checkpoint { .. }) => {}
                        None => break,
                    }
                }
            }
        }

        // Bounded reap. Capture the child's ExitStatus (the signal/code
        // is the crash signal — SIGKILL ≈ OOM-killer, SIGSEGV ≈ crash)
        // instead of discarding it. The SIGINT (interrupt), SIGKILL
        // (shutdown / max_run_secs), or EOF paths should all let the
        // child exit promptly. If a signalled child doesn't go within the
        // grace window, escalate to SIGKILL so we never wedge the loop.
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

        // Collect the stderr tail. The drain task ends when the child
        // closes stderr (the reap above just ensured that for a normal
        // exit); bound the wait so a still-running child on the grace
        // path can't hang us.
        let stderr_tail: Vec<String> = match timeout(Duration::from_secs(2), stderr_task).await {
            Ok(Ok(tail)) => tail,
            _ => Vec::new(),
        };

        // Abnormal exit detection. `ok` is still true ONLY on the clean
        // stdout-EOF path — every path where WE stopped the child
        // (shutdown / max_run_secs / interrupt / read error) set
        // ok=false. So `ok && result_marker.is_none()` means claude
        // EOF'd on its own WITHOUT emitting its terminal `result` line:
        // it died mid-turn. Surface the cause (exit signal + stderr tail)
        // as a System message so it persists into session_events, which
        // outlives the VM (and its in-guest harness log) on eviction.
        //
        // Gating on the missing `result` line — NOT on a non-zero exit
        // code — is deliberate: the e2e bogus-token test makes claude
        // 401 and exit non-zero, but claude DOES emit a `result` line
        // first, so this stays silent there and the test's terminal
        // AgentMessage remains the 401.
        if ok && result_marker.is_none() {
            ok = false;
            let detail = describe_abnormal_exit(exit_status, &stderr_tail);
            tracing::error!(detail, "claude exited abnormally mid-turn");
            emit(
                evt_tx,
                HarnessEvent::AgentMessage {
                    run_id: run_id.clone().unwrap_or_else(|| "abnormal-exit".into()),
                    message_id: format!("abnormal-{}", uuid::Uuid::new_v4()),
                    role: AgentRole::System,
                    text: detail,
                },
            )
            .await;
        } else {
            // Read the result fields explicitly (not via Debug) so they
            // count as live for the dead_code lint, and so the log
            // distinguishes a clean `success` from a handled-error result
            // (`error_max_turns`, an API error claude surfaced, …).
            tracing::info!(
                status = ?exit_status,
                result_subtype = result_marker.as_ref().map(|m| m.subtype.as_str()),
                result_is_error = result_marker.as_ref().map(|m| m.is_error),
                ok,
                "claude run ended",
            );
        }

        RunOutcome {
            ok,
            run_id,
            queued_prompt,
            interrupted,
        }
    }

    fn build_claude_argv(resume_id: &Option<String>, text: &str) -> Vec<String> {
        let mut argv = vec![
            "--print".to_string(),
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
        argv.push(text.to_string());
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
    /// stream-json` into zero or more HarnessEvents. Tracks the
    /// run_id captured from `system.init` and the per-run
    /// tool-call count.
    pub fn translate_jsonl(
        line: &str,
        run_id: &mut Option<String>,
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
                    let session_id = v
                        .get("session_id")
                        .and_then(|s| s.as_str())
                        .map(str::to_string);
                    let rid = session_id
                        .clone()
                        .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4()));
                    *run_id = Some(rid.clone());
                    if let Some(sid) = session_id {
                        // `translate_jsonl` is sync and is called from
                        // unit tests outside any tokio runtime. Only
                        // fire-and-forget the disk write when a runtime
                        // is actually available; in tests this becomes
                        // a no-op rather than panicking, and we don't
                        // accidentally write to `/workspace/.engram` on
                        // the test host.
                        if let Ok(handle) = tokio::runtime::Handle::try_current() {
                            handle.spawn(async move { write_claude_session_id(&sid).await });
                        }
                    }
                    out.push(HarnessEvent::RunStarted {
                        run_id: rid,
                        prompt_summary: None,
                    });
                }
            }
            "assistant" => {
                let rid = run_id.clone().unwrap_or_default();
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
                let rid = run_id.clone().unwrap_or_default();
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
                // Outer loop emits RunCompleted from claude's exit
                // status; the result event is informational.
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

        async fn write_fake_claude(lines: &[&str]) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let mut body = String::from("#!/bin/sh\n");
            for l in lines {
                body.push_str(&format!("printf '%s\\n' '{l}'\n"));
            }
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
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

        // End-to-end (minus real claude): the engine runs the initial
        // prompt, emits the run's events, then a trailing `Idle`; a
        // reattach while idle re-announces `Idle`; Shutdown exits.
        #[tokio::test]
        async fn engine_runs_prompt_then_idle_and_reannounces_on_reattach() {
            let script = write_fake_claude(&[
                r#"{"type":"system","subtype":"init"}"#,
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
                Some("do it".into()),
            ));

            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::RunStarted { .. })
            ));
            match evt_rx.recv().await {
                Some(HarnessEvent::AgentMessage { text, .. }) => assert_eq!(text, "hello"),
                other => panic!("expected AgentMessage, got {other:?}"),
            }
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::RunCompleted { ok: true, .. })
            ));
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // A reconnect while idle re-announces Idle so the host's soft
            // TTL re-arms; a mid-run reattach (covered implicitly above)
            // emits none.
            reattach.notify_one();
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 0 })
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine should exit on shutdown");
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
    fn translates_system_init_and_assistant_text() {
        let init = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        let asst = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"hi there"}]}}"#;
        let mut run_id = None;
        let mut tc = 0u32;
        let evs = translate_jsonl(init, &mut run_id, &mut tc, 50).unwrap();
        assert_eq!(run_id.as_deref(), Some("abc-123"));
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], HarnessEvent::RunStarted { .. }));

        let evs = translate_jsonl(asst, &mut run_id, &mut tc, 50).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            HarnessEvent::AgentMessage { text, .. } => assert_eq!(text, "hi there"),
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn translates_tool_use_and_tool_result_pair() {
        let init = r#"{"type":"system","subtype":"init","session_id":"x"}"#;
        let asst = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls /workspace"}}]}}"#;
        let user = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"file1.txt\nfile2.py","is_error":false}]}}"#;

        let mut run_id = None;
        let mut tc = 0u32;
        translate_jsonl(init, &mut run_id, &mut tc, 50);
        let evs = translate_jsonl(asst, &mut run_id, &mut tc, 50).unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], HarnessEvent::ToolCallStarted { .. }));
        assert_eq!(tc, 1);

        let evs = translate_jsonl(user, &mut run_id, &mut tc, 50).unwrap();
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
