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
    use std::process::{ExitCode, Stdio};
    use std::sync::Arc;
    use std::time::Duration;

    use clap::Parser;
    use engram_core::SessionId;
    use engram_harness_proto::{
        read_msg, write_msg, AgentRole, HarnessAttach, HarnessAttachAck, HarnessCommand,
        HarnessEvent, HarnessFrame,
    };
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
    use tokio::process::Command;
    use tokio::sync::{mpsc, Mutex};
    use tokio::time::{timeout, Instant};

    pub const CLAUDE_SESSION_ID_FILE: &str = "/workspace/.engram/claude-session-id";

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

        // Outer loop: dial → run one connection → if the connection
        // dropped (FC snapshot/restore round-trip is the canonical
        // case), back off briefly and re-dial. State that needs to
        // survive a reconnect lives here:
        //   - `next_prompt`: a queued user prompt the previous
        //     connection died before we could ack/run. We carry it
        //     forward so the user doesn't have to re-issue.
        //   - `/workspace/.engram/claude-session-id`: persisted by
        //     `run_one_claude_prompt`; survives because it's on disk.
        let mut next_prompt: Option<String> = std::env::var("ENGRAM_INITIAL_PROMPT").ok();
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
            match run_one_connection(stream, &cli, &mut next_prompt).await {
                Outcome::Exit(code) => return code,
                Outcome::Reconnect { reason } => {
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

    async fn run_one_connection(
        stream: (BoxedReader, BoxedWriter),
        cli: &Cli,
        next_prompt: &mut Option<String>,
    ) -> Outcome {
        let (mut reader, mut writer) = stream;

        // Handshake.
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

        // Reader task: shovel HarnessCommands onto an mpsc; the
        // writer side stays single-threaded so event writes are
        // serial and ordered. Channel-close (sender dropped) is the
        // signal that the reader task hit EOF — i.e. the connection
        // is dead.
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

        let writer = Arc::new(Mutex::new(writer));

        // If we have a pending prompt (initial $ENGRAM_INITIAL_PROMPT,
        // or a queued one rolled over from a dropped connection),
        // we'll run it on this turn. Otherwise emit Idle so the host
        // knows we're waiting.
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
                    match cmd_rx.recv().await {
                        Some(HarnessCommand::Prompt { text }) => break text,
                        Some(HarnessCommand::Shutdown { .. }) => {
                            tracing::info!("shutdown received; exiting");
                            reader_task.abort();
                            return Outcome::Exit(ExitCode::SUCCESS);
                        }
                        Some(HarnessCommand::Checkpoint { .. }) => {
                            // Claude writes its conversation file
                            // synchronously per message — nothing to
                            // flush.
                        }
                        None => {
                            // Reader task ended → connection died.
                            // Reconnect rather than exit.
                            reader_task.abort();
                            return Outcome::Reconnect {
                                reason: "cmd_chan_closed",
                            };
                        }
                    }
                },
            };

            // Stash the prompt back into next_prompt so a mid-run
            // disconnect re-runs it on the next connection. We clear
            // it on successful RunCompleted+Idle below.
            *next_prompt = Some(text.clone());

            let outcome = run_one_claude_prompt(cli, &writer, &text, &mut cmd_rx).await;

            // Successful (or at least delivered) run — clear the
            // pending prompt so we don't re-run it on the next turn.
            *next_prompt = None;

            if write_event(
                &writer,
                HarnessEvent::RunCompleted {
                    run_id: outcome.run_id.clone().unwrap_or_else(|| "unknown".into()),
                    ok: outcome.ok,
                },
            )
            .await
            .is_err()
            {
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

    #[derive(Default)]
    struct RunOutcome {
        ok: bool,
        run_id: Option<String>,
        queued_prompt: Option<String>,
    }

    async fn run_one_claude_prompt<W>(
        cli: &Cli,
        writer: &Arc<Mutex<W>>,
        text: &str,
        cmd_rx: &mut mpsc::Receiver<HarnessCommand>,
    ) -> RunOutcome
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
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
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, bin = %claude_bin, "spawn claude failed");
                let _ = write_event(
                    writer,
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

        let deadline = Instant::now() + Duration::from_secs(cli.max_run_secs);
        let mut tool_calls = 0u32;
        let mut run_id: Option<String> = None;
        let mut queued_prompt: Option<String> = None;
        let mut ok = true;

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
                            if let Some(translated) = translate_jsonl(
                                &line,
                                &mut run_id,
                                &mut tool_calls,
                                cli.max_tool_calls,
                            ) {
                                for ev in translated {
                                    let _ = write_event(writer, ev).await;
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
                        Some(HarnessCommand::Checkpoint { .. }) => {}
                        None => break,
                    }
                }
            }
        }

        let _ = child.wait().await;
        RunOutcome {
            ok,
            run_id,
            queued_prompt,
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

    async fn write_event<W>(writer: &Arc<Mutex<W>>, ev: HarnessEvent) -> std::io::Result<()>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut w = writer.lock().await;
        write_msg(&mut *w, &HarnessFrame::Event(ev)).await
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
}
