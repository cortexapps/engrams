//! Harness adapter for the `claude` CLI.
//!
//! Lives at `/sbin/engram-harness-claude` inside the rootfs (or
//! cargo-target/.../engram-harness-claude on the dev-mode
//! ProcessBackend path). Exec'd by `engram-agentd`'s harness
//! supervisor after the host pushes a `SpawnHarness` request
//! describing this binary.
//!
//! Strategy (ADR 0037): **persistent process**. We spawn ONE `claude
//! --input-format stream-json --output-format stream-json
//! --dangerously-skip-permissions [--resume <claude_id>]` child and
//! hold it for the adapter's lifetime. Each `Prompt` command is fed as
//! one stream-json user message into the child's kept-open stdin; the
//! per-turn boundary is the `{"type":"result"}` frame (NOT process
//! exit). The child stays warm between turns — no Bun boot / V8-heap
//! rebuild per prompt — which is also what lets a warm, idle harness be
//! captured into the base memory snapshot (ADR 0022 File-restore).
//!
//! The child lives in `entry()`, OUTSIDE the per-connection loop, so it
//! survives a vsock reconnect — the canonical case being an FC
//! snapshot/restore, where the whole guest (this adapter + its `claude`
//! child) is frozen and thawed while only the host-side connection
//! drops. EOF on the child's stdout therefore means the child
//! **crashed** (OOM / panic / signal), not a turn-end: we surface a
//! diagnostic System message and respawn with `--resume` to recover the
//! conversation. `Interrupt` SIGINTs the in-flight turn but keeps the
//! process alive.
//!
//! `--dangerously-skip-permissions` is mandatory: there's no human in
//! the VM to answer permission prompts. `IS_SANDBOX=1` is the
//! documented escape hatch for Claude's root-check. The sandbox is the
//! safety boundary, not Claude's per-tool consent.
//!
//! The first turn captures Claude's auto-generated session id and
//! stashes it in `/workspace/.engram/claude-session-id` so a respawn
//! (crash recovery / reconnect) can `--resume` into the same
//! conversation. The file is per-session writable disk, not shared.

// Cross-platform stub — vsock dialing is Linux-only, and the
// adapter only ships inside FC rootfs / Linux ProcessBackend.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("engram-harness-claude is Linux-only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod adapter {
    use std::collections::HashMap;
    use std::process::{ExitCode, Stdio};
    use std::sync::Arc;
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
    use tokio::process::{ChildStdin, ChildStdout, Command};
    use tokio::sync::{mpsc, Mutex};
    use tokio::time::{timeout, Instant};

    pub const CLAUDE_SESSION_ID_FILE: &str = "/workspace/.engram/claude-session-id";

    /// Grace window after a SIGINT (operator interrupt or per-turn
    /// deadline) before we escalate to SIGKILL. Claude flushes its
    /// transcript per message, so a clean turn-cancel lands well within
    /// this; the escalation only fires if the child wedges.
    const SIGINT_GRACE: Duration = Duration::from_secs(5);

    /// Truncation budgets used when building summary fields. Adapter-
    /// local enforcement of the wire docs.
    pub const MAX_ARGS_SUMMARY_BYTES: usize = 1024;
    pub const MAX_RESULT_SUMMARY_BYTES: usize = 4096;
    pub const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;

    #[derive(Parser, Debug)]
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

        /// Per-turn wall-clock cap (seconds). After this the adapter
        /// SIGINTs the in-flight turn (and SIGKILLs if it wedges) and
        /// closes the turn `RunCompleted{ok:false}` — the persistent
        /// process stays alive for the next prompt. Default is 24h: an
        /// autonomous agent in an isolated VM is expected to grind on a
        /// task for many hours. Tighten per-deployment if needed.
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

        // Per-session identity delivered by a late `Bind` (ADR 0037 P4).
        // Both live in `entry()` so they survive a vsock reconnect (the
        // FC pause/resume case): once a warm harness is bound, the bound
        // session id + env persist across reconnects.
        //   - `bound_env`: the per-session env layered onto every `claude`
        //     spawn. Empty on the cold path (agentd seeded the process env
        //     already); populated by `Bind` for a warm-captured harness.
        //   - `bound_session_id`: overrides the (sentinel) `cli.session_id`
        //     in `HarnessAttach` once bound, so reconnects re-attach under
        //     the real session.
        let mut bound_env: HashMap<String, String> = HashMap::new();
        let mut bound_session_id: Option<SessionId> = None;

        // Spawn the persistent `claude` child up front, before the dial
        // loop. It comes up idle (claude only dials the API on the first
        // user message), so this is the warm-but-quiescent state a base
        // snapshot wants to capture (ADR 0037). A spawn failure here is
        // non-fatal — `ensure_claude` retries on the first prompt.
        let mut claude: Option<ClaudeChild> = match spawn_persistent_claude(
            &cli,
            &bound_env,
            read_claude_session_id().await,
        )
        .await
        {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(error = %e, "initial claude spawn failed; will retry on first prompt");
                None
            }
        };

        // Outer loop: dial → run one connection → if the connection
        // dropped (FC snapshot/restore round-trip is the canonical
        // case), back off briefly and re-dial. State that survives a
        // reconnect lives here:
        //   - `claude`: the persistent child. A reconnect re-establishes
        //     the host channel only; the child (and its warm heap +
        //     conversation) keeps running across the freeze.
        //   - `next_prompt`: a queued user prompt the previous
        //     connection died before we could ack/run. Carried forward.
        //   - `/workspace/.engram/claude-session-id`: persisted on disk;
        //     used to `--resume` on a crash respawn.
        let mut next_prompt: Option<String> = std::env::var("ENGRAM_INITIAL_PROMPT").ok();

        // ADR 0037 P5b: a SIGUSR1 nudge forces an immediate host-connection
        // reconnect. After a warm-restore, the harness's vsock to the host
        // died with the capture VM but its read SILENTLY HANGS (no RST/EOF —
        // unlike a pause/resume), so the read-error-driven reconnect never
        // fires and the harness sits idle forever. agentd (which supervises
        // this process) sends SIGUSR1 on restore; the idle connection loop
        // `select!`s on this `Notify` and re-dials at once.
        let reconnect_signal = std::sync::Arc::new(tokio::sync::Notify::new());
        {
            let sig = reconnect_signal.clone();
            tokio::spawn(async move {
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                {
                    Ok(mut s) => {
                        while s.recv().await.is_some() {
                            tracing::info!("SIGUSR1: forcing host-connection reconnect");
                            // notify_one (not notify_waiters): stores a permit
                            // if the idle loop isn't parked on `.notified()`
                            // yet, so a nudge that races our re-entry into the
                            // select! still fires on the next poll instead of
                            // being silently dropped.
                            sig.notify_one();
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "couldn't install SIGUSR1 handler; warm-restore reconnect nudge disabled",
                    ),
                }
            });
        }

        let mut consecutive_failures: u32 = 0;
        const MAX_BACKOFF_SECS: u64 = 30;
        const MAX_CONSECUTIVE_FAILURES: u32 = 10;
        loop {
            let stream = match dial(&cli).await {
                Some(s) => s,
                None => {
                    // Dial itself failed — distinct from a mid-session
                    // drop. With no connection, there's nothing to
                    // reconnect *to*, so back off and retry.
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if consecutive_failures > MAX_CONSECUTIVE_FAILURES {
                        tracing::error!(
                            consecutive_failures,
                            "giving up after repeated dial failures",
                        );
                        return ExitCode::from(1);
                    }
                    let backoff =
                        std::cmp::min(MAX_BACKOFF_SECS, 1u64 << consecutive_failures.min(5));
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    continue;
                }
            };
            match run_one_connection(
                stream,
                &cli,
                &mut next_prompt,
                &mut claude,
                &mut bound_env,
                &mut bound_session_id,
                &reconnect_signal,
            )
            .await
            {
                Outcome::Exit(code) => return code,
                Outcome::Reconnect { reason } => {
                    // ADR 0037 P5b: a SIGUSR1 nudge is a *deliberate*
                    // reconnect (the host knows the connection is dead and
                    // wants a fresh one NOW), not a failure. Re-dial
                    // immediately — no backoff, no failure count — so the
                    // warm-restore latency win isn't eaten by a 2s+ sleep.
                    if reason == "reconnect_nudge" {
                        tracing::info!(reason, "harness reconnect nudge; re-dialing immediately");
                        continue;
                    }
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff =
                        std::cmp::min(MAX_BACKOFF_SECS, 1u64 << consecutive_failures.min(5));
                    tracing::warn!(
                        reason,
                        consecutive_failures,
                        backoff_secs = backoff,
                        "harness connection dropped; reconnecting",
                    );
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
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

    enum Outcome {
        /// Adapter is done — propagate this exit code up.
        Exit(ExitCode),
        /// Connection dropped mid-life; re-dial and continue.
        Reconnect { reason: &'static str },
    }

    /// The persistent `claude` child: the process, its kept-open stdin
    /// (we write one stream-json user message per turn), and its stdout
    /// line reader (must persist across turns — rebuilding it would
    /// discard buffered partial JSONL).
    struct ClaudeChild {
        child: tokio::process::Child,
        stdin: ChildStdin,
        lines: tokio::io::Lines<BufReader<ChildStdout>>,
    }

    /// Spawn the persistent `claude` child in stream-json I/O mode.
    /// stdin is piped (we feed turns); stdout is piped (we parse
    /// events); stderr is inherited (lands in the in-guest harness log —
    /// piping it risks a write(2) deadlock once the 64KiB buffer fills).
    /// `resume_id` is passed as `--resume` only when recovering an
    /// existing conversation (crash respawn / reconnect).
    ///
    /// `bound_env` is the per-session environment delivered by a late
    /// `Bind` (ADR 0037 P4); it is empty on the cold path (agentd already
    /// seeded the harness *process* env with the session env, which the
    /// child inherits) and populated only for a warm-captured harness that
    /// booted with placeholder env and must adopt this session's forge/
    /// upload tokens + real session id. It is layered on **before** our
    /// fixed invariants below so a session env can't clobber `IS_SANDBOX` /
    /// the Bash timeouts.
    async fn spawn_persistent_claude(
        cli: &Cli,
        bound_env: &HashMap<String, String>,
        resume_id: Option<String>,
    ) -> std::io::Result<ClaudeChild> {
        let argv = build_persistent_argv(&resume_id);
        tracing::info!(
            ?argv,
            bound_env_keys = bound_env.len(),
            "spawning persistent claude"
        );
        let claude_bin: &str = cli
            .claude_bin
            .as_deref()
            .expect("claude_bin resolved at entry()");
        let mut child = Command::new(claude_bin)
            .args(&argv)
            // Per-session env from a late Bind (empty on the cold path).
            // Applied first so the fixed invariants below always win.
            .envs(bound_env)
            // Long-run knobs for unattended Claude inside a VM (Bash
            // defaults would silently kill long builds), the
            // nonessential-traffic toggle (no autoupdater/telemetry),
            // and IS_SANDBOX=1 (root-check escape hatch — the VM is the
            // boundary).
            .env("BASH_DEFAULT_TIMEOUT_MS", "1800000")
            .env("BASH_MAX_TIMEOUT_MS", "7200000")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("IS_SANDBOX", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let lines = BufReader::new(stdout).lines();
        Ok(ClaudeChild {
            child,
            stdin,
            lines,
        })
    }

    fn build_persistent_argv(resume_id: &Option<String>) -> Vec<String> {
        let mut argv = vec![
            "--input-format".to_string(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // No human in the VM to approve tool calls. The VM itself is
            // the safety boundary.
            "--dangerously-skip-permissions".into(),
        ];
        if let Some(id) = resume_id {
            argv.push("--resume".into());
            argv.push(id.clone());
        }
        argv
    }

    /// Feed one user turn into the persistent child's stdin as a
    /// stream-json user message. Built with `serde_json` (NOT string
    /// interpolation) so prompts containing quotes / newlines /
    /// backslashes serialize correctly.
    async fn write_user_turn(stdin: &mut ChildStdin, text: &str) -> std::io::Result<()> {
        let msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        });
        let mut line = serde_json::to_string(&msg).unwrap_or_default();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await
    }

    /// Render a non-success `ExitStatus` into a diagnostic string.
    /// Distinguishes a plain non-zero exit from a fatal signal, and
    /// calls out `SIGKILL` (commonly the OOM-killer inside the guest).
    pub fn describe_exit(status: std::process::ExitStatus) -> String {
        if let Some(code) = status.code() {
            return format!("claude exited with code {code}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                let hint = match sig {
                    9 => " (SIGKILL — likely the guest OOM-killer; check dmesg / cgroup memory)",
                    11 => " (SIGSEGV — claude crashed)",
                    6 => " (SIGABRT — claude panicked/aborted)",
                    _ => "",
                };
                return format!("claude killed by signal {sig}{hint}");
            }
        }
        "claude exited abnormally (no exit code)".to_string()
    }

    async fn run_one_connection(
        stream: (BoxedReader, BoxedWriter),
        cli: &Cli,
        next_prompt: &mut Option<String>,
        claude: &mut Option<ClaudeChild>,
        bound_env: &mut HashMap<String, String>,
        bound_session_id: &mut Option<SessionId>,
        reconnect_signal: &std::sync::Arc<tokio::sync::Notify>,
    ) -> Outcome {
        let (mut reader, writer) = stream;

        // Handshake. Attach under the bound session id once a `Bind` has
        // arrived (warm-captured harness adopting its session); until then
        // the sentinel `cli.session_id` from spawn.
        let mut writer_unlocked = writer;
        if let Err(e) = write_msg(
            &mut writer_unlocked,
            &HarnessAttach {
                session_id: bound_session_id.unwrap_or(cli.session_id),
                harness_version: format!("engram-harness-claude/{}", env!("CARGO_PKG_VERSION")),
            },
        )
        .await
        {
            tracing::error!(error = %e, "attach write failed");
            return Outcome::Reconnect {
                reason: "attach_write",
            };
        }
        let ack: HarnessAttachAck = match read_msg(&mut reader).await {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "attach ack read failed");
                return Outcome::Reconnect { reason: "ack_read" };
            }
        };
        if !ack.ok {
            // Host explicitly rejected — not transport flakiness, no
            // amount of retry will fix it. Exit terminally.
            tracing::error!(message = ?ack.message, "host rejected attach");
            return Outcome::Exit(ExitCode::from(1));
        }

        // Reader task: shovel HarnessCommands onto an mpsc; the writer
        // side stays single-threaded so event writes are serial and
        // ordered. Channel-close (sender dropped) signals the reader hit
        // EOF — the connection is dead.
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<HarnessCommand>(8);
        let reader_task = tokio::spawn(async move {
            loop {
                match read_msg::<_, HarnessFrame>(&mut reader).await {
                    Ok(HarnessFrame::Command(c)) => {
                        if cmd_tx.send(c).await.is_err() {
                            return;
                        }
                    }
                    Ok(HarnessFrame::Event(_)) => {} // hosts shouldn't send events
                    Err(_) => return,
                }
            }
        });

        let writer = Arc::new(Mutex::new(writer_unlocked));

        // If we have a pending prompt (initial $ENGRAM_INITIAL_PROMPT,
        // or a queued one rolled over from a dropped connection), run it
        // on this turn. Otherwise emit Idle so the host knows we're
        // waiting.
        if next_prompt.is_none() && write_event(&writer, HarnessEvent::Idle).await.is_err() {
            reader_task.abort();
            return Outcome::Reconnect {
                reason: "idle_write",
            };
        }

        loop {
            let text = match next_prompt.take() {
                Some(t) => t,
                None => loop {
                    // ADR 0037 P5b: while idle, a SIGUSR1 reconnect nudge
                    // (warm-restore) must wake us even though the dead vsock
                    // read is hung — so `select!` the command channel against
                    // the reconnect signal. On nudge: drop this (dead)
                    // connection + re-dial.
                    let cmd = tokio::select! {
                        c = cmd_rx.recv() => c,
                        _ = reconnect_signal.notified() => {
                            tracing::info!("reconnect nudge while idle; re-dialing host channel");
                            reader_task.abort();
                            return Outcome::Reconnect { reason: "reconnect_nudge" };
                        }
                    };
                    match cmd {
                        Some(HarnessCommand::Prompt { text }) => break text,
                        Some(HarnessCommand::Shutdown { .. }) => {
                            tracing::info!("shutdown received; killing claude + exiting");
                            kill_claude(claude).await;
                            reader_task.abort();
                            return Outcome::Exit(ExitCode::SUCCESS);
                        }
                        Some(HarnessCommand::Checkpoint { .. }) => {
                            // Claude writes its conversation file
                            // synchronously per message — nothing to
                            // flush.
                        }
                        Some(HarnessCommand::Interrupt) => {
                            // Idle between turns — no in-flight turn to
                            // SIGINT. Operator stop with nothing running
                            // is a no-op; keep waiting.
                            tracing::debug!("interrupt while idle; nothing to stop");
                        }
                        Some(HarnessCommand::Bind {
                            session_id,
                            session_env,
                            first_prompt,
                        }) => {
                            tracing::info!(
                                %session_id,
                                env_keys = session_env.len(),
                                has_prompt = first_prompt.is_some(),
                                "late-bind: adopting per-session identity",
                            );
                            *bound_session_id = Some(session_id);
                            *bound_env = session_env;
                            // Deliver this session's vsock capability tokens
                            // (forge / upload) + real session id to the
                            // in-guest helpers via the per-session env file.
                            // We do NOT respawn the warm child: keeping it
                            // alive is what preserves the warm V8 heap (the
                            // whole point of warm-capture). Its frozen
                            // capture-time env is fine — the HTTP-egress
                            // OAuth token is a constant placeholder the proxy
                            // swaps per-session on the wire, and the helpers
                            // read the real forge/upload tokens from the file.
                            if let Err(e) = engram_harness_proto::write_session_env_file(bound_env)
                            {
                                tracing::warn!(error = %e, "writing session env file at bind failed");
                            }
                            // Adopt the running warm child as-is. Spawn only
                            // if none is present (warm spawn failed at capture
                            // / cold-ish fallback) — then it gets the bound
                            // env layered on.
                            if claude.is_none() {
                                *claude = spawn_persistent_claude(
                                    cli,
                                    bound_env,
                                    read_claude_session_id().await,
                                )
                                .await
                                .ok();
                            }
                            // Run the bound first prompt now; otherwise stay
                            // idle and await a later `Prompt`.
                            match first_prompt {
                                Some(t) => break t,
                                None => continue,
                            }
                        }
                        None => {
                            // Reader task ended → connection died.
                            // Reconnect rather than exit; the persistent
                            // claude child keeps running.
                            reader_task.abort();
                            return Outcome::Reconnect {
                                reason: "cmd_chan_closed",
                            };
                        }
                    }
                },
            };

            // Stash the prompt back into next_prompt so a mid-turn
            // disconnect or crash re-runs it on the next attempt. Cleared
            // on a delivered turn below.
            *next_prompt = Some(text.clone());

            // Ensure the persistent child is alive (the eager spawn at
            // entry may have failed, or a prior crash left it None).
            if claude.is_none() {
                match spawn_persistent_claude(cli, bound_env, read_claude_session_id().await).await
                {
                    Ok(c) => *claude = Some(c),
                    Err(e) => {
                        tracing::error!(error = %e, "spawn claude failed");
                        let _ = write_event(
                            &writer,
                            HarnessEvent::AgentMessage {
                                run_id: "spawn-failed".into(),
                                message_id: format!("spawn-{}", uuid::Uuid::new_v4()),
                                role: AgentRole::System,
                                text: format!("failed to spawn claude: {e}"),
                            },
                        )
                        .await;
                        let _ = write_event(
                            &writer,
                            HarnessEvent::RunCompleted {
                                run_id: "spawn-failed".into(),
                                ok: false,
                            },
                        )
                        .await;
                        let _ = write_event(&writer, HarnessEvent::Idle).await;
                        *next_prompt = None;
                        continue;
                    }
                }
            }

            let outcome = {
                let child = claude.as_mut().expect("ensured above");
                run_one_turn(child, cli, &writer, &text, &mut cmd_rx).await
            };

            // Turn delivered — clear the pending prompt so we don't
            // re-run it (covers interrupt: an operator stop must NOT
            // auto-re-run the interrupted prompt).
            *next_prompt = None;

            // A crashed child must be replaced before the next turn.
            // Respawn with --resume to recover the conversation from the
            // persisted session id. Do this even on the reconnect path so
            // the child is ready when we re-attach.
            if outcome.crashed {
                *claude = spawn_persistent_claude(cli, bound_env, read_claude_session_id().await)
                    .await
                    .ok();
            }

            // Mid-turn vsock drop: the turn was drained to its boundary
            // (claude isn't wedged), but its events / RunCompleted are
            // lost on the dead connection. Reconnect; the next connection
            // emits Idle.
            if let Some(reason) = outcome.reconnect {
                reader_task.abort();
                return Outcome::Reconnect { reason };
            }

            if outcome.shutdown {
                kill_claude(claude).await;
                reader_task.abort();
                return Outcome::Exit(ExitCode::SUCCESS);
            }

            let run_id = outcome.run_id.clone().unwrap_or_else(|| "unknown".into());
            // Surface an abnormal child exit into the transcript as a
            // System message — it persists as a session_event in PG, so
            // "the agent crashed mid-turn" stays diagnosable even after
            // the VM is gone. Emitted before RunCompleted so the ordering
            // reads cause-then-close.
            if let Some(reason) = &outcome.abnormal_exit {
                let _ = write_event(
                    &writer,
                    HarnessEvent::AgentMessage {
                        run_id: run_id.clone(),
                        message_id: format!("crash-{}", uuid::Uuid::new_v4()),
                        role: AgentRole::System,
                        text: format!("agent process ended unexpectedly: {reason}"),
                    },
                )
                .await;
            }

            // An operator interrupt closes the turn with RunInterrupted
            // (distinct marker); everything else with RunCompleted. Both
            // are followed by the mandatory Idle, and the loop awaits the
            // next prompt either way — the session stays alive.
            let end_event = if outcome.interrupted {
                HarnessEvent::RunInterrupted { run_id }
            } else {
                HarnessEvent::RunCompleted {
                    run_id,
                    ok: outcome.ok,
                }
            };
            if write_event(&writer, end_event).await.is_err() {
                reader_task.abort();
                return Outcome::Reconnect {
                    reason: "run_completed_write",
                };
            }
            if write_event(&writer, HarnessEvent::Idle).await.is_err() {
                reader_task.abort();
                return Outcome::Reconnect {
                    reason: "idle_write",
                };
            }

            if let Some(queued) = outcome.queued_prompt {
                *next_prompt = Some(queued);
            }
        }
    }

    /// SIGTERM/SIGKILL + reap the persistent child (shutdown path).
    /// `kill_on_drop` is a backstop, but an explicit reap avoids leaving
    /// a zombie during the grace window.
    async fn kill_claude(claude: &mut Option<ClaudeChild>) {
        if let Some(mut c) = claude.take() {
            let _ = c.child.start_kill();
            let _ = timeout(SIGINT_GRACE, c.child.wait()).await;
        }
    }

    #[derive(Default)]
    struct TurnOutcome {
        ok: bool,
        run_id: Option<String>,
        /// A prompt that arrived mid-turn; the caller runs it next.
        queued_prompt: Option<String>,
        /// Stopped by an operator `Interrupt` — caller emits
        /// `RunInterrupted` rather than `RunCompleted`.
        interrupted: bool,
        /// The child died (EOF / read error / SIGKILL escalation) and
        /// must be respawned with `--resume` before the next turn.
        crashed: bool,
        /// Diagnostic for an *unsolicited* abnormal exit (a crash we
        /// didn't cause). `None` when we initiated the exit
        /// (interrupt / shutdown / deadline) or the turn ended cleanly.
        abnormal_exit: Option<String>,
        /// `Shutdown` arrived mid-turn — caller kills the child + exits.
        shutdown: bool,
        /// The host connection dropped mid-turn (event write failed or
        /// the command channel closed). The turn is drained to its
        /// boundary first; then the caller reconnects.
        reconnect: Option<&'static str>,
    }

    /// SIGINT a running `claude` child — the graceful "stop the current
    /// turn" signal (mirrors a Ctrl-C / ESC). Claude flushes its
    /// conversation file per message synchronously, so the session stays
    /// cleanly `--resume`-able. We escalate to SIGKILL only as a
    /// grace-timeout fallback.
    fn sigint_child(child: &tokio::process::Child) {
        if let Some(pid) = child.id() {
            if let Err(e) = kill(Pid::from_raw(pid as i32), Signal::SIGINT) {
                tracing::warn!(pid, error = %e, "SIGINT to claude child failed");
            }
        }
    }

    /// Drive one turn against the persistent child: feed the prompt as a
    /// stream-json user message, then read stdout until the `result`
    /// frame (turn done), EOF (child crashed), or a control event. The
    /// child stays alive across a clean turn.
    async fn run_one_turn<W>(
        claude: &mut ClaudeChild,
        cli: &Cli,
        writer: &Arc<Mutex<W>>,
        text: &str,
        cmd_rx: &mut mpsc::Receiver<HarnessCommand>,
    ) -> TurnOutcome
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        // Feed the prompt. A broken pipe means the child died between
        // turns → crashed; the caller respawns + re-runs.
        if let Err(e) = write_user_turn(&mut claude.stdin, text).await {
            tracing::warn!(error = %e, "writing prompt to claude stdin failed");
            return TurnOutcome {
                ok: false,
                crashed: true,
                abnormal_exit: Some(format!("could not deliver prompt to claude: {e}")),
                ..Default::default()
            };
        }

        let deadline = Instant::now() + Duration::from_secs(cli.max_run_secs);
        let mut tool_calls = 0u32;
        let mut run_id: Option<String> = None;
        let mut out = TurnOutcome {
            ok: true,
            ..Default::default()
        };
        // True once WE signalled the child (interrupt / deadline) — gates
        // abnormal-exit diagnosis and arms the SIGKILL grace.
        let mut solicited = false;
        // Set when we SIGINT the turn; once past it without a clean turn
        // boundary, escalate to SIGKILL.
        let mut kill_at: Option<Instant> = None;
        // Once the host channel is gone we stop writing events / reading
        // commands, but keep draining stdout so claude doesn't wedge on a
        // full pipe.
        let mut vsock_alive = true;

        loop {
            let now = Instant::now();
            // Per-turn deadline → SIGINT the turn (keep the process) and
            // arm the SIGKILL grace.
            if kill_at.is_none() && now >= deadline {
                tracing::warn!("max_run_secs elapsed; SIGINT-ing turn");
                sigint_child(&claude.child);
                out.ok = false;
                solicited = true;
                kill_at = Some(now + SIGINT_GRACE);
            }
            // Grace expired after a SIGINT → SIGKILL; the child is gone,
            // so the caller respawns.
            if let Some(k) = kill_at {
                if now >= k {
                    tracing::warn!("claude didn't end the turn after SIGINT; SIGKILL");
                    let _ = claude.child.start_kill();
                    out.crashed = true;
                    break;
                }
            }
            let read_deadline = kill_at.unwrap_or(deadline);
            let remaining = read_deadline.saturating_duration_since(now);

            tokio::select! {
                res = timeout(remaining, claude.lines.next_line()) => {
                    match res {
                        Ok(Ok(Some(line))) => {
                            let v: Value = match serde_json::from_str(&line) {
                                Ok(v) => v,
                                Err(_) => continue, // non-JSON noise
                            };
                            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            if ty == "result" {
                                // End of turn. The process stays alive.
                                let is_err = v
                                    .get("is_error")
                                    .and_then(|x| x.as_bool())
                                    .unwrap_or(false);
                                out.ok = out.ok && !is_err;
                                break;
                            }
                            for ev in translate_value(
                                &v,
                                &mut run_id,
                                &mut tool_calls,
                                cli.max_tool_calls,
                            ) {
                                if vsock_alive && write_event(writer, ev).await.is_err() {
                                    vsock_alive = false;
                                    out.reconnect = Some("event_write");
                                }
                            }
                        }
                        // EOF with no terminal `result` ⇒ the child died.
                        Ok(Ok(None)) => {
                            out.crashed = true;
                            break;
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "stdout read error");
                            out.crashed = true;
                            break;
                        }
                        // Read timed out: re-loop so the deadline / grace
                        // checks at the top fire.
                        Err(_) => {}
                    }
                }
                cmd = cmd_rx.recv(), if vsock_alive => {
                    match cmd {
                        Some(HarnessCommand::Prompt { text }) => {
                            if out.queued_prompt.is_none() {
                                out.queued_prompt = Some(text);
                            } else {
                                tracing::warn!("dropping prompt — one already queued");
                            }
                        }
                        Some(HarnessCommand::Shutdown { .. }) => {
                            out.shutdown = true;
                            break;
                        }
                        Some(HarnessCommand::Interrupt) => {
                            // Operator stop: SIGINT the turn but keep the
                            // process. Claude should end the turn with a
                            // `result` (clean RunInterrupted); if it dies
                            // instead, the EOF arm flags crashed and the
                            // caller respawns. Arm the SIGKILL grace in
                            // case it wedges.
                            tracing::info!("interrupt mid-turn; SIGINT-ing claude");
                            sigint_child(&claude.child);
                            out.ok = false;
                            out.interrupted = true;
                            solicited = true;
                            if kill_at.is_none() {
                                kill_at = Some(Instant::now() + SIGINT_GRACE);
                            }
                        }
                        Some(HarnessCommand::Checkpoint { .. }) => {}
                        Some(HarnessCommand::Bind { .. }) => {
                            // Late-bind targets an idle warm harness (no
                            // turn running). Mid-turn it would mean
                            // re-identifying a session under an in-flight
                            // run — undefined; ignore and keep the turn.
                            tracing::warn!(
                                "Bind received mid-turn; ignoring (warm-bind happens at idle)",
                            );
                        }
                        None => {
                            // Host channel closed mid-turn. Stop reading
                            // commands + writing events, but keep draining
                            // stdout to the turn boundary so claude isn't
                            // wedged; then reconnect.
                            vsock_alive = false;
                            out.reconnect = Some("cmd_chan_closed");
                        }
                    }
                }
            }
        }

        // If the child crashed, reap it and (when the exit was
        // unsolicited) classify it for the transcript. A clean turn-end
        // leaves the process running — nothing to reap.
        if out.crashed {
            let status = match timeout(SIGINT_GRACE, claude.child.wait()).await {
                Ok(Ok(s)) => Some(s),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "wait(claude) failed; exit status unknown");
                    None
                }
                Err(_) => {
                    let _ = claude.child.start_kill();
                    claude.child.wait().await.ok()
                }
            };
            if !solicited {
                out.abnormal_exit = match status {
                    Some(s) if s.success() => None,
                    Some(s) => Some(describe_exit(s)),
                    None => Some("claude exited but its status could not be read".to_string()),
                };
                if out.abnormal_exit.is_some() {
                    tracing::error!(reason = ?out.abnormal_exit, "claude exited abnormally mid-turn");
                }
            }
        }

        out
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

    async fn write_event<W>(writer: &Arc<Mutex<W>>, ev: HarnessEvent) -> std::io::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut w = writer.lock().await;
        write_msg(&mut *w, &HarnessFrame::Event(ev)).await
    }

    /// Parse one JSONL line from Claude's stream-json output, then
    /// translate. Returns `None` only when the line isn't JSON. Thin
    /// wrapper over [`translate_value`] so the hot path (which already
    /// parsed the line to detect the `result` frame) doesn't re-parse.
    /// Test-only: the hot path calls [`translate_value`] directly.
    #[cfg(test)]
    pub fn translate_jsonl(
        line: &str,
        run_id: &mut Option<String>,
        tool_calls: &mut u32,
        max_tool_calls: u32,
    ) -> Option<Vec<HarnessEvent>> {
        let v: Value = serde_json::from_str(line).ok()?;
        Some(translate_value(&v, run_id, tool_calls, max_tool_calls))
    }

    /// Translate one parsed stream-json frame into zero or more
    /// HarnessEvents. Tracks the run_id captured from `system.init` and
    /// the per-turn tool-call count. The `result` frame is handled by the
    /// turn loop (it's the turn boundary) and yields no events here.
    pub fn translate_value(
        v: &Value,
        run_id: &mut Option<String>,
        tool_calls: &mut u32,
        max_tool_calls: u32,
    ) -> Vec<HarnessEvent> {
        let mut out: Vec<HarnessEvent> = Vec::new();
        let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
            return out;
        };
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
                        // `translate_value` is sync and is called from
                        // unit tests outside any tokio runtime. Only
                        // fire-and-forget the disk write when a runtime
                        // is available; in tests this becomes a no-op
                        // rather than panicking.
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
                let Some(msg) = v.get("message") else {
                    return out;
                };
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
                let Some(msg) = v.get("message") else {
                    return out;
                };
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
            // "result" is the turn boundary, handled by run_one_turn.
            _ => {}
        }
        out
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

    /// The `result` frame is the turn boundary, not an event — it
    /// translates to zero events (run_one_turn consumes it directly).
    #[test]
    fn result_frame_yields_no_events() {
        let result = r#"{"type":"result","subtype":"success","is_error":false}"#;
        let mut run_id = Some("r".to_string());
        let mut tc = 0u32;
        let evs = translate_jsonl(result, &mut run_id, &mut tc, 50).unwrap();
        assert!(evs.is_empty(), "result should yield no events: {evs:?}");
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        let s = "🦀".repeat(100);
        let t = truncate_str(&s, 10);
        assert!(t.ends_with("…[truncated]"));
    }

    #[test]
    fn describe_exit_classifies_code_and_signal() {
        use std::os::unix::process::ExitStatusExt;
        // Raw wait status encoding: a plain exit code N is (N << 8).
        let code1 = std::process::ExitStatus::from_raw(1 << 8);
        assert!(
            describe_exit(code1).contains("code 1"),
            "a non-zero exit code should be reported",
        );
        // ...and a fatal signal N is the low 7 bits.
        let killed = std::process::ExitStatus::from_raw(9);
        let d = describe_exit(killed);
        assert!(
            d.contains("signal 9"),
            "fatal signal should be reported: {d}"
        );
        assert!(
            d.contains("OOM"),
            "SIGKILL should hint at the OOM-killer (the most common guest cause): {d}",
        );
    }
}
