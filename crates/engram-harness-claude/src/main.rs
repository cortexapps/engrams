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
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{ExitCode, ExitStatus, Stdio};
    use std::sync::Arc;
    #[cfg(test)]
    use std::sync::Mutex;
    use std::time::Duration;

    use clap::Parser;
    use engram_core::SessionId;
    #[cfg(test)]
    use engram_harness_proto::{read_msg, write_msg, HarnessFrame};
    use engram_harness_proto::{
        AgentRole, EditHunk, FileChange, HarnessCommand, HarnessEvent, MAX_FILE_CHANGE_BYTES,
    };
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    use serde_json::Value;
    #[cfg(test)]
    use tokio::io::AsyncWrite;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command;
    use tokio::sync::{mpsc, Notify};
    use tokio::time::{timeout, Instant};

    pub const CLAUDE_SESSION_ID_FILE: &str = "/workspace/.engram/claude-session-id";

    /// ADR 0054 Flavor B: the per-session hook↔harness unix socket and the
    /// generated `--settings` file. Both live under `/workspace/.engram`
    /// (already per-session, like the claude session-id file), so the names
    /// are fixed yet collision-free across sessions and correct on every
    /// backend (FC / VZ / Process). The harness binds the socket
    /// before spawning claude; the hook reaches it via `ENGRAM_HOOK_SOCK`.
    pub const HOOK_SOCK_FILE: &str = "/workspace/.engram/hook.sock";
    pub const HOOK_SETTINGS_FILE: &str = "/workspace/.engram/claude-settings.json";
    pub const MCP_CONFIG_FILE: &str = "/workspace/.engram/mcp-config.json";
    pub const MCP_SOCK_FILE: &str = "/workspace/.engram/mcp.sock";

    /// ADR 0089 P2: the orchestrator's model-facing tool manifest. The main
    /// harness parses `ENGRAM_TOOLS` once at startup and carries this typed
    /// value across every claude respawn. The transient MCP bridge parses its
    /// own inherited copy because it is a self-invoked process with no shared
    /// address space.
    pub type ToolManifest = Vec<ManifestTool>;

    #[derive(Clone, Debug, serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ManifestTool {
        pub name: String,
        pub description: String,
        pub input_schema: Value,
        pub execution: ToolExecution,
        #[serde(default)]
        pub native_bindings: NativeBindings,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ToolExecution {
        Sync,
        Deferred,
    }

    #[derive(Clone, Debug, Default, serde::Deserialize)]
    pub struct NativeBindings {
        pub claude: Option<String>,
    }

    fn parse_tool_manifest(raw: &str) -> serde_json::Result<ToolManifest> {
        serde_json::from_str(raw)
    }

    fn manifest_from_env() -> ToolManifest {
        match std::env::var("ENGRAM_TOOLS") {
            Ok(raw) if !raw.trim().is_empty() => match parse_tool_manifest(&raw) {
                Ok(manifest) => manifest,
                Err(e) => {
                    tracing::error!(error = %e, "invalid ENGRAM_TOOLS manifest; exposing no injected tools");
                    Vec::new()
                }
            },
            _ => Vec::new(),
        }
    }

    fn injected_tools(manifest: &ToolManifest) -> impl Iterator<Item = &ManifestTool> {
        manifest
            .iter()
            .filter(|tool| tool.native_bindings.claude.is_none())
    }

    pub fn deferred_tool_names(manifest: &ToolManifest) -> HashSet<String> {
        injected_tools(manifest)
            .filter(|tool| tool.execution == ToolExecution::Deferred)
            .map(|tool| tool.name.clone())
            .collect()
    }

    /// Truncation budgets used when building summary fields. Adapter-
    /// local enforcement of the wire docs.
    pub const MAX_ARGS_SUMMARY_BYTES: usize = 1024;
    pub const MAX_RESULT_SUMMARY_BYTES: usize = 4096;
    pub const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;
    /// Cap for a harness-suggested session title. Titles are a short line;
    /// this only bounds a pathological one. Truncated code-point-safely by
    /// `truncate_str`.
    pub const MAX_TITLE_BYTES: usize = 512;
    /// Phase 1c: per-chunk cap for live `AgentMessageChunk` token deltas.
    /// The underlying SSE `text_delta`s are token-batched (tens of bytes
    /// typically), so this only bounds a pathological delta. Kept well
    /// under Postgres' 8000-byte `NOTIFY` payload ceiling (the coordinator
    /// fans chunks cross-replica via `pg_notify`) with room for the JSON
    /// envelope; a clipped chunk is harmless — the durable `AgentMessage`
    /// carries the full text.
    pub const MAX_CHUNK_BYTES: usize = 6 * 1024;

    /// Abnormal-exit diagnostics: how much of `claude`'s stderr to retain
    /// for the crash artifact. Bounded so a chatty child can't grow the
    /// tail without limit — the drain task always reads the pipe, so it
    /// never wedges (the reason stderr was historically `inherit`ed).
    pub const MAX_STDERR_TAIL_LINES: usize = 64;
    pub const MAX_STDERR_TAIL_BYTES: usize = 4096;

    /// Phase 3 (ADR 0030): grace for the in-band `control_request` interrupt.
    /// If claude doesn't abort the turn (emit a `result`) within this window —
    /// a build that ignores the control frame — the engine escalates to
    /// SIGINT so an interrupt can never wedge a run.
    pub const INTERRUPT_GRACE_SECS: u64 = 8;

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
        /// `ENGRAM_TRANSPORT` (vsock) and dials accordingly. Mutually
        /// exclusive with `--connect`. `--vsock-host` is a deprecated
        /// alias kept for back-compat with FC bakes.
        #[arg(long = "port", alias = "vsock-host", env = "ENGRAM_HARNESS_VSOCK_HOST")]
        pub vsock_host: Option<u32>,

        /// Engram session id (from `ENGRAM_SESSION_ID`). Sent in
        /// `HarnessAttach`.
        #[arg(long, env = "ENGRAM_SESSION_ID")]
        pub session_id: SessionId,

        /// ADR 0073: the sandbox half of the attach token (from
        /// `ENGRAM_SANDBOX_ID`, stamped by the backend at spawn).
        /// Required — a harness with no token cannot attach, and
        /// failing at arg-parse is louder and earlier than bouncing
        /// `UnknownBinding` forever.
        #[arg(long, env = "ENGRAM_SANDBOX_ID")]
        pub sandbox_id: engram_core::SandboxId,

        /// ADR 0073: the binding-generation half of the attach token
        /// (from `ENGRAM_BINDING_EPOCH`, minted coordinator-side).
        #[arg(long, env = "ENGRAM_BINDING_EPOCH")]
        pub binding_epoch: u64,

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

        /// Override the hook socket bind path. Production always uses the
        /// fixed per-session in-VM path (`HOOK_SOCK_FILE`); this is a test
        /// seam so a `run_engine` test can bind an isolated socket and fire
        /// hooks against it without colliding with the shared production
        /// path (parallel tests, nextest's process-per-test). Not a CLI arg.
        #[arg(skip)]
        pub hook_sock_path: Option<String>,

        /// Test seam for the long-lived MCP bridge socket. Production uses
        /// `MCP_SOCK_FILE`; fake-engine tests bind an isolated temp path.
        #[arg(skip)]
        pub mcp_sock_path: Option<String>,

        /// Parsed once from `ENGRAM_TOOLS` by the main harness process. Not a
        /// clap argument; tests inject a fixture directly.
        #[arg(skip)]
        pub tool_manifest: ToolManifest,
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
        cli.tool_manifest = manifest_from_env();
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

        // CLI dispatch: TCP loopback (Process backend) vs in-VM vsock
        // transport (FC + VZ, selected by ENGRAM_TRANSPORT).
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
        let engram_harness_sdk::Channels {
            command_tx,
            command_rx,
            event_tx,
            event_rx,
            reattach,
        } = engram_harness_sdk::Channels::new();
        // Issue #535 (d): no more env-seeded initial prompt — every prompt,
        // first or follow-up, arrives as a `HarnessCommand::Prompt` frame
        // over the same connection this loop dials. The engine starts with
        // an empty pending queue and waits.

        let engine = tokio::spawn(run_engine(
            cli.clone(),
            command_rx,
            reattach.clone(),
            event_tx,
        ));
        engram_harness_sdk::serve(
            engram_harness_sdk::ConnectionConfig {
                connect: cli.connect,
                port: cli.vsock_host,
                session_id: cli.session_id,
                sandbox_id: cli.sandbox_id,
                binding_epoch: cli.binding_epoch,
                harness_version: format!("engram-harness-claude/{}", env!("CARGO_PKG_VERSION")),
            },
            engine,
            command_tx,
            event_rx,
            reattach,
        )
        .await
    }

    /// The single event a connection pulled from the engine but hadn't
    /// finished writing when it dropped. Re-sent first on the next
    /// connection so a transient drop never loses an event. Only ever
    /// touched by `pump_events` (one connection at a time), under a sync
    /// lock so there's no await between pulling an event and parking it.
    #[cfg(test)]
    type HeldEvent = Arc<Mutex<Option<HarnessEvent>>>;

    /// Outcome of one host connection's life.
    #[cfg(test)]
    enum ConnOutcome {
        /// The engine finished (Shutdown / all command senders gone).
        /// Stop reconnecting and reap the engine's exit code.
        EngineDone,
        /// Host rejected the attach — typically "no sandbox bound to this
        /// session_id" while the host-agent is mid-reattach after a roll /
        /// restart and hasn't repopulated its session→sandbox map yet.
        /// TRANSIENT, not fatal: the binding returns once the host finishes
        /// reattaching, so the loop backs off and retries. (Regression:
        /// session b9b28452 — exiting here orphaned the agent across a
        /// deploy roll; the VM survived but the harness died and the session
        /// wedged `active` forever.)
        Rejected { reason: String },
        /// ADR 0073: host rejected the attach `Superseded` — a newer
        /// binding generation owns this session. FATAL by design:
        /// exit 0; retrying can never succeed.
        Superseded,
        /// Handshake didn't complete (transport flake at/ before attach).
        HandshakeFailed { reason: &'static str },
        /// An established connection later dropped — re-dial.
        Dropped { reason: &'static str },
    }

    /// What the reconnect loop does after one connection's outcome.
    /// Extracted so the retry policy is unit-testable in isolation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[cfg(test)]
    enum Reconnect {
        /// Engine finished — end the loop and reap its exit code.
        Stop,
        /// An established link dropped — settle briefly, reset the counter.
        Settle,
        /// Couldn't reach/attach the host (transport flake, handshake race,
        /// OR an attach rejection) — exponential backoff, retry forever.
        Backoff,
    }

    #[cfg(test)]
    impl ConnOutcome {
        /// The reconnect decision for this outcome. The load-bearing
        /// invariant: ONLY `EngineDone` stops the loop. An attach `Rejected`
        /// — e.g. "no sandbox bound to this session_id" during a host roll —
        /// backs off and retries, because the harness is the session's sole
        /// event channel and giving up strands it (session b9b28452).
        fn reconnect(&self) -> Reconnect {
            match self {
                ConnOutcome::EngineDone => Reconnect::Stop,
                // ADR 0073: superseded = a newer generation owns the
                // session. The ONLY-EngineDone-stops invariant gains its
                // one deliberate exception: this rejection is typed and
                // deterministic (never a transient roll window, which
                // rejects UnknownBinding instead), so exiting cannot
                // strand a session the way the b9b28452 string-matched
                // exit did.
                ConnOutcome::Superseded => Reconnect::Stop,
                ConnOutcome::Dropped { .. } => Reconnect::Settle,
                ConnOutcome::HandshakeFailed { .. } | ConnOutcome::Rejected { .. } => {
                    Reconnect::Backoff
                }
            }
        }

        /// Human-readable cause for the reconnect log line.
        fn reason(&self) -> &str {
            match self {
                ConnOutcome::EngineDone => "engine_done",
                ConnOutcome::Superseded => "superseded",
                ConnOutcome::Rejected { reason } => reason,
                ConnOutcome::HandshakeFailed { reason } | ConnOutcome::Dropped { reason } => reason,
            }
        }
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

    /// ADR 0054 Flavor B: the hook↔harness socket server and its shared
    /// state. The harness is a long-lived **server**; each `PreToolUse`
    /// hook invocation (the `hook-bridge` subcommand) is a transient
    /// **client**. The runtime process tree is three deep —
    /// `engram-harness-claude` → `claude` → a per-PreToolUse hook — and the
    /// hook's stdio is claimed by claude, so a unix socket is the only
    /// out-of-band channel from the hook back to the harness (its
    /// grandparent). NDJSON, one line each way; the connection is never
    /// held open.
    pub mod hook_server {
        use super::{
            emit, injected_tools, mcp_server, BufReader, HarnessEvent, ToolExecution, ToolManifest,
        };
        use engram_harness_proto::{Answers, Question};
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use tokio::net::{UnixListener, UnixStream};
        use tokio::sync::{mpsc, Mutex};

        /// `tool_use_id → canonical Answers`, owned by `run_engine` so it
        /// survives a claude respawn: an answer is stashed by the live
        /// process's `AnswerQuestion` arm and consumed by the hook of the
        /// *next*, resumed process (findings #11/#12 — the answer can only
        /// land on the resumed re-fire). `tokio::sync::Mutex` (not std)
        /// because the accept handler holds it across an `.await`, and it is
        /// touched concurrently by the accept task and the `cmd_rx` arm.
        pub type AnswersInHand = Arc<Mutex<HashMap<String, Answers>>>;
        /// The most-recently-started turn's `run_id`, so a hook (which only
        /// ever fires inside an in-flight turn) can tag its
        /// `UserQuestion`/`QuestionAnswered` events. Set by
        /// `start_turn`/`start_continuation_turn`.
        pub type CurrentRunId = Arc<Mutex<Option<String>>>;
        /// ADR 0054: SESSION-level "an AskUserQuestion card is awaiting an
        /// answer right now." Set true when the hook cards the first AUQ;
        /// cleared when that answer is delivered. While true, EVERY further AUQ
        /// — same run or a later one (the #64389 double-fire fires both ways) —
        /// is a duplicate: no card, recorded in `DuplicateDeferredIds` for the scrub.
        /// Session-scoped (not per-run) so a cross-run sibling is still caught.
        pub type QuestionOutstanding = Arc<Mutex<bool>>;
        /// ADR 0054 / 0089: `tool_use_id`s of duplicate deferred calls the hook
        /// suppressed (AUQ or manifest tool).
        /// Drained at the answer-resume and handed to `scrub_transcript`, which
        /// deletes each duplicate's `tool_use` from claude's transcript so the
        /// resume re-fires exactly one logical call. Owned by `run_engine` so
        /// the hook (which populates it) and the scrub (which drains it) share it.
        pub type DuplicateDeferredIds = Arc<Mutex<HashSet<String>>>;
        /// Deferred generic calls awaiting a host result, tagged with the turn
        /// and tool name. Turn + name identify #64389 double-fires; every
        /// result delivery resumes through the durable transcript regardless
        /// of whether the process that parked the call is still alive.
        pub type DeferredCalls = Arc<Mutex<HashMap<String, DeferredCall>>>;
        pub struct DeferredCall {
            pub run_id: String,
            pub tool_name: String,
        }

        /// Session-scoped state shared by every transient hook connection.
        /// Bundling it makes the ownership boundary explicit and avoids a
        /// positional argument list that would be easy to mis-wire as generic
        /// tool state grows alongside the older AUQ state.
        #[derive(Clone)]
        pub struct State {
            pub answers_in_hand: AnswersInHand,
            pub current_run_id: CurrentRunId,
            pub evt_tx: mpsc::Sender<HarnessEvent>,
            pub question_outstanding: QuestionOutstanding,
            pub duplicate_deferred_ids: DuplicateDeferredIds,
            pub manifest: Arc<ToolManifest>,
            pub results_in_hand: mcp_server::ResultsInHand,
            pub deferred_calls: DeferredCalls,
        }

        /// hook → harness (request). One line.
        #[derive(serde::Deserialize)]
        pub struct HookRequest {
            pub tool_use_id: String,
            #[serde(default)]
            pub tool_name: String,
            #[serde(default)]
            pub tool_input: serde_json::Value,
            #[serde(default)]
            pub questions: Vec<Question>,
        }

        /// harness → hook (verdict). One line. `#[serde(tag = "verdict")]`
        /// gives the exact `{"verdict":"answer","answers":{…}}` /
        /// `{"verdict":"defer"}` wire shape. Shared by both ends (compiled
        /// once → client and server can never be a version apart).
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(tag = "verdict", rename_all = "snake_case")]
        pub enum HookVerdict {
            Answer { answers: Answers },
            Defer,
            Allow,
        }

        /// Accept loop: one transient hook client per connection, each
        /// handled on its own task so parallel AUQ fires (distinct
        /// `tool_use_id`s) never block one another. Runs for the whole
        /// engine lifetime (outlives any single claude process).
        pub async fn serve(listener: UnixListener, state: State) {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(handle_one(stream, state.clone()));
                    }
                    Err(e) => tracing::warn!(error = %e, "hook socket accept failed"),
                }
            }
        }

        async fn handle_one(stream: UnixStream, state: State) {
            let State {
                answers_in_hand,
                current_run_id,
                evt_tx,
                question_outstanding,
                duplicate_deferred_ids,
                manifest,
                results_in_hand,
                deferred_calls,
            } = state;
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let line = match lines.next_line().await {
                Ok(Some(l)) => l,
                _ => return,
            };
            let req: HookRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "malformed hook request; ignoring");
                    return;
                }
            };
            let run_id = current_run_id.lock().await.clone().unwrap_or_default();

            // Before ADR 0089 only AUQ reached this socket, so old requests
            // omitted `tool_name`. Treat an empty name as AUQ to keep that
            // one-line protocol byte-compatible while new prefixed MCP tools
            // carry their full hook payload.
            let is_auq = req.tool_name.is_empty() || req.tool_name == "AskUserQuestion";
            if !is_auq {
                let mut event = None;
                let verdict = if req.tool_name == "ToolSearch" {
                    // Claude lazily loads MCP tools through ToolSearch; parking
                    // this built-in would prevent the real tool from ever
                    // being proposed (ADR 0089 spike).
                    HookVerdict::Allow
                } else if let Some(name) = req.tool_name.strip_prefix("mcp__engrams__") {
                    match injected_tools(&manifest).find(|tool| tool.name == name) {
                        Some(tool) if tool.execution == ToolExecution::Sync => HookVerdict::Allow,
                        Some(_) if results_in_hand.lock().await.contains_key(&req.tool_use_id) => {
                            // The hook must leave the stash intact: after allow,
                            // Claude invokes the MCP bridge, which consumes it.
                            HookVerdict::Allow
                        }
                        Some(_) => {
                            // Claude #64389 can issue the same logical deferred
                            // call twice with distinct ids in one turn. Decide
                            // and insert under one lock so concurrent hook
                            // connections cannot both become host-visible.
                            // An exact-id re-fire remains ordinary idempotency:
                            // never scrub the original id.
                            let (first_request, duplicate) = {
                                let mut calls = deferred_calls.lock().await;
                                if calls.contains_key(&req.tool_use_id) {
                                    (false, false)
                                } else if calls
                                    .values()
                                    .any(|call| call.run_id == run_id && call.tool_name == name)
                                {
                                    (false, true)
                                } else {
                                    calls.insert(
                                        req.tool_use_id.clone(),
                                        DeferredCall {
                                            run_id: run_id.clone(),
                                            tool_name: name.to_string(),
                                        },
                                    );
                                    (true, false)
                                }
                            };
                            if duplicate {
                                duplicate_deferred_ids
                                    .lock()
                                    .await
                                    .insert(req.tool_use_id.clone());
                                tracing::warn!(
                                    %run_id,
                                    tool_use_id = %req.tool_use_id,
                                    tool_name = %name,
                                    "ADR 0089: duplicate deferred manifest tool in one turn \
                                     (#64389 double-fire); deferring with no request \
                                     (scrubbed before resume)"
                                );
                            }
                            if first_request {
                                event = Some(HarnessEvent::ToolCallRequested {
                                    run_id,
                                    call_id: req.tool_use_id.clone(),
                                    name: name.to_string(),
                                    args_json: req.tool_input.to_string(),
                                });
                            }
                            HookVerdict::Defer
                        }
                        None => HookVerdict::Allow,
                    }
                } else {
                    HookVerdict::Allow
                };
                if let Some(event) = event {
                    emit(&evt_tx, event).await;
                }
                write_verdict(&mut w, &verdict).await;
                return;
            }

            // Verdict (+ optional event):
            //   answer-in-hand → answer + QuestionAnswered (consume with
            //     `remove`, so a re-fire of the SAME id after consumption
            //     defers — one tool_result, structural idempotency, ADR 0054).
            //     Clear `question_outstanding`: this card is now answered, so
            //     the NEXT genuine question is carded afresh.
            //   else, no question outstanding → defer + UserQuestion (card),
            //     mark a question outstanding.
            //   else (a question is ALREADY outstanding) → defer with NO card —
            //     the #64389 stdin double-fire, WITHIN this run or a LATER one
            //     (claude, held open on stdin, re-asks either way). We DEFER
            //     rather than deny: a deny leaves a "stop and wait" tool_result
            //     in claude's transcript that the model retries after the real
            //     answer lands (re-ask + a second card). The duplicate stays a
            //     parked tool; its id goes to `duplicate_deferred_ids` so the
            //     answer-resume scrub drops it, leaving exactly one question.
            // The answers-map lock is a temporary (dropped before the
            // outstanding lock — no lock-order coupling), and the event is
            // emitted AFTER the locks drop (`emit` can block on host-link
            // backpressure).
            let (verdict, event): (HookVerdict, Option<HarnessEvent>) = {
                let answer = answers_in_hand.lock().await.remove(&req.tool_use_id);
                match answer {
                    Some(answers) => {
                        *question_outstanding.lock().await = false;
                        (
                            HookVerdict::Answer {
                                answers: answers.clone(),
                            },
                            Some(HarnessEvent::QuestionAnswered {
                                run_id,
                                tool_call_id: req.tool_use_id,
                                answers,
                            }),
                        )
                    }
                    None => {
                        // Atomic check-and-set under one lock: the first AUQ
                        // since the last answer flips the flag and is carded;
                        // any AUQ while it is already set is the duplicate.
                        let duplicate = {
                            let mut outstanding = question_outstanding.lock().await;
                            let was_set = *outstanding;
                            *outstanding = true;
                            was_set
                        };
                        if duplicate {
                            duplicate_deferred_ids
                                .lock()
                                .await
                                .insert(req.tool_use_id.clone());
                            tracing::warn!(
                                %run_id,
                                tool_use_id = %req.tool_use_id,
                                "ADR 0054: duplicate AskUserQuestion while one is outstanding \
                                 (#64389 double-fire); deferring with no card (scrubbed before resume)"
                            );
                            // Defer (keep it parked) but emit no card.
                            (HookVerdict::Defer, None)
                        } else {
                            (
                                HookVerdict::Defer,
                                Some(HarnessEvent::UserQuestion {
                                    run_id,
                                    tool_call_id: req.tool_use_id,
                                    questions: req.questions,
                                }),
                            )
                        }
                    }
                }
            };
            if let Some(event) = event {
                emit(&evt_tx, event).await;
            }

            write_verdict(&mut w, &verdict).await;
            // drop closes the connection; the hook reads its one line and exits.
        }

        async fn write_verdict<W: tokio::io::AsyncWrite + Unpin>(
            writer: &mut W,
            verdict: &HookVerdict,
        ) {
            let mut out = serde_json::to_string(verdict)
                .unwrap_or_else(|_| r#"{"verdict":"defer"}"#.to_string());
            out.push('\n');
            let _ = writer.write_all(out.as_bytes()).await;
            let _ = writer.flush().await;
        }
    }

    /// ADR 0054 Flavor B: the `engram-harness-claude hook-bridge`
    /// subcommand — the transient `PreToolUse` hook client. Re-invoking the
    /// harness binary as its own hook (git/busybox style) means one
    /// artifact, no extra guest runtime (no node/jq), and no protocol drift
    /// (client & server share `hook_server`'s serde types). Reads the
    /// PreToolUse payload on stdin, decides, writes claude's hook output on
    /// stdout.
    pub mod hook_bridge {
        use super::hook_server::HookVerdict;
        use super::BufReader;
        use engram_harness_proto::{Answers, Question};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        pub async fn run() -> std::process::ExitCode {
            let mut input = String::new();
            if tokio::io::stdin().read_to_string(&mut input).await.is_err() {
                print_allow();
                return std::process::ExitCode::SUCCESS;
            }
            let v: serde_json::Value =
                serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
            let tool_name = v.get("tool_name").and_then(|s| s.as_str()).unwrap_or("");

            // Ordinary built-ins (including ToolSearch) → allow. Manifest MCP
            // tools MUST round-trip so the main process can apply their sync /
            // deferred policy and emit the generic request event.
            // This remains the
            // `--dangerously-skip-permissions` replacement: the VM is still
            // the safety boundary, but the decision is now explicit and
            // auditable.
            let is_manifest_mcp = tool_name.starts_with("mcp__engrams__");
            if tool_name != "AskUserQuestion" && !is_manifest_mcp {
                print_allow();
                return std::process::ExitCode::SUCCESS;
            }

            let tool_use_id = v
                .get("tool_use_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let tool_input = v
                .get("tool_input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // Parse into the SHARED `Question` type so we keep `multi_select`
            // per question for denormalization (its `#[serde(rename)]` makes
            // this byte-faithful to claude's `tool_input.questions`).
            let questions: Vec<Question> = tool_input
                .get("questions")
                .and_then(|q| serde_json::from_value(q.clone()).ok())
                .unwrap_or_default();
            let sock = std::env::var("ENGRAM_HOOK_SOCK").unwrap_or_default();

            match round_trip(&sock, &tool_use_id, tool_name, &tool_input, &questions).await {
                Some(HookVerdict::Answer { answers }) => {
                    // Denormalize canonical Vec<String> → claude's
                    // updatedInput.answers: a bare string for single-select,
                    // an array for multiSelect (finding #6), using each
                    // question's multi_select flag. The claude arity quirk
                    // stays isolated here. claude wants the full updatedInput
                    // echoed (questions + answers), so graft `answers` onto
                    // the original tool_input.
                    let updated_answers = denormalize(&questions, &answers);
                    let mut updated_input = tool_input;
                    match updated_input {
                        serde_json::Value::Object(ref mut map) => {
                            map.insert("answers".to_string(), updated_answers);
                        }
                        _ => {
                            updated_input = serde_json::json!({
                                "questions": questions_to_json(&questions),
                                "answers": updated_answers,
                            });
                        }
                    }
                    let out = serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "allow",
                            "updatedInput": updated_input,
                        }
                    });
                    println!("{out}");
                }
                Some(HookVerdict::Allow) => print_allow(),
                // Defer verdict (incl. a no-card duplicate, ADR 0054), or any
                // socket failure → defer. The turn ends `tool_deferred` and the
                // VM can idle-evict.
                Some(HookVerdict::Defer) | None => print_defer(),
            }
            std::process::ExitCode::SUCCESS
        }

        fn print_allow() {
            println!(
                r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"allow"}}}}"#
            );
        }
        fn print_defer() {
            println!(
                r#"{{"hookSpecificOutput":{{"hookEventName":"PreToolUse","permissionDecision":"defer"}}}}"#
            );
        }

        async fn round_trip(
            sock: &str,
            tool_use_id: &str,
            tool_name: &str,
            tool_input: &serde_json::Value,
            questions: &[Question],
        ) -> Option<HookVerdict> {
            if sock.is_empty() {
                return None;
            }
            let stream = UnixStream::connect(sock).await.ok()?;
            let (r, mut w) = stream.into_split();
            let req = serde_json::json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "tool_input": tool_input,
                "questions": questions,
            });
            let mut line = serde_json::to_string(&req).ok()?;
            line.push('\n');
            w.write_all(line.as_bytes()).await.ok()?;
            w.flush().await.ok()?;
            let mut lines = BufReader::new(r).lines();
            let resp = lines.next_line().await.ok()??;
            serde_json::from_str(&resp).ok()
        }

        /// Build claude's `updatedInput.answers` map. Keyed by question text
        /// (finding #8); value is a bare string (single-select) or an array
        /// (multiSelect) per the paired `Question.multi_select`. `pub` for
        /// unit testing — the binary has no external API to keep narrow.
        pub fn denormalize(questions: &[Question], answers: &Answers) -> serde_json::Value {
            let mut out = serde_json::Map::new();
            for q in questions {
                if let Some(labels) = answers.get(&q.question) {
                    let val = if q.multi_select {
                        serde_json::Value::Array(
                            labels
                                .iter()
                                .cloned()
                                .map(serde_json::Value::String)
                                .collect(),
                        )
                    } else {
                        serde_json::Value::String(labels.first().cloned().unwrap_or_default())
                    };
                    out.insert(q.question.clone(), val);
                }
            }
            serde_json::Value::Object(out)
        }

        fn questions_to_json(questions: &[Question]) -> serde_json::Value {
            serde_json::to_value(questions).unwrap_or(serde_json::Value::Null)
        }
    }

    /// ADR 0089 P2: disposable stdio MCP frontend. Claude owns this process's
    /// lifetime, so it keeps no correlation state: initialize and tools/list
    /// are answered from the inherited manifest, while each tools/call blocks
    /// on the long-lived harness main process over a one-line unix-socket
    /// request/response.
    pub mod mcp_bridge {
        use super::{injected_tools, manifest_from_env, BufReader, ToolManifest, MCP_SOCK_FILE};
        use serde::{Deserialize, Serialize};
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
        use tokio::net::UnixStream;

        #[derive(Serialize)]
        struct McpCallRequest<'a> {
            name: &'a str,
            args: Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            tool_use_id: Option<&'a str>,
        }

        #[derive(Deserialize)]
        struct McpCallResponse {
            result_json: String,
        }

        pub async fn run() -> std::process::ExitCode {
            let manifest = manifest_from_env();
            let socket_path =
                std::env::var("ENGRAM_MCP_SOCK").unwrap_or_else(|_| MCP_SOCK_FILE.to_string());
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            let mut stdout = tokio::io::stdout();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => return std::process::ExitCode::SUCCESS,
                    Err(_) => return std::process::ExitCode::from(1),
                };
                let request: Value = match serde_json::from_str(&line) {
                    Ok(request) => request,
                    Err(_) => {
                        let response = json_rpc_error(Value::Null, -32700, "Parse error");
                        if write_response(&mut stdout, &response).await.is_err() {
                            return std::process::ExitCode::from(1);
                        }
                        continue;
                    }
                };
                if let Some(response) = handle_request(request, &manifest, &socket_path).await {
                    if write_response(&mut stdout, &response).await.is_err() {
                        return std::process::ExitCode::from(1);
                    }
                }
            }
        }

        async fn write_response<W: AsyncWrite + Unpin>(
            writer: &mut W,
            response: &Value,
        ) -> std::io::Result<()> {
            let mut line = serde_json::to_vec(response)
                .expect("JSON-RPC response contains only serializable values");
            line.push(b'\n');
            writer.write_all(&line).await?;
            writer.flush().await
        }

        pub async fn handle_request(
            request: Value,
            manifest: &ToolManifest,
            socket_path: &str,
        ) -> Option<Value> {
            let method = request.get("method").and_then(Value::as_str).unwrap_or("");
            // MCP lifecycle notifications (`notifications/initialized`,
            // cancellation, progress) deliberately have no response.
            let id = request.get("id")?.clone();
            match method {
                "initialize" => {
                    let protocol_version = request
                        .pointer("/params/protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or("2025-06-18");
                    Some(json_rpc_result(
                        id,
                        serde_json::json!({
                            "protocolVersion": protocol_version,
                            "capabilities": {"tools": {"listChanged": false}},
                            "serverInfo": {
                                "name": "engrams",
                                "version": env!("CARGO_PKG_VERSION")
                            }
                        }),
                    ))
                }
                "tools/list" => {
                    let tools: Vec<Value> = injected_tools(manifest)
                        .map(|tool| {
                            serde_json::json!({
                                "name": tool.name,
                                "description": tool.description,
                                "inputSchema": tool.input_schema,
                            })
                        })
                        .collect();
                    Some(json_rpc_result(id, serde_json::json!({"tools": tools})))
                }
                "tools/call" => {
                    let Some(name) = request.pointer("/params/name").and_then(Value::as_str) else {
                        return Some(json_rpc_error(id, -32602, "Missing tool name"));
                    };
                    let Some(tool) = injected_tools(manifest).find(|tool| tool.name == name) else {
                        return Some(json_rpc_error(id, -32602, "Unknown tool"));
                    };
                    tracing::debug!(tool = %tool.name, execution = ?tool.execution, "forwarding MCP tool call");
                    let args = request
                        .pointer("/params/arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    let tool_use_id = request
                        .pointer("/params/_meta/claudecode~1toolUseId")
                        .and_then(Value::as_str);
                    match round_trip(socket_path, name, args, tool_use_id).await {
                        Ok(result_json) => Some(json_rpc_result(
                            id,
                            serde_json::json!({
                                "content": [{"type": "text", "text": result_json}]
                            }),
                        )),
                        Err(e) => Some(json_rpc_error(
                            id,
                            -32603,
                            &format!("Engram tool bridge failed: {e}"),
                        )),
                    }
                }
                _ => Some(json_rpc_error(id, -32601, "Method not found")),
            }
        }

        async fn round_trip(
            socket_path: &str,
            name: &str,
            args: Value,
            tool_use_id: Option<&str>,
        ) -> std::io::Result<String> {
            let stream = UnixStream::connect(socket_path).await?;
            let (r, mut w) = stream.into_split();
            let request = McpCallRequest {
                name,
                args,
                tool_use_id,
            };
            let mut line = serde_json::to_vec(&request)
                .expect("MCP call request contains only serializable values");
            line.push(b'\n');
            w.write_all(&line).await?;
            w.flush().await?;
            let response = BufReader::new(r)
                .lines()
                .next_line()
                .await?
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
            serde_json::from_str::<McpCallResponse>(&response)
                .map(|response| response.result_json)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }

        fn json_rpc_result(id: Value, result: Value) -> Value {
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
        }

        fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message}
            })
        }
    }

    /// ADR 0089 P2: long-lived MCP server half owned by `run_engine`. Each
    /// bridge connection represents one blocking MCP tools/call. A result
    /// already in hand is consumed immediately (the deferred re-fire path);
    /// otherwise the connection is parked in a oneshot table and the generic
    /// request event is emitted exactly once.
    pub mod mcp_server {
        use super::{emit, hook_server, BufReader, HarnessEvent};
        use serde::{Deserialize, Serialize};
        use serde_json::Value;
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        use tokio::net::{UnixListener, UnixStream};
        use tokio::sync::{mpsc, oneshot, Mutex};

        pub type ResultsInHand = Arc<Mutex<HashMap<String, String>>>;
        pub type ParkedCalls = Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>;

        #[derive(Clone)]
        pub struct State {
            pub results_in_hand: ResultsInHand,
            pub parked_calls: ParkedCalls,
            pub deferred_calls: hook_server::DeferredCalls,
            pub current_run_id: hook_server::CurrentRunId,
            pub evt_tx: mpsc::Sender<HarnessEvent>,
        }

        #[derive(Deserialize)]
        struct McpCallRequest {
            name: String,
            #[serde(default = "empty_object")]
            args: Value,
            tool_use_id: Option<String>,
        }

        #[derive(Serialize)]
        struct McpCallResponse {
            result_json: String,
        }

        fn empty_object() -> Value {
            serde_json::json!({})
        }

        pub async fn serve(listener: UnixListener, state: State) {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(handle_one(stream, state.clone()));
                    }
                    Err(e) => tracing::warn!(error = %e, "MCP socket accept failed"),
                }
            }
        }

        async fn handle_one(stream: UnixStream, state: State) {
            let State {
                results_in_hand,
                parked_calls,
                deferred_calls,
                current_run_id,
                evt_tx,
            } = state;
            let (r, mut w) = stream.into_split();
            let Some(line) = BufReader::new(r).lines().next_line().await.ok().flatten() else {
                return;
            };
            let request: McpCallRequest = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(e) => {
                    tracing::warn!(error = %e, "malformed MCP bridge request");
                    return;
                }
            };
            let call_id = request
                .tool_use_id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("toolu-engram-{}", uuid::Uuid::new_v4()));

            // Both this insertion and `route_tool_result` lock parked→results.
            // Holding the parked lock across the stash check closes the only
            // lost-wakeup window: a result can neither slip between the check
            // and insertion nor be stashed while a sender is already parked.
            let rx = {
                let mut parked = parked_calls.lock().await;
                if let Some(result_json) = results_in_hand.lock().await.remove(&call_id) {
                    deferred_calls.lock().await.remove(&call_id);
                    drop(parked);
                    write_result(&mut w, result_json).await;
                    return;
                }
                let (tx, rx) = oneshot::channel();
                parked.insert(call_id.clone(), tx);
                rx
            };

            let run_id = current_run_id.lock().await.clone().unwrap_or_default();
            emit(
                &evt_tx,
                HarnessEvent::ToolCallRequested {
                    run_id,
                    call_id: call_id.clone(),
                    name: request.name,
                    args_json: request.args.to_string(),
                },
            )
            .await;

            match rx.await {
                Ok(result_json) => write_result(&mut w, result_json).await,
                Err(_) => tracing::debug!(%call_id, "parked MCP call was cancelled"),
            }
        }

        async fn write_result<W: tokio::io::AsyncWrite + Unpin>(
            writer: &mut W,
            result_json: String,
        ) {
            let response = McpCallResponse { result_json };
            let mut line = serde_json::to_vec(&response)
                .expect("MCP result response contains only serializable values");
            line.push(b'\n');
            let _ = writer.write_all(&line).await;
            let _ = writer.flush().await;
        }

        /// Route a host result to a currently parked bridge, or durably stash
        /// it for a later id-stable re-fire. Returns true only when a live
        /// parked connection accepted the result.
        pub async fn route_tool_result(
            call_id: &str,
            result_json: String,
            parked_calls: &ParkedCalls,
            results_in_hand: &ResultsInHand,
        ) -> bool {
            let mut parked = parked_calls.lock().await;
            if let Some(sender) = parked.remove(call_id) {
                match sender.send(result_json) {
                    Ok(()) => return true,
                    Err(result_json) => {
                        results_in_hand
                            .lock()
                            .await
                            .insert(call_id.to_string(), result_json);
                        return false;
                    }
                }
            }
            results_in_hand
                .lock()
                .await
                .insert(call_id.to_string(), result_json);
            false
        }
    }

    /// Aborts the hook-socket accept task and unlinks the socket file when
    /// `run_engine` returns (any path). The VM teardown reclaims the tmpfs
    /// anyway; this matters for the ProcessBackend (dev), where the harness
    /// process exits but the host persists — a stale bind would otherwise
    /// `EADDRINUSE` the next harness.
    struct SockGuard {
        path: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for SockGuard {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// ADR 0054 Flavor B: generate the `--settings` file claude is launched
    /// with — a `PreToolUse` command hook that re-invokes THIS binary as
    /// `hook-bridge`. Written at startup (not baked) from `current_exe()`
    /// so the hook command is always the path of the running binary,
    /// correct across FC / VZ / Process (where `resolve_claude_bin`'s
    /// sibling layout — and thus this binary's path — varies).
    async fn write_hook_settings() {
        let self_exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "engram-harness-claude".to_string());
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "*",
                    "hooks": [{ "type": "command", "command": format!("{self_exe} hook-bridge") }]
                }]
            }
        });
        let _ = tokio::fs::create_dir_all("/workspace/.engram").await;
        if let Err(e) = tokio::fs::write(HOOK_SETTINGS_FILE, settings.to_string()).await {
            tracing::warn!(error = %e, "couldn't write claude hook settings");
        }
    }

    /// Write claude's strict MCP config when the manifest contains at least
    /// one tool that is not natively bound to Claude. Removing a stale file on
    /// the empty path matters on ProcessBackend, where `/workspace` can
    /// survive a harness restart with a different manifest.
    async fn write_mcp_config(
        path: &Path,
        manifest: &ToolManifest,
        self_exe: &Path,
    ) -> std::io::Result<bool> {
        if injected_tools(manifest).next().is_none() {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            return Ok(false);
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let config = serde_json::json!({
            "mcpServers": {
                "engrams": {
                    "type": "stdio",
                    "command": self_exe.to_string_lossy(),
                    "args": ["mcp-bridge"]
                }
            }
        });
        tokio::fs::write(
            path,
            serde_json::to_vec(&config).expect("MCP config contains only serializable values"),
        )
        .await?;
        Ok(true)
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
    ) -> ExitCode {
        // The pending queue is owned here so it survives a respawn: a
        // prompt queued mid-turn (type-ahead) or one that hadn't started
        // when claude crashed isn't lost across the process restart.
        // Phase 1b: this IS the user-visible editable queue — each entry
        // carries its `prompt_id`. Issue #535 (d): starts EMPTY — the
        // create-time initial prompt is no longer env-seeded; it arrives as
        // an ordinary `HarnessCommand::Prompt` frame once the host attaches,
        // same as every follow-up.
        let mut pending: VecDeque<QueuedPrompt> = VecDeque::new();

        // ADR 0052: prompt_ids we've already accepted (started or queued).
        // The host re-delivers un-confirmed prompts on every reattach
        // (command-side at-least-once); this dedupes a replay of one we
        // already have so it can't double-run. Owned here so it survives a
        // claude respawn (a re-delivery racing the respawn is still caught).
        let mut seen_prompt_ids: HashSet<String> = HashSet::new();

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

        // ADR 0054 / 0089: the answer + generic result bridges. Their stashes
        // and `current_run_id` are owned here so they survive a claude respawn
        // (the answer is stashed by one process and consumed by the hook of
        // the next, resumed one — findings #11/#12). The hook socket is
        // bound ONCE, before the first spawn (bind-before-spawn → no hook
        // can fire before the listener exists), and shared across respawns;
        // the accept task outlives any single claude process.
        let answers_in_hand: hook_server::AnswersInHand =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let results_in_hand: mcp_server::ResultsInHand =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let parked_mcp_calls: mcp_server::ParkedCalls =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let deferred_calls: hook_server::DeferredCalls =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let tool_manifest = Arc::new(cli.tool_manifest.clone());
        let current_run_id: hook_server::CurrentRunId = Arc::new(tokio::sync::Mutex::new(None));
        // ADR 0054 Part C: narrate-past message-ids to scrub from claude's
        // transcript at the next deferred-delivery resume. Owned here so it
        // survives a respawn (populated during the defer turn, drained at
        // ResumeForDeferred).
        let scrub_msg_ids: Arc<tokio::sync::Mutex<HashSet<String>>> =
            Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        // ADR 0054: session-level AskUserQuestion dedup of the #64389 double-fire
        // (within- OR cross-run). `question_outstanding` is set when the hook
        // cards a question and cleared when its answer is delivered; while set,
        // every further AUQ is a duplicate with no card. Manifest deferred
        // tools use the same set for same-name double-fires within one turn.
        // `duplicate_deferred_ids` collects every suppressed duplicate's
        // tool_use id so the deferred-delivery scrub deletes it from the
        // transcript. Owned here so all deferred paths span respawns.
        let question_outstanding: hook_server::QuestionOutstanding =
            Arc::new(tokio::sync::Mutex::new(false));
        let duplicate_deferred_ids: hook_server::DuplicateDeferredIds =
            Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        write_hook_settings().await;
        let self_exe =
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("engram-harness-claude"));
        let mcp_config_path = match write_mcp_config(
            Path::new(MCP_CONFIG_FILE),
            &cli.tool_manifest,
            &self_exe,
        )
        .await
        {
            Ok(true) => Some(MCP_CONFIG_FILE),
            Ok(false) => None,
            Err(e) => {
                tracing::error!(error = %e, "couldn't write claude MCP config; exposing no injected tools");
                None
            }
        };
        // Production: the fixed in-VM path. Tests may inject an isolated one.
        let hook_sock_path = cli
            .hook_sock_path
            .clone()
            .unwrap_or_else(|| HOOK_SOCK_FILE.to_string());
        let _ = tokio::fs::remove_file(&hook_sock_path).await; // clear a stale bind
        let _sock_guard = match tokio::net::UnixListener::bind(&hook_sock_path) {
            Ok(listener) => {
                let task = tokio::spawn(hook_server::serve(
                    listener,
                    hook_server::State {
                        answers_in_hand: answers_in_hand.clone(),
                        current_run_id: current_run_id.clone(),
                        evt_tx: evt_tx.clone(),
                        question_outstanding: question_outstanding.clone(),
                        duplicate_deferred_ids: duplicate_deferred_ids.clone(),
                        manifest: tool_manifest.clone(),
                        results_in_hand: results_in_hand.clone(),
                        deferred_calls: deferred_calls.clone(),
                    },
                ));
                Some(SockGuard {
                    path: hook_sock_path.clone(),
                    task,
                })
            }
            Err(e) => {
                // Degraded, not fatal: ordinary built-ins still auto-allow;
                // AUQ and manifest MCP hooks fail closed as defer.
                tracing::error!(error = %e, "bind hook socket failed; deferred tools cannot be delivered");
                None
            }
        };
        let mcp_sock_path = cli
            .mcp_sock_path
            .clone()
            .unwrap_or_else(|| MCP_SOCK_FILE.to_string());
        let _ = tokio::fs::remove_file(&mcp_sock_path).await;
        let _mcp_sock_guard = match tokio::net::UnixListener::bind(&mcp_sock_path) {
            Ok(listener) => {
                let task = tokio::spawn(mcp_server::serve(
                    listener,
                    mcp_server::State {
                        results_in_hand: results_in_hand.clone(),
                        parked_calls: parked_mcp_calls.clone(),
                        deferred_calls: deferred_calls.clone(),
                        current_run_id: current_run_id.clone(),
                        evt_tx: evt_tx.clone(),
                    },
                ));
                Some(SockGuard {
                    path: mcp_sock_path,
                    task,
                })
            }
            Err(e) => {
                tracing::error!(error = %e, "bind MCP socket failed; injected tools will fail");
                None
            }
        };

        loop {
            let spawned_at = Instant::now();
            match run_claude_session(
                &cli,
                &mut cmd_rx,
                &reattach,
                &evt_tx,
                &mut pending,
                &mut seen_prompt_ids,
                &answers_in_hand,
                &results_in_hand,
                &parked_mcp_calls,
                &deferred_calls,
                &current_run_id,
                &scrub_msg_ids,
                &question_outstanding,
                mcp_config_path,
            )
            .await
            {
                // Clean shutdown, or all command senders gone (the
                // connection loop exited = process teardown).
                SessionOutcome::Shutdown | SessionOutcome::ChannelClosed => {
                    return ExitCode::SUCCESS;
                }
                // ADR 0054 / 0089: an intentional deferred-delivery resume —
                // NOT a crash. The command arm stashed the result and SIGINT'd
                // claude; respawn with `--resume` like `Respawn`, but reset
                // the fast-crash budget (proof of liveness) and never sleep.
                // The deferred tool re-fires on startup and is answered.
                SessionOutcome::ResumeForDeferred => {
                    fast_crashes = 0;
                    // ADR 0054 Part C / ADR 0089: claude was intentionally
                    // SIGINT'd and is dead now → its transcript is quiescent and safe
                    // to edit. Remove both poisons so the resumed model sees a
                    // clean defer (one `tool_use`, nothing after) and delivers
                    // the real answer instead of re-asking: (a) narrate-past
                    // assistant text (`scrub_msg_ids`), and (b) duplicate
                    // deferred tool_uses the hook suppressed
                    // (`duplicate_deferred_ids`: AUQ within/cross-run or
                    // manifest tool within-turn #64389 double-fire). Fail-safe:
                    // on any miss/error we skip — the Part B fallback still
                    // delivers the result as a user message.
                    let ids: HashSet<String> = {
                        let mut g = scrub_msg_ids.lock().await;
                        std::mem::take(&mut *g)
                    };
                    let dup_ids: HashSet<String> = {
                        let mut g = duplicate_deferred_ids.lock().await;
                        std::mem::take(&mut *g)
                    };
                    if !ids.is_empty() || !dup_ids.is_empty() {
                        if let Some(sid) = read_claude_session_id().await {
                            match find_claude_transcript(&sid) {
                                Some(tr) => {
                                    let res = tokio::task::spawn_blocking(move || {
                                        scrub_transcript(&tr, &ids, &dup_ids)
                                    })
                                    .await;
                                    match res {
                                        Ok(Ok(n)) => tracing::info!(
                                            removed = n,
                                            %sid,
                                            "ADR 0054 / 0089: scrubbed narrate-past + duplicate deferred calls from transcript before resume"
                                        ),
                                        Ok(Err(e)) => tracing::warn!(
                                            error = %e,
                                            %sid,
                                            "Part C scrub failed; relying on Part B answer-as-message fallback"
                                        ),
                                        Err(e) => tracing::warn!(
                                            error = %e,
                                            %sid,
                                            "Part C scrub task panicked; relying on Part B fallback"
                                        ),
                                    }
                                }
                                None => tracing::warn!(
                                    %sid,
                                    "Part C: claude transcript not found; relying on Part B fallback"
                                ),
                            }
                        }
                    }
                    // loop → respawn with --resume, no backoff
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

    /// Drain engine events to the current connection's writer. The
    /// event being written is parked in `held` *before* the await, so
    /// if this future is cancelled (the read half died) or the write
    /// fails, the event survives and is re-sent on the next connection
    /// — at-least-once delivery across a reconnect, no loss.
    #[cfg(test)]
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
        /// An AUQ answer or generic deferred result is stashed before SIGINT so
        /// the deferred tool re-fires id-stably on `--resume`. A sibling of
        /// `Respawn` that is INTENTIONAL: no fast-crash backoff, no
        /// abnormal-exit System message.
        ResumeForDeferred,
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
        /// Phase 1c: the id of the assistant message currently streaming
        /// (captured from the partial-stream `message_start`), so token
        /// chunks (`AgentMessageChunk`) carry the SAME `message_id` the
        /// terminal `AgentMessage` will use — the UI keys the live bubble
        /// on it and reconciles in place when the durable message lands.
        current_message_id: Option<String>,
        /// ADR 0054 Flavor A: file changes parsed from a `Write`/`Edit`/
        /// `MultiEdit` tool_use, keyed by tool_use_id and held until the
        /// matching `tool_result` — so a `FileChanged` is emitted only on a
        /// SUCCESSFUL result (never a phantom diff for a failed edit). Value
        /// is `(path, change)`.
        pending_file_changes: HashMap<String, (String, FileChange)>,
        /// `(tool_name, started_at)` per in-flight tool_use_id, recorded at
        /// `ToolCallStarted` and consumed at the matching `tool_result` so
        /// `ToolCallCompleted` carries a real `duration_ms` + `tool_name`
        /// (both shipped hardcoded-empty/zero before — 2026-07-11 campaign
        /// papercut: every timing consumer read 0).
        tool_call_starts: HashMap<String, (String, std::time::Instant)>,
        /// Deferred AUQ + manifest tool_use_ids seen this turn that have not
        /// received a `tool_result`. While non-empty, assistant text/chunks are
        /// the narrate-past hallucination (Claude talking past a parked tool)
        /// and are suppressed; delivery clears the id and re-enables output.
        deferred_pending: HashSet<String>,
        /// Manifest names (without Claude's `mcp__engrams__` prefix) whose
        /// hooks park the turn. Kept with the turn so transcript translation
        /// can classify generic tool_use blocks without global/env lookups.
        deferred_tool_names: HashSet<String>,
        /// ADR 0054 Part B: set for a `start_continuation_turn` (answer-resume).
        /// If such a turn ENDS with an answer still in `answers_in_hand`, the
        /// deferred tool was never re-fired — claude narrate-past'd and
        /// abandoned it (a `--resume` does not re-present an abandoned tool).
        /// The engine then delivers the answer as a fresh user message rather
        /// than leaving the question hanging.
        is_delivery_resume: bool,
        /// ADR 0054 Part C: message-ids of the narrate-past assistant messages
        /// suppressed THIS turn (Part A hid them from the UI). Copied into the
        /// session-level scrub set at turn-end so they survive the kill→resume
        /// gap, then removed from claude's transcript before `--resume` (see
        /// `scrub_transcript`).
        suppressed_msg_ids: Vec<String>,
    }

    /// A prompt waiting in the harness-owned queue (Phase 1b — type-ahead
    /// / steering). The harness is the single-writer owner: it buffers
    /// these in memory and writes one to claude's stdin only at the
    /// consumption boundary (the running turn's `result`). `prompt_id`
    /// carries the client/coord-minted id used to correlate the queue
    /// events and the eventual `RunStarted{prompt_id}` — issue #535 (d):
    /// every prompt, first or follow-up, now arrives via `HarnessCommand::
    /// Prompt`, so this is always `Some` in practice (the historical `None`
    /// case was the env-seeded initial prompt, deleted). Kept `Option`
    /// rather than force-unwrapping at each read site — a defensive `None`
    /// costs nothing here and this isn't a wire type.
    struct QueuedPrompt {
        prompt_id: Option<String>,
        text: String,
    }

    /// SIGINT a running `claude` child — the graceful "stop the current
    /// turn" signal (mirrors a Ctrl-C / ESC). Claude flushes its
    /// conversation file per message synchronously, so the session stays
    /// cleanly `--resume`-able after this. We escalate to SIGKILL only as
    /// a grace-timeout fallback in the reap below.
    ///
    /// Phase 3 (ADR 0030): this is now the interrupt FALLBACK only — the
    /// primary path is the in-band `control_request` (`write_control_
    /// interrupt`), which leaves the process alive. SIGINT is used if the
    /// control frame can't be written, or if it isn't honored within
    /// `INTERRUPT_GRACE_SECS` (a claude build lacking the frame).
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
    #[allow(clippy::too_many_arguments)]
    async fn run_claude_session(
        cli: &Cli,
        cmd_rx: &mut mpsc::Receiver<HarnessCommand>,
        reattach: &Arc<Notify>,
        evt_tx: &mpsc::Sender<HarnessEvent>,
        pending: &mut VecDeque<QueuedPrompt>,
        seen_prompt_ids: &mut HashSet<String>,
        answers_in_hand: &hook_server::AnswersInHand,
        results_in_hand: &mcp_server::ResultsInHand,
        parked_mcp_calls: &mcp_server::ParkedCalls,
        deferred_calls: &hook_server::DeferredCalls,
        current_run_id: &hook_server::CurrentRunId,
        // ADR 0054 Part C: session-level set of narrate-past message-ids to
        // scrub from the transcript before the next answer-resume. Owned by
        // `run_engine` so it survives a respawn; populated at turn-end here.
        scrub_msg_ids: &Arc<tokio::sync::Mutex<HashSet<String>>>,
        // ADR 0054: set true while an AskUserQuestion card is awaiting an
        // answer (the hook owns it). While set, a further AUQ is the #64389
        // duplicate — its card is already suppressed at the hook, so we also
        // drop its `ToolCallStarted` here (no phantom tool-call in the UI).
        question_outstanding: &hook_server::QuestionOutstanding,
        mcp_config_path: Option<&str>,
    ) -> SessionOutcome {
        // A socket call outside a turn must carry the wire-mandated empty
        // run_id, never the previous turn's id.
        *current_run_id.lock().await = None;
        let resume_id = read_claude_session_id().await;
        // ADR 0060: ENGRAM_APPEND_SYSTEM_PROMPT (carried via harness_env) flavors
        // the agent's system prompt. Read per spawn — it is constant for the
        // process, and a respawn must re-apply it.
        let append_system_prompt = std::env::var("ENGRAM_APPEND_SYSTEM_PROMPT").ok();
        let argv = build_claude_argv(&resume_id, append_system_prompt.as_deref(), mcp_config_path);
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
            // Claude's root-check: the CLI otherwise refuses to start as
            // root, which is exactly how it runs inside our VM. Kept after
            // ADR 0054 dropped `--dangerously-skip-permissions` — the
            // root-check is independent of that flag, and the VM itself is
            // the security boundary.
            // `ENGRAM_HOOK_SOCK` (ADR 0054): claude inherits it and the
            // PreToolUse hook inherits it from claude (finding #7), so the
            // hook finds the per-session socket while the `--settings`
            // artifact stays path-agnostic.
            .env("BASH_DEFAULT_TIMEOUT_MS", "1800000")
            .env("BASH_MAX_TIMEOUT_MS", "7200000")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("IS_SANDBOX", "1")
            .env(
                "ENGRAM_HOOK_SOCK",
                cli.hook_sock_path.as_deref().unwrap_or(HOOK_SOCK_FILE),
            )
            .env(
                "ENGRAM_MCP_SOCK",
                cli.mcp_sock_path.as_deref().unwrap_or(MCP_SOCK_FILE),
            )
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
        let stderr_task =
            engram_harness_sdk::spawn_stderr_tail(stderr, MAX_STDERR_TAIL_LINES, true);

        let mut turn: Option<TurnState> = None;
        let mut shutting_down = false;
        let mut shutdown_deadline: Option<Instant> = None;
        // Set when an operator Interrupt aborts the current turn; the
        // `result`/reap path closes that run as `RunInterrupted` (not a
        // crash or a clean completion).
        let mut interrupted_run: Option<String> = None;
        // Set when a deferred delivery intentionally SIGINT'd the child; the
        // reap path must NOT treat the exit as a crash and returns
        // `ResumeForDeferred`.
        let mut resuming_for_deferred = false;

        // Phase 3: armed when a `control_request` interrupt is sent. If
        // claude doesn't honor it within the grace window (a build lacking
        // the control frame), the sleeper escalates to SIGINT. Cleared when
        // the abort's `result` lands.
        let mut interrupt_deadline: Option<Instant> = None;

        // ADR 0054 / 0089: if an answer or deferred result is stashed, claude
        // WILL re-fire the tool on this `--resume` startup (id-stable, no
        // stdin — findings #9/#12). Establish a CONTINUATION turn (fresh
        // run_id, `RunStarted` with no `prompt_id`, no user-echo, no
        // `write_user_message`) so the re-fired `tool_result` and the model's
        // continuation are captured by the in-flight-turn branch instead of
        // dropped by the "line outside any turn" / "result with no in-flight
        // turn" branches. This takes precedence over the pending queue — an
        // outstanding delivery must be consumed first. (Edge: a stale result
        // with no matching pending tool leaves a continuation turn with no
        // output until `max_run_secs` or the next command; rare, and the
        // session stays command-responsive.)
        if !answers_in_hand.lock().await.is_empty() || !results_in_hand.lock().await.is_empty() {
            turn = Some(start_continuation_turn(evt_tx, cli, current_run_id).await);
        } else {
            // Kick off the first queued prompt (an initial prompt, or a
            // queue that survived a respawn) with no leading Idle; otherwise
            // announce Idle so the host's soft TTL arms.
            match pending.pop_front() {
                Some(qp) => {
                    if let Some(s) = stdin.as_mut() {
                        turn = Some(
                            start_turn(evt_tx, s, cli, qp.prompt_id, &qp.text, current_run_id)
                                .await,
                        );
                    }
                }
                None => emit(evt_tx, HarnessEvent::Idle).await,
            }
        }

        loop {
            // The relevant deadline this iteration: the in-flight turn's
            // wall-clock cap and/or the shutdown grace window, whichever
            // is sooner. `None` → park forever (idle, no shutdown).
            let next_deadline = [
                turn.as_ref().map(|t| t.deadline),
                shutdown_deadline,
                interrupt_deadline,
            ]
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
                                    *current_run_id.lock().await = None;
                                    // ADR 0054 Part C: carry this turn's
                                    // suppressed narrate-past ids into the
                                    // session-level scrub set so they survive
                                    // the kill→resume gap (empty for a clean
                                    // defer — nothing was suppressed).
                                    if !t.suppressed_msg_ids.is_empty() {
                                        scrub_msg_ids
                                            .lock()
                                            .await
                                            .extend(t.suppressed_msg_ids.iter().cloned());
                                    }
                                    // ADR 0054 / 0089: a turn that left a deferred tool pending
                                    // but ended `terminal_reason != "tool_deferred"`
                                    // is a narrate-past — claude abandoned the
                                    // deferred question instead of suspending. The
                                    // hallucinated reply was already suppressed in
                                    // `translate_jsonl`; surface the event for
                                    // observability. The UserQuestion card the hook
                                    // emitted stands, so the session stays
                                    // awaiting-answer.
                                    let narrated_past = marker.terminal_reason.as_deref()
                                        != Some("tool_deferred")
                                        && !t.deferred_pending.is_empty();
                                    let pending_tools = t.deferred_pending.len();
                                    let is_delivery_resume = t.is_delivery_resume;
                                    let run_id = t.run_id;
                                    tracing::info!(
                                        %run_id,
                                        subtype = %marker.subtype,
                                        is_error = marker.is_error,
                                        terminal_reason =
                                            marker.terminal_reason.as_deref().unwrap_or(""),
                                        "turn result"
                                    );
                                    if narrated_past {
                                        tracing::warn!(
                                            %run_id,
                                            pending_tools,
                                            terminal_reason =
                                                marker.terminal_reason.as_deref().unwrap_or(""),
                                            "deferred-tool narrate-past: claude ended the turn \
                                             without suspending on a parked tool; suppressed the \
                                             hallucinated reply (ADR 0054 / 0089)"
                                        );
                                    }
                                    if interrupted_run.as_deref() == Some(run_id.as_str()) {
                                        // Phase 3: the `control_request` abort
                                        // surfaced as a `result error_during_
                                        // execution` — the process is STILL
                                        // ALIVE. Report RunInterrupted, disarm
                                        // the SIGINT-escalation deadline, and
                                        // fall through to consume the next
                                        // queued prompt on the same process.
                                        interrupted_run = None;
                                        interrupt_deadline = None;
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
                                    // ADR 0054 Part B: an answer-resume that ends
                                    // with the answer STILL in hand means the
                                    // deferred tool was never re-fired — claude
                                    // narrate-past'd and abandoned it (a `--resume`
                                    // does NOT re-present an abandoned tool, verified
                                    // empirically). The stash→resume→re-fire path is
                                    // a dead end here, so deliver the answer as a
                                    // fresh user message (claude asked for it
                                    // conversationally) and mark the card answered —
                                    // never leave the question hanging.
                                    let stale: Vec<(String, engram_harness_proto::Answers)> =
                                        if is_delivery_resume {
                                            answers_in_hand.lock().await.drain().collect()
                                        } else {
                                            Vec::new()
                                        };
                                    let stale_results: Vec<(String, String)> =
                                        if is_delivery_resume {
                                            results_in_hand.lock().await.drain().collect()
                                        } else {
                                            Vec::new()
                                        };
                                    let stale_result_names: HashMap<String, String> =
                                        if stale_results.is_empty() {
                                            HashMap::new()
                                        } else {
                                            let mut deferred = deferred_calls.lock().await;
                                            stale_results
                                                .iter()
                                                .map(|(call_id, _)| {
                                                    let tool_name = deferred
                                                        .remove(call_id)
                                                        .map(|call| call.tool_name)
                                                        .unwrap_or_default();
                                                    (call_id.clone(), tool_name)
                                                })
                                                .collect()
                                        };
                                    if !stale.is_empty() || !stale_results.is_empty() {
                                        // ADR 0054: this is an answer-delivery
                                        // path the hook never reaches (claude
                                        // narrate-past'd and abandoned the
                                        // deferred tool, so its re-fire — and
                                        // the hook's `question_outstanding`
                                        // clear at the answer verdict — never
                                        // happen). Clear the flag HERE, before
                                        // the fallback turn starts, or it stays
                                        // set for the rest of the session and
                                        // every later genuine AUQ is wrongly
                                        // deduped as a #64389 duplicate (no
                                        // card, scrubbed → silently swallowed).
                                        // Invariant: the flag is cleared on
                                        // EVERY answer-delivery path, not just
                                        // the hook's.
                                        if !stale.is_empty() {
                                            *question_outstanding.lock().await = false;
                                        }
                                        if let Some(s) = stdin.as_mut() {
                                            let text = fallback_delivery_message(
                                                &stale,
                                                &stale_results,
                                            );
                                            let ft = start_turn(
                                                evt_tx, s, cli, None, &text, current_run_id,
                                            )
                                            .await;
                                            for (tool_call_id, answers) in stale {
                                                emit(
                                                    evt_tx,
                                                    HarnessEvent::QuestionAnswered {
                                                        run_id: ft.run_id.clone(),
                                                        tool_call_id,
                                                        answers,
                                                    },
                                                )
                                                .await;
                                            }
                                            // The normal id-stable re-fire
                                            // produces ToolCallCompleted when
                                            // Claude streams its tool_result.
                                            // This abandoned-re-fire fallback
                                            // has no such stream event, so ack
                                            // each drained result explicitly or
                                            // its coordinator outbox row will
                                            // redeliver forever.
                                            for (tool_call_id, _) in stale_results {
                                                emit(
                                                    evt_tx,
                                                    HarnessEvent::ToolCallCompleted {
                                                        run_id: ft.run_id.clone(),
                                                        tool_name: stale_result_names
                                                            .get(&tool_call_id)
                                                            .cloned()
                                                            .unwrap_or_default(),
                                                        tool_call_id,
                                                        ok: true,
                                                        duration_ms: 0,
                                                        result_summary: None,
                                                    },
                                                )
                                                .await;
                                            }
                                            turn = Some(ft);
                                        }
                                    } else {
                                        match pending.pop_front() {
                                            Some(qp) => {
                                                if let Some(s) = stdin.as_mut() {
                                                    turn = Some(
                                                        start_turn(
                                                            evt_tx,
                                                            s,
                                                            cli,
                                                            qp.prompt_id,
                                                            &qp.text,
                                                            current_run_id,
                                                        )
                                                        .await,
                                                    );
                                                }
                                            }
                                            None => emit(evt_tx, HarnessEvent::Idle).await,
                                        }
                                    }
                                } else {
                                    tracing::warn!("result line with no in-flight turn; ignoring");
                                }
                            } else if let Some(t) = turn.as_mut() {
                                if let Some(translated) = translate_jsonl_with_deferred_tools(
                                    &line,
                                    &t.run_id,
                                    &mut t.tool_calls,
                                    cli.max_tool_calls,
                                    &mut t.current_message_id,
                                    &mut t.pending_file_changes,
                                    &mut t.tool_call_starts,
                                    &t.deferred_tool_names,
                                    &mut t.deferred_pending,
                                    &mut t.suppressed_msg_ids,
                                ) {
                                    for ev in translated {
                                        // ADR 0054: drop a duplicate AUQ's
                                        // tool-log entry (the #64389 double-fire,
                                        // within- or cross-run). A question is
                                        // already outstanding, so the hook gave
                                        // this AUQ no card; suppress its
                                        // ToolCallStarted too so no phantom tool
                                        // call flickers in the UI. (The first,
                                        // carded question's card stands regardless
                                        // — the UI renders that, not this entry.)
                                        if let HarnessEvent::ToolCallStarted {
                                            tool_name, ..
                                        } = &ev
                                        {
                                            if tool_name == "AskUserQuestion"
                                                && *question_outstanding.lock().await
                                            {
                                                continue;
                                            }
                                        }
                                        emit(evt_tx, ev).await;
                                    }
                                }
                            } else {
                                // A line outside any turn (e.g. claude's
                                // init banner before the first prompt):
                                // parse mainly for the session-id capture
                                // side effect. No turn ⇒ no chunks to stream,
                                // so the message-id sink is a throwaway. A
                                // `TitleSuggested` can legitimately arrive
                                // between turns, though — forward those (they
                                // carry no run_id) rather than drop them.
                                let mut sink = 0u32;
                                if let Some(translated) = translate_jsonl(
                                    &line,
                                    "",
                                    &mut sink,
                                    cli.max_tool_calls,
                                    &mut None,
                                    &mut HashMap::new(),
                                    &mut HashMap::new(),
                                    &mut HashSet::new(),
                                    &mut Vec::new(),
                                ) {
                                    for ev in translated {
                                        if matches!(ev, HarnessEvent::TitleSuggested { .. }) {
                                            emit(evt_tx, ev).await;
                                        }
                                    }
                                }
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
                            if !seen_prompt_ids.insert(prompt_id.clone()) {
                                // ADR 0052: a host replay (command-side
                                // at-least-once) of a prompt we already
                                // started or queued — ignore so it can't
                                // double-run.
                                tracing::debug!(%prompt_id, "duplicate prompt (replay); ignoring");
                            } else if turn.is_none() {
                                // Idle: start the run immediately.
                                if let Some(s) = stdin.as_mut() {
                                    turn = Some(
                                        start_turn(
                                            evt_tx,
                                            s,
                                            cli,
                                            Some(prompt_id),
                                            &text,
                                            current_run_id,
                                        )
                                        .await,
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
                        Some(HarnessCommand::AnswerQuestion { tool_call_id, answers }) => {
                            // ADR 0054: stash the answer for the hook of the
                            // NEXT (resumed) process — it can ONLY land on the
                            // resumed re-fire, never on this live process
                            // (feeding the live stream re-infers a new
                            // tool_use_id, finding #11). Then end THIS claude
                            // and re-enter via `--resume`.
                            tracing::info!(
                                %tool_call_id,
                                "answer received; resuming claude to re-fire the deferred AUQ"
                            );
                            answers_in_hand.lock().await.insert(tool_call_id, answers);
                            resuming_for_deferred = true;
                            // claude flushes its transcript per-message, so the
                            // session stays cleanly `--resume`-able after SIGINT.
                            sigint_child(&child);
                            // `break` (NOT `return`): the existing `None =>
                            // return ChannelClosed` arm skips the reap block,
                            // but we MUST reap (wait the child, drain stderr,
                            // close any defensively-in-flight turn). Breaking
                            // the loop runs the reap, which returns
                            // `ResumeForDeferred`.
                            break;
                        }
                        Some(HarnessCommand::ToolResult { call_id, result_json }) => {
                            let known_deferred =
                                deferred_calls.lock().await.contains_key(&call_id);
                            // A live parked MCP bridge is a synchronous call:
                            // serve it in place. Deferred hooks never open the
                            // bridge until their id-stable resume re-fire, so
                            // every other result is stashed before SIGINT and
                            // consumed only after transcript scrub + --resume.
                            let delivered = mcp_server::route_tool_result(
                                &call_id,
                                result_json,
                                parked_mcp_calls,
                                results_in_hand,
                            )
                            .await;
                            if delivered {
                                tracing::debug!(%call_id, "served result to parked live MCP call");
                            } else {
                                tracing::info!(
                                    %call_id,
                                    known_deferred,
                                    "deferred or unowned tool result stashed; resuming for id-stable re-fire"
                                );
                                resuming_for_deferred = true;
                                sigint_child(&child);
                                break;
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
                                // Phase 3 (ADR 0030): the PRIMARY interrupt is
                                // an in-band `control_request` on the held-open
                                // stdin. claude aborts the in-flight turn
                                // (surfacing as `result error_during_execution`)
                                // and STAYS ALIVE — no SIGINT, no respawn — so
                                // conversation context is preserved and the
                                // next queued prompt runs on the SAME warm
                                // process. (This is the root-cause fix for the
                                // SIGINT-kills-the-persistent-process bug:
                                // session ba3ae8d5, where interrupt + a queued
                                // message respawned `--resume` into a fragile,
                                // context-less process that the harness then
                                // misread as a crash.) Mark the run so the
                                // `result` handler reports RunInterrupted, and
                                // arm a grace deadline: a build that ignores the
                                // control frame is escalated to SIGINT (the
                                // fallback) so an interrupt can never wedge.
                                let run_id = t.run_id.clone();
                                interrupted_run = Some(run_id.clone());
                                let request_id = format!("int-{}", uuid::Uuid::new_v4());
                                match stdin.as_mut() {
                                    Some(s) => match write_control_interrupt(s, &request_id).await {
                                        Ok(()) => {
                                            tracing::info!(%run_id, "interrupt: sent control_request; awaiting abort");
                                            interrupt_deadline = Some(
                                                Instant::now()
                                                    + Duration::from_secs(INTERRUPT_GRACE_SECS),
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(%run_id, error = %e, "control_request write failed; SIGINT fallback");
                                            sigint_child(&child);
                                        }
                                    },
                                    None => {
                                        // stdin already closed (a Shutdown is
                                        // draining) — fall back to SIGINT.
                                        tracing::warn!(%run_id, "interrupt with no stdin; SIGINT fallback");
                                        sigint_child(&child);
                                    }
                                }
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
                        // All command senders gone = the connection loop
                        // exited = process teardown (the Superseded exit
                        // path). SIGINT claude first: this arm skips the
                        // reap block, and an orphaned twin left running
                        // would contend with the successor harness's
                        // `--resume` on the same transcript (claude
                        // flushes per-message, so SIGINT is
                        // resume-safe).
                        None => {
                            sigint_child(&child);
                            return SessionOutcome::ChannelClosed;
                        }
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
                    if interrupt_deadline.is_some_and(|d| Instant::now() >= d) && turn.is_some() {
                        // Phase 3 fallback: the `control_request` interrupt
                        // wasn't honored within grace (a claude build lacking
                        // the control frame). Escalate to SIGINT — claude
                        // exits, the reap reports RunInterrupted (interrupted_run
                        // is set) and the engine respawns `--resume`. Disarm so
                        // we don't re-fire; claude's stdout EOF breaks the loop.
                        tracing::warn!("control_request interrupt not honored within grace; SIGINT escalation");
                        interrupt_deadline = None;
                        sigint_child(&child);
                    } else if shutting_down {
                        tracing::warn!("shutdown grace elapsed; killing claude");
                        let _ = child.start_kill();
                        break;
                    } else {
                        tracing::warn!("max_run_secs elapsed; killing claude");
                        let _ = child.start_kill();
                        break;
                    }
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
            *current_run_id.lock().await = None;
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
            } else if resuming_for_deferred {
                // ADR 0054: an `AnswerQuestion` tore down a turn that was
                // (defensively) still in flight — the deferred-question turn
                // normally already ended `tool_deferred` (so `turn` is
                // None), but a racing answer is handled here. This is NOT a
                // crash: close the run quietly (no abnormal-exit System
                // message); the resumed process establishes the continuation
                // turn. Branch ordering matters — this MUST precede the
                // generic crash `else`.
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
        } else if resuming_for_deferred {
            // Intentional deferred-delivery resume — checked before the
            // generic `Respawn` so it never trips fast-crash backoff or
            // reads as a crash.
            SessionOutcome::ResumeForDeferred
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
        current_run_id: &hook_server::CurrentRunId,
    ) -> TurnState {
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        // ADR 0054: publish the run_id so a hook firing within this turn
        // tags its UserQuestion/QuestionAnswered with the right run.
        *current_run_id.lock().await = Some(run_id.clone());
        // `RunStarted{prompt_id}` is the "queued prompt consumed" signal:
        // a UI that drew a greyed type-ahead item with this id moves it
        // into the conversation now. Issue #535 (d): `Some` in practice for
        // every turn now, including the session's create-time initial one.
        //
        // `prompt_summary` is deliberately `None`: the coordinator emits the
        // user turn as a `role:user` agent_message (carrying `prompt_id`),
        // which is the single authoritative source of the user bubble. If we
        // ALSO put the text on `RunStarted`, the web renders the prompt twice
        // (the `rs:idx` + `m:idx` double-render). The web correlates the queue
        // lifecycle via `prompt_id`, not via this summary.
        emit(
            evt_tx,
            HarnessEvent::RunStarted {
                run_id: run_id.clone(),
                prompt_id,
                prompt_summary: None,
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
            current_message_id: None,
            pending_file_changes: HashMap::new(),
            tool_call_starts: HashMap::new(),
            deferred_pending: HashSet::new(),
            deferred_tool_names: deferred_tool_names(&cli.tool_manifest),
            is_delivery_resume: false,
            suppressed_msg_ids: Vec::new(),
        }
    }

    /// ADR 0054: start a CONTINUATION turn for an answer-resume. Like
    /// `start_turn` it mints a `run_id`, publishes it to `current_run_id`,
    /// and emits `RunStarted` BEFORE any output — but with **no `prompt_id`**
    /// (this turn was not started by a user prompt) and **no
    /// `write_user_message`** (the deferred AUQ re-fires from claude's
    /// persisted state on `--resume`; the stream's sole `user` event is the
    /// deferred `tool_result` = the answer). So no synthetic user turn ever
    /// becomes an engram `session_event`. Reuses `max_run_secs` — the model
    /// can grind arbitrarily long once unblocked; the defer-spin never arms
    /// because the answer is in hand on the first re-fire.
    async fn start_continuation_turn(
        evt_tx: &mpsc::Sender<HarnessEvent>,
        cli: &Cli,
        current_run_id: &hook_server::CurrentRunId,
    ) -> TurnState {
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        *current_run_id.lock().await = Some(run_id.clone());
        emit(
            evt_tx,
            HarnessEvent::RunStarted {
                run_id: run_id.clone(),
                prompt_id: None,
                prompt_summary: None,
            },
        )
        .await;
        TurnState {
            run_id,
            tool_calls: 0,
            deadline: Instant::now() + Duration::from_secs(cli.max_run_secs),
            current_message_id: None,
            pending_file_changes: HashMap::new(),
            tool_call_starts: HashMap::new(),
            deferred_pending: HashSet::new(),
            deferred_tool_names: deferred_tool_names(&cli.tool_manifest),
            is_delivery_resume: true,
            suppressed_msg_ids: Vec::new(),
        }
    }

    /// ADR 0054 Part B: render the canonical answer map(s) as a plain user
    /// message — used when a narrate-past abandoned the deferred tool, so the
    /// answer can't ride a re-fired `tool_result`. claude asked the question
    /// conversationally, so an ordinary message is exactly what it awaits.
    fn fallback_delivery_message(
        stale_answers: &[(String, engram_harness_proto::Answers)],
        stale_results: &[(String, String)],
    ) -> String {
        let mut lines = Vec::new();
        if !stale_answers.is_empty() {
            lines.push("Here are my answers to the question(s) you just asked:".to_string());
        }
        for (_tool_call_id, answers) in stale_answers {
            for (question, labels) in answers {
                lines.push(format!("- {}: {}", question, labels.join(", ")));
            }
        }
        if !stale_results.is_empty() {
            lines.push("Results for the deferred tool call(s) you made earlier:".to_string());
        }
        for (call_id, result_json) in stale_results {
            lines.push(format!("- {call_id}: {result_json}"));
        }
        lines.join("\n")
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

    /// Phase 3 (ADR 0030): the in-band interrupt. Writes a `control_request`
    /// with `subtype:interrupt` to claude's held-open stdin. claude acks with
    /// a `control_response` and ABORTS the in-flight turn (which surfaces as a
    /// `result` with `subtype:error_during_execution`) while the process STAYS
    /// ALIVE — so the next prompt runs on the same warm process: no respawn, no
    /// lost context. We don't block on the `control_response` ack; the
    /// subsequent `result` is the load-bearing signal the engine acts on.
    async fn write_control_interrupt(
        stdin: &mut tokio::process::ChildStdin,
        request_id: &str,
    ) -> std::io::Result<()> {
        let msg = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" },
        });
        let mut line = serde_json::to_string(&msg).expect("serialize control_request");
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await
    }

    /// Build the argv for the persistent streaming `claude`. `--input-
    /// format stream-json` holds stdin open for newline-delimited `user`
    /// messages (one per turn); `--output-format stream-json --verbose`
    /// gives us the per-turn `result` terminator on stdout. No trailing
    /// prompt arg — prompts are written to stdin via `write_user_message`.
    fn build_claude_argv(
        resume_id: &Option<String>,
        append_system_prompt: Option<&str>,
        mcp_config_path: Option<&str>,
    ) -> Vec<String> {
        let mut argv = vec![
            "--print".to_string(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // Phase 1c: stream the assistant's token deltas as
            // `stream_event`/`content_block_delta` lines (in addition to
            // the complete `assistant` message at block end), so the
            // engine can forward them as ephemeral `AgentMessageChunk`s
            // for live-typing. The complete message is still emitted and
            // remains the durable record.
            "--include-partial-messages".into(),
            // ADR 0054: a PreToolUse hook replaces
            // `--dangerously-skip-permissions`. The hook (written by
            // `write_hook_settings`, run as `hook-bridge`) auto-allows
            // ordinary tools — explicit and auditable, the VM is still the
            // safety boundary — and defers AskUserQuestion to the harness.
            "--settings".into(),
            HOOK_SETTINGS_FILE.into(),
        ];
        if let Some(id) = resume_id {
            argv.push("--resume".into());
            argv.push(id.clone());
        }
        if let Some(path) = mcp_config_path {
            argv.push("--mcp-config".into());
            argv.push(path.to_string());
            // Without strict mode claude merges the guest user's own MCP
            // configuration, exposing servers outside the orchestrator's
            // per-session manifest (ADR 0089 spike, Claude 2.1.207).
            argv.push("--strict-mcp-config".into());
        }
        // ADR 0060: an external trigger (e.g. Slack) flavors the agent's system
        // prompt via ENGRAM_APPEND_SYSTEM_PROMPT (carried through harness_env).
        // Empty = unset (skip the flag).
        if let Some(p) = append_system_prompt {
            if !p.is_empty() {
                argv.push("--append-system-prompt".into());
                argv.push(p.to_string());
            }
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

    /// ADR 0054 Part C — remove the narrate-past poison from claude's
    /// transcript so an answer-resume starts from a clean defer.
    ///
    /// This only fires **once in a while.** A PreToolUse `defer` is *supposed*
    /// to end the turn right at the `tool_use` (transcript byte-identical,
    /// nothing after it). But nondeterministically — ~1-in-6 in local repros,
    /// independent of model/tool/permission-mode — claude instead runs a
    /// continuation inference, hands *itself* `tool_result{is_error:true,
    /// content:"[Tool result missing due to internal error]"}` for the deferred
    /// tool, and narrates an error / "I'll ask again." That is an **upstream
    /// claude-code bug**, tracked at
    /// <https://github.com/anthropics/claude-code/issues/64389> — NOT our hook
    /// (the hook-bridge returns a clean `defer`, exit 0; the error placeholder
    /// is synthesized inside claude on the continuation request, never on the
    /// wire we control).
    ///
    /// Part A already hides that assistant text from the UI, but it still
    /// lands in claude's own `.jsonl`; on `--resume` the model reads its own
    /// stale text and re-asks regardless of the real answer we deliver. We
    /// remove exactly the assistant messages whose ids Part A suppressed
    /// (matched by `message.id`, never a content heuristic), AND any assistant
    /// message carrying a `tool_use` whose id is a suppressed duplicate
    /// deferred call (`dup_tool_ids` — AUQ within/cross-run or manifest tool
    /// within-turn #64389 double-fire; matched by the tool_use id the hook
    /// recorded, not a heuristic). Re-link the
    /// `parentUuid` of any survivor that pointed at a removed line, and write
    /// atomically. The result is byte-equivalent to a clean single-call defer,
    /// which resumes correctly. Returns messages removed.
    fn scrub_transcript(
        path: &Path,
        suppressed_ids: &HashSet<String>,
        dup_tool_ids: &HashSet<String>,
    ) -> std::io::Result<usize> {
        let content = std::fs::read_to_string(path)?;
        // uuid -> parentUuid of each removed line, for re-linking survivors.
        let mut removed_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut kept: Vec<String> = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    kept.push(line.to_string());
                    continue;
                }
            };
            let is_assistant = v.get("type").and_then(|t| t.as_str()) == Some("assistant");
            let id = v
                .get("message")
                .and_then(|m| m.get("id"))
                .and_then(|s| s.as_str());
            // A duplicate-deferred-call message: an assistant line whose
            // content holds a `tool_use` block with an id the hook flagged.
            let carries_dup_tool = is_assistant
                && !dup_tool_ids.is_empty()
                && v.get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks.iter().any(|b| {
                            b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                                && b.get("id")
                                    .and_then(|s| s.as_str())
                                    .map(|i| dup_tool_ids.contains(i))
                                    .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
            let drop_by_msg_id =
                is_assistant && id.map(|i| suppressed_ids.contains(i)).unwrap_or(false);
            if drop_by_msg_id || carries_dup_tool {
                if let Some(uuid) = v.get("uuid").and_then(|s| s.as_str()) {
                    let parent = v
                        .get("parentUuid")
                        .and_then(|s| s.as_str())
                        .map(str::to_string);
                    removed_parent.insert(uuid.to_string(), parent);
                }
                continue; // drop the narrate-past / duplicate-call message
            }
            kept.push(line.to_string());
        }
        let removed = removed_parent.len();
        if removed == 0 {
            return Ok(0);
        }
        // Re-link any survivor whose parent we removed (defensive — the
        // narrate-past is normally the trailing message of the turn, so this is
        // usually a no-op, but it keeps the parentUuid chain intact regardless).
        let out: Vec<String> = kept
            .into_iter()
            .map(|line| {
                let mut v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => return line,
                };
                let reparent = v
                    .get("parentUuid")
                    .and_then(|s| s.as_str())
                    .and_then(|p| removed_parent.get(p))
                    .cloned();
                if let Some(grandparent) = reparent {
                    v["parentUuid"] = match grandparent {
                        Some(gp) => Value::String(gp),
                        None => Value::Null,
                    };
                    return v.to_string();
                }
                line
            })
            .collect();
        // Atomic replace: write a sibling temp file, then rename over the
        // original (claude is dead at the resume gap, so nothing races us).
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".scrubtmp");
        let tmp = PathBuf::from(tmp);
        let mut body = out.join("\n");
        body.push('\n');
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(removed)
    }

    /// Locate claude's on-disk transcript for `sid`. claude writes it under
    /// `<config>/projects/<encoded-cwd>/<sid>.jsonl`, where `<config>` is
    /// `$CLAUDE_CONFIG_DIR` or `$HOME/.claude`. We glob by session id rather
    /// than recompute the cwd-encoding, so the lookup is robust to changes in
    /// that scheme.
    fn find_claude_transcript(sid: &str) -> Option<PathBuf> {
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").unwrap_or_else(|| "/root".into());
                PathBuf::from(home).join(".claude")
            });
        let projects = config.join("projects");
        let target = format!("{sid}.jsonl");
        for entry in std::fs::read_dir(&projects).ok()?.flatten() {
            let candidate = entry.path().join(&target);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
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
        /// ADR 0054: `tool_deferred` vs `completed` — the ONLY discriminator
        /// between a genuinely-deferred `AskUserQuestion` (turn suspends,
        /// awaiting an answer) and a "narrate-past" (claude abandons the
        /// deferred tool and ends the turn). Both are `subtype:"success"`,
        /// `is_error:false`, so neither of those can tell them apart. Mirrors
        /// the Agent SDK's `SDKResultSuccess.terminal_reason`. `None` on older
        /// CLIs that don't emit the field.
        pub terminal_reason: Option<String>,
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
            terminal_reason: v
                .get("terminal_reason")
                .and_then(|s| s.as_str())
                .map(str::to_string),
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
    // The params are the per-turn mutable state threaded from the session loop
    // (TurnState fields + the line); bundling them into a struct would obscure
    // more than it clarifies, and the throwaway call site has no TurnState.
    #[allow(clippy::too_many_arguments)]
    pub fn translate_jsonl(
        line: &str,
        run_id: &str,
        tool_calls: &mut u32,
        max_tool_calls: u32,
        current_message_id: &mut Option<String>,
        pending_file_changes: &mut HashMap<String, (String, FileChange)>,
        tool_call_starts: &mut HashMap<String, (String, std::time::Instant)>,
        deferred_pending: &mut HashSet<String>,
        suppressed_msg_ids: &mut Vec<String>,
    ) -> Option<Vec<HarnessEvent>> {
        translate_jsonl_with_deferred_tools(
            line,
            run_id,
            tool_calls,
            max_tool_calls,
            current_message_id,
            pending_file_changes,
            tool_call_starts,
            &HashSet::new(),
            deferred_pending,
            suppressed_msg_ids,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn translate_jsonl_with_deferred_tools(
        line: &str,
        run_id: &str,
        tool_calls: &mut u32,
        max_tool_calls: u32,
        // Phase 1c: the streaming message id, tracked across lines within a
        // turn (set by `stream_event`/`message_start`) so token chunks carry
        // the same id as the terminal `assistant` message.
        current_message_id: &mut Option<String>,
        // ADR 0054 Flavor A: file changes parsed from a tool_use, keyed by
        // tool_use_id and held until the matching tool_result (so we emit a
        // `FileChanged` only on success). Tracked across lines within a turn.
        pending_file_changes: &mut HashMap<String, (String, FileChange)>,
        // Papercut fix (2026-07-11 campaign): (tool_name, started_at) per
        // in-flight tool_use_id — recorded at tool_use, consumed at the
        // matching tool_result so ToolCallCompleted carries a real
        // duration_ms + tool_name. Tracked across lines within a turn.
        tool_call_starts: &mut HashMap<String, (String, std::time::Instant)>,
        // ADR 0089: generic manifest names whose PreToolUse hook returns
        // `defer`. Claude reports them with the MCP server prefix.
        deferred_tool_names: &HashSet<String>,
        // ADR 0054 / 0089: pending no-result AUQ and deferred manifest
        // tool_use_ids. While non-empty, assistant text/chunks are the
        // narrate-past hallucination and are suppressed.
        deferred_pending: &mut HashSet<String>,
        // ADR 0054 Part C: ids of assistant messages suppressed as narrate-past
        // this turn, recorded here so `scrub_transcript` can delete them from
        // claude's transcript before the answer-resume.
        suppressed_msg_ids: &mut Vec<String>,
    ) -> Option<Vec<HarnessEvent>> {
        let v: Value = serde_json::from_str(line).ok()?;
        let ty = v.get("type")?.as_str()?;
        let mut out: Vec<HarnessEvent> = Vec::new();
        match ty {
            "stream_event" => {
                // Phase 1c: partial-message streaming (`--include-partial-
                // messages`). `message_start` carries the assistant message
                // id — the SAME id the terminal `assistant` line will use —
                // so we stash it and chunks correlate to the durable message.
                // A `content_block_delta` with a `text_delta` is one live
                // token chunk → emit an EPHEMERAL `AgentMessageChunk`.
                // Everything else (block start/stop, message delta/stop,
                // tool-input deltas) is ignored here: the durable
                // `assistant`/`user` lines carry the authoritative content.
                let event = v.get("event")?;
                match event.get("type").and_then(|s| s.as_str()).unwrap_or("") {
                    "message_start" => {
                        if let Some(id) = event
                            .get("message")
                            .and_then(|m| m.get("id"))
                            .and_then(|s| s.as_str())
                        {
                            *current_message_id = Some(id.to_string());
                        }
                    }
                    "content_block_delta" => {
                        if let Some(delta) = event.get("delta") {
                            if delta.get("type").and_then(|s| s.as_str()) == Some("text_delta") {
                                if let Some(chunk) = delta.get("text").and_then(|s| s.as_str()) {
                                    // ADR 0054 / 0089: while a tool is pending (deferred,
                                    // no result), live token chunks are the
                                    // narrate-past hallucination streaming out —
                                    // drop them so the bogus reply never types out.
                                    if !chunk.is_empty() && deferred_pending.is_empty() {
                                        out.push(HarnessEvent::AgentMessageChunk {
                                            run_id: run_id.to_string(),
                                            message_id: current_message_id
                                                .clone()
                                                .unwrap_or_else(|| "msg-?".to_string()),
                                            chunk: truncate_str(chunk, MAX_CHUNK_BYTES),
                                        });
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
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
                // ADR 0054 / 0089: snapshot BEFORE the block loop. If an EARLIER
                // message in this turn left a tool pending (the hook deferred
                // it), any assistant text in THIS later message is the
                // narrate-past hallucination → drop it. A preamble in the SAME
                // message as the AUQ tool_use survives: that id is only inserted
                // during this loop, after this snapshot.
                let suppress_text = !deferred_pending.is_empty();
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
                                let input = b.get("input");
                                // ADR 0054 Flavor A: stash a parsed file change
                                // for a Write/Edit/MultiEdit, keyed by id, to
                                // emit on the SUCCESSFUL tool_result. The
                                // generic ToolCallStarted still fires (the UI
                                // dedups it against the FileChanged card).
                                if let Some(fc) = input.and_then(|i| parse_file_change(&name, i)) {
                                    pending_file_changes.insert(tcid.clone(), fc);
                                }
                                let args_summary = input
                                    .map(|v| truncate_str(&v.to_string(), MAX_ARGS_SUMMARY_BYTES));
                                // AUQ and manifest-deferred MCP tools receive no
                                // `tool_result` until external delivery. Mark the
                                // id so a subsequent narrate-past is suppressed.
                                let is_deferred_manifest_tool = name
                                    .strip_prefix("mcp__engrams__")
                                    .is_some_and(|name| deferred_tool_names.contains(name));
                                if name == "AskUserQuestion" || is_deferred_manifest_tool {
                                    deferred_pending.insert(tcid.clone());
                                }
                                tool_call_starts.insert(
                                    tcid.clone(),
                                    (name.clone(), std::time::Instant::now()),
                                );
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
                        if suppress_text {
                            // ADR 0054 Part C: this assistant message is the
                            // narrate-past hallucination (already dropped from
                            // the UI by Part A). Record its id so the scrub can
                            // remove it from claude's transcript before the
                            // answer-resume — otherwise claude reads its own
                            // stale "I'll try again" text on `--resume` and
                            // re-asks regardless of the real answer.
                            suppressed_msg_ids.push(msg_id);
                        } else {
                            out.push(HarnessEvent::AgentMessage {
                                run_id: rid,
                                message_id: msg_id,
                                role: AgentRole::Assistant,
                                text: truncate_str(&text_buf, MAX_AGENT_MESSAGE_BYTES),
                            });
                        }
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
                            // ADR 0054 / 0089: a tool_result resolves a pending
                            // deferred call, so following assistant
                            // text is legitimate ("You selected …") and must
                            // not be suppressed.
                            deferred_pending.remove(&tcid);
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
                            let (tool_name, duration_ms) = match tool_call_starts.remove(&tcid) {
                                Some((name, started)) => {
                                    (name, started.elapsed().as_millis() as u64)
                                }
                                // Result for a tool_use this process never saw
                                // start (e.g. a --resume replay) — honest zeros.
                                None => (String::new(), 0),
                            };
                            out.push(HarnessEvent::ToolCallCompleted {
                                run_id: rid.clone(),
                                tool_call_id: tcid.clone(),
                                tool_name,
                                ok: !is_error,
                                duration_ms,
                                result_summary: Some(truncate_str(
                                    &result_text,
                                    MAX_RESULT_SUMMARY_BYTES,
                                )),
                            });
                            // ADR 0054 Flavor A: a Write/Edit/MultiEdit landed
                            // its result. Emit the rich diff ONLY on success;
                            // drop the stashed change on failure (truthful —
                            // no phantom diff for a failed edit).
                            if let Some((path, change)) = pending_file_changes.remove(&tcid) {
                                if !is_error {
                                    out.push(HarnessEvent::FileChanged {
                                        run_id: rid.clone(),
                                        tool_call_id: tcid,
                                        path,
                                        change,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            "result" => {
                // The session loop closes the turn on this line via
                // `detect_result_marker`; nothing to translate here.
            }
            "ai-title" => {
                // Claude Code emits `{"type":"ai-title","aiTitle":"…",…}` with an
                // LLM-generated short title for the session. Surface it as a
                // first-class harness event; the coordinator records the latest
                // one and the orchestrator uses it as the session's display title.
                if let Some(title) = v.get("aiTitle").and_then(|s| s.as_str()) {
                    let title = title.trim();
                    if !title.is_empty() {
                        out.push(HarnessEvent::TitleSuggested {
                            title: truncate_str(title, MAX_TITLE_BYTES),
                        });
                    }
                }
            }
            _ => {}
        }
        Some(out)
    }

    /// ADR 0054 Flavor A: map a Claude `Write`/`Edit`/`MultiEdit` tool input
    /// into a normalized `(path, FileChange)`. Returns `None` for any other
    /// tool or a malformed input. Each inner string is truncated to
    /// `MAX_FILE_CHANGE_BYTES` *here* (per-string, before serialization) so a
    /// single huge write can't blow the frame — the 1 KB `args_summary` cap is
    /// untouched and still applies to every tool's generic event.
    pub fn parse_file_change(name: &str, input: &Value) -> Option<(String, FileChange)> {
        let path = input.get("file_path")?.as_str()?.to_string();
        let clip = |s: &str| truncate_str(s, MAX_FILE_CHANGE_BYTES);
        match name {
            "Write" => {
                let content = clip(input.get("content")?.as_str()?);
                Some((path, FileChange::Write { content }))
            }
            "Edit" => {
                let old = clip(input.get("old_string")?.as_str()?);
                let new = clip(input.get("new_string")?.as_str()?);
                Some((
                    path,
                    FileChange::Edit {
                        hunks: vec![EditHunk { old, new }],
                    },
                ))
            }
            "MultiEdit" => {
                let edits = input.get("edits")?.as_array()?;
                let hunks: Vec<EditHunk> = edits
                    .iter()
                    .filter_map(|e| {
                        Some(EditHunk {
                            old: clip(e.get("old_string")?.as_str()?),
                            new: clip(e.get("new_string")?.as_str()?),
                        })
                    })
                    .collect();
                if hunks.is_empty() {
                    return None;
                }
                Some((path, FileChange::Edit { hunks }))
            }
            _ => None,
        }
    }

    pub fn truncate_str(s: &str, max_bytes: usize) -> String {
        engram_harness_sdk::truncate_utf8(s, max_bytes)
    }

    #[cfg(test)]
    mod engine_tests {
        use super::*;
        use engram_harness_proto::{Answers, Question, QuestionOption};
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // ADR 0060: an external trigger flavors the agent's system prompt via
        // ENGRAM_APPEND_SYSTEM_PROMPT (rides harness_env). build_claude_argv
        // turns a set value into `--append-system-prompt <value>`.
        #[test]
        fn argv_carries_append_system_prompt_when_set() {
            let argv =
                build_claude_argv(&None, Some("You were triggered from a Slack thread."), None);
            let pos = argv
                .iter()
                .position(|a| a == "--append-system-prompt")
                .expect("flag present when set");
            assert_eq!(
                argv.get(pos + 1).map(String::as_str),
                Some("You were triggered from a Slack thread."),
            );
        }

        #[test]
        fn argv_omits_append_system_prompt_when_absent_or_empty() {
            for v in [None, Some("")] {
                let argv = build_claude_argv(&None, v, None);
                assert!(
                    !argv.iter().any(|a| a == "--append-system-prompt"),
                    "flag must be absent for {v:?}",
                );
            }
        }

        #[test]
        fn argv_carries_strict_mcp_config_when_injected_tools_exist() {
            let argv = build_claude_argv(&None, None, Some("/tmp/mcp-config.json"));
            let pos = argv
                .iter()
                .position(|arg| arg == "--mcp-config")
                .expect("MCP config flag present");
            assert_eq!(
                argv.get(pos + 1).map(String::as_str),
                Some("/tmp/mcp-config.json")
            );
            assert!(
                argv.iter().any(|arg| arg == "--strict-mcp-config"),
                "strict mode prevents guest user MCP servers from loading"
            );
        }

        #[test]
        fn argv_omits_mcp_flags_without_injected_tools() {
            let argv = build_claude_argv(&None, None, None);
            assert!(!argv.iter().any(|arg| arg == "--mcp-config"));
            assert!(!argv.iter().any(|arg| arg == "--strict-mcp-config"));
        }

        #[tokio::test]
        async fn mcp_config_declares_only_the_self_stdio_server() {
            let dir = std::env::temp_dir()
                .join(format!("engram-mcp-config-test-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&dir).await.unwrap();
            let path = dir.join("mcp-config.json");
            let manifest = parse_tool_manifest(
                r#"[
                    {"name":"save_memory","description":"Save it","inputSchema":{"type":"object"},"execution":"sync","nativeBindings":{}},
                    {"name":"ask_user_question","description":"Ask","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{"claude":"AskUserQuestion"}}
                ]"#,
            )
            .unwrap();

            assert!(
                write_mcp_config(&path, &manifest, Path::new("/sbin/engram-harness-claude"))
                    .await
                    .unwrap()
            );
            let config: Value =
                serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
            assert_eq!(
                config,
                serde_json::json!({
                    "mcpServers": {
                        "engrams": {
                            "type": "stdio",
                            "command": "/sbin/engram-harness-claude",
                            "args": ["mcp-bridge"]
                        }
                    }
                })
            );

            let native_only = parse_tool_manifest(
                r#"[{"name":"ask_user_question","description":"Ask","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{"claude":"AskUserQuestion"}}]"#,
            )
            .unwrap();
            assert!(
                !write_mcp_config(&path, &native_only, Path::new("/sbin/harness"))
                    .await
                    .unwrap(),
                "a native-only manifest must not enable MCP"
            );
            assert!(tokio::fs::metadata(&path).await.is_err());
            let _ = tokio::fs::remove_dir_all(dir).await;
        }

        fn mcp_fixture_manifest() -> ToolManifest {
            parse_tool_manifest(
                r#"[
                    {"name":"save_memory","description":"Save a memory","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}},"execution":"sync","nativeBindings":{}},
                    {"name":"ask_user_question","description":"Ask","inputSchema":{"type":"object"},"execution":"deferred","nativeBindings":{"claude":"AskUserQuestion"}}
                ]"#,
            )
            .unwrap()
        }

        #[tokio::test]
        async fn mcp_initialize_reports_protocol_and_capabilities() {
            let response = mcp_bridge::handle_request(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18"}
                }),
                &mcp_fixture_manifest(),
                "/unused.sock",
            )
            .await
            .unwrap();
            assert_eq!(response["jsonrpc"], "2.0");
            assert_eq!(response["id"], 1);
            assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
            assert!(response["result"]["capabilities"]["tools"].is_object());
            assert_eq!(response["result"]["serverInfo"]["name"], "engrams");
        }

        #[tokio::test]
        async fn mcp_tools_list_comes_from_injected_manifest_tools() {
            let response = mcp_bridge::handle_request(
                serde_json::json!({"jsonrpc":"2.0","id":"list-1","method":"tools/list"}),
                &mcp_fixture_manifest(),
                "/unused.sock",
            )
            .await
            .unwrap();
            assert_eq!(
                response["result"]["tools"],
                serde_json::json!([{
                    "name": "save_memory",
                    "description": "Save a memory",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}}
                    }
                }]),
                "Claude-native bindings are omitted from the MCP surface"
            );
        }

        #[tokio::test]
        async fn mcp_unknown_method_is_a_json_rpc_error() {
            let response = mcp_bridge::handle_request(
                serde_json::json!({"jsonrpc":"2.0","id":9,"method":"no/such/method"}),
                &mcp_fixture_manifest(),
                "/unused.sock",
            )
            .await
            .unwrap();
            assert_eq!(response["id"], 9);
            assert_eq!(response["error"]["code"], -32601);
        }

        #[tokio::test]
        async fn mcp_tools_call_round_trips_through_main_process_socket() {
            let sock = std::env::temp_dir()
                .join(format!(
                    "engram-mcp-bridge-test-{}.sock",
                    uuid::Uuid::new_v4()
                ))
                .to_string_lossy()
                .into_owned();
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                let req: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(req["name"], "save_memory");
                assert_eq!(req["args"], serde_json::json!({"text":"remember"}));
                assert_eq!(req["tool_use_id"], "toolu_123");
                w.write_all(b"{\"result_json\":\"{\\\"saved\\\":true}\"}\n")
                    .await
                    .unwrap();
                w.flush().await.unwrap();
            });

            let response = mcp_bridge::handle_request(
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":2,
                    "method":"tools/call",
                    "params":{
                        "name":"save_memory",
                        "arguments":{"text":"remember"},
                        "_meta":{"claudecode/toolUseId":"toolu_123"}
                    }
                }),
                &mcp_fixture_manifest(),
                &sock,
            )
            .await
            .unwrap();
            assert_eq!(
                response["result"]["content"],
                serde_json::json!([{"type":"text","text":"{\"saved\":true}"}])
            );
            server.await.unwrap();
            let _ = tokio::fs::remove_file(sock).await;
        }

        async fn spawn_main_mcp_server() -> (
            String,
            mcp_server::ResultsInHand,
            mcp_server::ParkedCalls,
            mpsc::Receiver<HarnessEvent>,
        ) {
            let sock = std::env::temp_dir()
                .join(format!(
                    "engram-mcp-main-test-{}.sock",
                    uuid::Uuid::new_v4()
                ))
                .to_string_lossy()
                .into_owned();
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let results: mcp_server::ResultsInHand =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let parked: mcp_server::ParkedCalls = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let run_id: hook_server::CurrentRunId =
                Arc::new(tokio::sync::Mutex::new(Some("run-mcp".into())));
            let deferred: hook_server::DeferredCalls =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let (evt_tx, evt_rx) = mpsc::channel(16);
            tokio::spawn(mcp_server::serve(
                listener,
                mcp_server::State {
                    results_in_hand: results.clone(),
                    parked_calls: parked.clone(),
                    deferred_calls: deferred,
                    current_run_id: run_id,
                    evt_tx,
                },
            ));
            (sock, results, parked, evt_rx)
        }

        async fn fire_main_mcp_call(sock: &str, call_id: &str) -> Value {
            let stream = tokio::net::UnixStream::connect(sock).await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut line = serde_json::to_vec(&serde_json::json!({
                "name": "save_memory",
                "args": {"text": "hello"},
                "tool_use_id": call_id
            }))
            .unwrap();
            line.push(b'\n');
            w.write_all(&line).await.unwrap();
            w.flush().await.unwrap();
            let response = BufReader::new(r)
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            serde_json::from_str(&response).unwrap()
        }

        #[tokio::test]
        async fn main_mcp_sync_call_parks_then_resolves_on_tool_result() {
            let (sock, results, parked, mut evt_rx) = spawn_main_mcp_server().await;
            let call_sock = sock.clone();
            let call =
                tokio::spawn(async move { fire_main_mcp_call(&call_sock, "toolu_sync").await });
            match evt_rx.recv().await {
                Some(HarnessEvent::ToolCallRequested {
                    run_id,
                    call_id,
                    name,
                    args_json,
                }) => {
                    assert_eq!(run_id, "run-mcp");
                    assert_eq!(call_id, "toolu_sync");
                    assert_eq!(name, "save_memory");
                    assert_eq!(
                        serde_json::from_str::<Value>(&args_json).unwrap(),
                        serde_json::json!({"text":"hello"})
                    );
                }
                other => panic!("expected ToolCallRequested, got {other:?}"),
            }
            assert!(
                mcp_server::route_tool_result(
                    "toolu_sync",
                    r#"{"saved":true}"#.into(),
                    &parked,
                    &results,
                )
                .await,
                "a parked connection was resolved"
            );
            assert_eq!(
                call.await.unwrap()["result_json"],
                serde_json::json!(r#"{"saved":true}"#)
            );
            assert!(results.lock().await.is_empty());
            let _ = tokio::fs::remove_file(sock).await;
        }

        #[tokio::test]
        async fn main_mcp_refire_consumes_result_in_hand_immediately() {
            let (sock, results, _parked, mut evt_rx) = spawn_main_mcp_server().await;
            results
                .lock()
                .await
                .insert("toolu_refire".into(), r#"{"saved":true}"#.into());
            let response = fire_main_mcp_call(&sock, "toolu_refire").await;
            assert_eq!(response["result_json"], r#"{"saved":true}"#);
            assert!(
                results.lock().await.is_empty(),
                "stash consumed exactly once"
            );
            assert!(
                evt_rx.try_recv().is_err(),
                "the re-fire must not emit a duplicate ToolCallRequested"
            );
            let _ = tokio::fs::remove_file(sock).await;
        }

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

        // Regression (session b9b28452): a host-agent roll answers the
        // harness's re-attach with "no sandbox bound to this session_id"
        // while it's still reattaching the survivor VM. That rejection is
        // TRANSIENT — the reconnect loop must back off and retry, never
        // exit. The old `Rejected => break Some(exit)` arm turned it
        // terminal: the VM survived the roll but the harness died and the
        // session wedged `active` forever. Only the engine finishing on its
        // own may stop the loop.
        #[test]
        fn attach_rejection_retries_and_never_ends_the_loop() {
            let rejected = ConnOutcome::Rejected {
                reason: "no sandbox bound to this session_id".into(),
            };
            assert_eq!(rejected.reconnect(), Reconnect::Backoff);
            assert_eq!(rejected.reason(), "no sandbox bound to this session_id");

            // The other transport failures retry too; only EngineDone stops.
            assert_eq!(
                ConnOutcome::HandshakeFailed { reason: "ack_read" }.reconnect(),
                Reconnect::Backoff,
            );
            assert_eq!(
                ConnOutcome::Dropped { reason: "eof" }.reconnect(),
                Reconnect::Settle,
            );
            assert_eq!(ConnOutcome::EngineDone.reconnect(), Reconnect::Stop);
            assert_eq!(ConnOutcome::Superseded.reconnect(), Reconnect::Stop);
        }

        // ADR 0054 Part C: the scrub removes exactly the suppressed
        // narrate-past message(s) by id, keeps the deferred tool_use, and
        // re-links any survivor whose parent it removed.
        #[test]
        fn scrub_removes_narrate_past_and_relinks() {
            let dir = std::env::temp_dir().join(format!("engram-scrub-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("sess.jsonl");
            let lines = [
                r#"{"type":"user","uuid":"u-prompt","parentUuid":null,"message":{"role":"user","content":[{"type":"text","text":"ask me red or green"}]}}"#,
                r#"{"type":"assistant","uuid":"u-A","parentUuid":"u-prompt","message":{"id":"msg_AAA","role":"assistant","content":[{"type":"tool_use","id":"toolu_X","name":"AskUserQuestion","input":{"questions":[]}}]}}"#,
                r#"{"type":"assistant","uuid":"u-B","parentUuid":"u-A","message":{"id":"msg_BBB","role":"assistant","content":[{"type":"text","text":"It seems the tool encountered an internal error."}]}}"#,
                r#"{"type":"assistant","uuid":"u-C","parentUuid":"u-B","message":{"id":"msg_CCC","role":"assistant","content":[{"type":"text","text":"trailing"}]}}"#,
            ];
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();

            let mut ids = HashSet::new();
            ids.insert("msg_BBB".to_string());
            let removed = scrub_transcript(&path, &ids, &HashSet::new()).unwrap();
            assert_eq!(removed, 1, "exactly one narrate-past message removed");

            let after = std::fs::read_to_string(&path).unwrap();
            assert!(!after.contains("msg_BBB"), "narrate-past message gone");
            assert!(!after.contains("internal error"), "narration text gone");
            assert!(
                after.contains("msg_AAA"),
                "the tool_use message is preserved"
            );
            assert!(
                after.contains("toolu_X"),
                "the deferred tool_use_id is preserved"
            );

            // u-C pointed at the removed u-B → must be re-linked to u-B's parent (u-A).
            let c = after
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .find(|v| v["uuid"] == "u-C")
                .unwrap();
            assert_eq!(
                c["parentUuid"], "u-A",
                "survivor re-linked past the removed line"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        // A transcript with none of the suppressed ids is left byte-for-byte
        // untouched (clean defer / nothing to scrub).
        #[test]
        fn scrub_is_noop_when_nothing_matches() {
            let dir =
                std::env::temp_dir().join(format!("engram-scrub-noop-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("sess.jsonl");
            let body = "{\"type\":\"assistant\",\"uuid\":\"u-A\",\"parentUuid\":null,\"message\":{\"id\":\"msg_AAA\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_X\",\"name\":\"AskUserQuestion\"}]}}\n";
            std::fs::write(&path, body).unwrap();

            let mut ids = HashSet::new();
            ids.insert("msg_NOPE".to_string());
            let removed = scrub_transcript(&path, &ids, &HashSet::new()).unwrap();
            assert_eq!(removed, 0);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                body,
                "file is untouched on a no-op scrub",
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        // ADR 0054: the scrub also drops a DUPLICATE AUQ's assistant message
        // — matched by the suppressed `tool_use` id, even in a different run
        // (the cross-run #64389 double-fire) — keeping the FIRST question's
        // tool_use and re-linking survivors, so resume re-fires exactly one.
        #[test]
        fn scrub_removes_duplicate_auq_tool_use() {
            let dir = std::env::temp_dir().join(format!("engram-scrub-dup-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("sess.jsonl");
            let lines = [
                r#"{"type":"user","uuid":"u-prompt","parentUuid":null,"message":{"role":"user","content":[{"type":"text","text":"ask me red or green"}]}}"#,
                // run A: the carded (first) question — kept.
                r#"{"type":"assistant","uuid":"u-A","parentUuid":"u-prompt","message":{"id":"msg_AAA","role":"assistant","content":[{"type":"tool_use","id":"toolu_FIRST","name":"AskUserQuestion","input":{"questions":[]}}]}}"#,
                // run B: the cross-run duplicate — dropped by its tool_use id.
                r#"{"type":"assistant","uuid":"u-B","parentUuid":"u-A","message":{"id":"msg_BBB","role":"assistant","content":[{"type":"tool_use","id":"toolu_DUP","name":"AskUserQuestion","input":{"questions":[]}}]}}"#,
                r#"{"type":"assistant","uuid":"u-C","parentUuid":"u-B","message":{"id":"msg_CCC","role":"assistant","content":[{"type":"text","text":"trailing"}]}}"#,
            ];
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();

            let mut dup = HashSet::new();
            dup.insert("toolu_DUP".to_string());
            let removed = scrub_transcript(&path, &HashSet::new(), &dup).unwrap();
            assert_eq!(removed, 1, "exactly the duplicate AUQ message removed");

            let after = std::fs::read_to_string(&path).unwrap();
            assert!(!after.contains("toolu_DUP"), "duplicate tool_use gone");
            assert!(
                after.contains("toolu_FIRST"),
                "the first (carded) question is preserved"
            );
            let c = after
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .find(|v| v["uuid"] == "u-C")
                .unwrap();
            assert_eq!(
                c["parentUuid"], "u-A",
                "survivor re-linked past the removed duplicate"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn scrub_removes_duplicate_manifest_tool_use() {
            let dir = std::env::temp_dir().join(format!(
                "engram-scrub-manifest-dup-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("sess.jsonl");
            let lines = [
                r#"{"type":"user","uuid":"u-prompt","parentUuid":null,"message":{"role":"user","content":[{"type":"text","text":"remember this"}]}}"#,
                r#"{"type":"assistant","uuid":"u-A","parentUuid":"u-prompt","message":{"id":"msg_AAA","role":"assistant","content":[{"type":"tool_use","id":"toolu_FIRST","name":"mcp__engrams__save_memory","input":{"text":"remember"}}]}}"#,
                r#"{"type":"assistant","uuid":"u-B","parentUuid":"u-A","message":{"id":"msg_BBB","role":"assistant","content":[{"type":"tool_use","id":"toolu_DUP","name":"mcp__engrams__save_memory","input":{"text":"remember"}}]}}"#,
                r#"{"type":"assistant","uuid":"u-C","parentUuid":"u-B","message":{"id":"msg_CCC","role":"assistant","content":[{"type":"text","text":"trailing"}]}}"#,
            ];
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();

            let duplicates = HashSet::from(["toolu_DUP".to_string()]);
            let removed = scrub_transcript(&path, &HashSet::new(), &duplicates).unwrap();
            assert_eq!(
                removed, 1,
                "exactly the duplicate manifest tool message is removed"
            );

            let after = std::fs::read_to_string(&path).unwrap();
            assert!(!after.contains("toolu_DUP"), "duplicate tool_use gone");
            assert!(
                after.contains("toolu_FIRST"),
                "the original deferred manifest tool_use is preserved"
            );
            let trailing = after
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find(|value| value["uuid"] == "u-C")
                .unwrap();
            assert_eq!(
                trailing["parentUuid"], "u-A",
                "survivor re-linked past the removed duplicate"
            );
            let _ = std::fs::remove_dir_all(&dir);
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
                // ADR 0073: the test's sole binding generation.
                sandbox_id: engram_core::SandboxId::new(),
                binding_epoch: 1,
                max_tool_calls: 100_000,
                max_tool_call_secs: 600,
                max_run_secs: 86_400,
                claude_bin: Some(claude_bin),
                hook_sock_path: None,
                mcp_sock_path: None,
                tool_manifest: Vec::new(),
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
            ));

            // Issue #535 (d): the pending queue starts empty, so the engine
            // announces `Idle` before it ever sees a prompt (there's no more
            // env-seeded entry to kick off with no leading Idle). Deliver
            // "the initial prompt" the same way every other prompt arrives,
            // as a `HarnessCommand::Prompt` frame.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "first".into(),
                })
                .await
                .unwrap();

            // Turn 1 is now in flight (the fake sleeps 300ms before its result).
            let (r1, pid1) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(
                pid1,
                Some("p1".to_string()),
                "prompt_id round-trips onto RunStarted"
            );

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

        // ADR 0052 Phase 4 (warm mid-turn teleport): a live host-to-host move
        // drops + re-dials the vsock WHILE a turn is generating (agentd SIGUSR1s
        // the moved harness → it reconnects). The engine is connection-decoupled
        // (the `turn`/cmd loop + the persistent claude child outlive any
        // connection), so the in-flight turn MUST survive the bounce: the
        // reattach must NOT synthesize an `Idle` mid-turn, and the run that was
        // open before the bounce is the same one that completes after it —
        // exactly one RunStarted→RunCompleted, no restart. This is the
        // engine-side guarantee behind the no-turn-restart-on-teleport promise
        // (the FC-level pipe survival is proven by the two-host teleport suite).
        #[tokio::test]
        async fn connection_bounce_mid_turn_preserves_the_run() {
            // One turn: sleep 300ms (stays in flight), then one assistant line
            // + result.
            let script = write_slow_fake_claude(
                &[
                    r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"ok"}]}}"#,
                    r#"{"type":"result","subtype":"success","is_error":false}"#,
                ],
                300,
            )
            .await;

            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(8);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(
                test_cli(script.clone()),
                cmd_rx,
                reattach.clone(),
                evt_tx,
            ));

            // Issue #535 (d): starts Idle (empty pending queue), then the
            // initial prompt arrives as an ordinary `HarnessCommand::Prompt`.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "first".into(),
                })
                .await
                .unwrap();

            // Turn 1 is now in flight (the fake sleeps 300ms before its result).
            let r1 = expect_run_started(&mut evt_rx).await;

            // The teleport connection bounce, mid-turn: a fresh connection
            // attaches — exactly what the post-move SIGUSR1 re-dial drives. Fire
            // it twice (re-dials can retry). While a turn is open the reattach
            // arm must emit nothing.
            reattach.notify_one();
            reattach.notify_one();

            // The very next events are the turn's own assistant line and its
            // RunCompleted — NO `Idle` injected by the bounce. (If the reattach
            // had synthesized one mid-turn, this `expect_agent_message` would see
            // it and panic.)
            expect_agent_message(&mut evt_rx, "ok").await;
            let c1 = expect_run_completed(&mut evt_rx).await;
            assert_eq!(
                r1, c1,
                "the run open before the bounce is the same one that completes after it — no restart",
            );

            // Now genuinely idle: the engine re-announces Idle. Clean shutdown.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
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

        // ADR 0052: the host re-delivers un-confirmed prompts on every
        // reattach (command-side at-least-once, the twin of the `held`
        // event slot). A replay of a prompt the harness already accepted
        // must be a no-op — never a second run — so the engine dedupes on
        // prompt_id.
        /// A fake `claude` that models the Phase 3 in-band interrupt: a
        /// `user` line emits assistant text but NO result (the turn stays IN
        /// FLIGHT — "generating"); a `control_request` line emits the abort
        /// (a `control_response` ack + a `result error_during_execution`)
        /// WITHOUT exiting — exactly claude's control-frame interrupt. It
        /// records its PID on each launch so a test can prove whether the
        /// process was respawned.
        async fn write_interrupt_aware_fake_claude(pidfile: &str) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let mut body = String::from("#!/bin/sh\n");
            body.push_str(&format!("echo $$ > '{pidfile}'\n"));
            body.push_str("printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\"}'\n");
            body.push_str("while IFS= read -r line; do\n");
            body.push_str("  case \"$line\" in\n");
            body.push_str("    *control_request*)\n");
            body.push_str("      printf '%s\\n' '{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\"}}'\n");
            body.push_str("      printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true}'\n");
            body.push_str("      ;;\n");
            body.push_str("    *)\n");
            body.push_str("      printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"id\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}'\n");
            body.push_str("      ;;\n");
            body.push_str("  esac\n");
            body.push_str("done\n");
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        fn temp_pidfile() -> String {
            std::env::temp_dir()
                .join(format!("fake-pid-{}", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned()
        }

        async fn read_pid(pidfile: &str) -> String {
            tokio::fs::read_to_string(pidfile)
                .await
                .unwrap()
                .trim()
                .to_string()
        }

        // Phase 3 (ADR 0030): an interrupt sends an in-band `control_request`;
        // claude aborts the turn (`result error_during_execution`) and STAYS
        // ALIVE. The engine reports RunInterrupted (not a crash) and does NOT
        // respawn — proven by the unchanged claude PID.
        #[tokio::test]
        async fn control_request_interrupt_aborts_turn_without_respawn() {
            let pidfile = temp_pidfile();
            let script = write_interrupt_aware_fake_claude(&pidfile).await;

            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(8);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(
                test_cli(script.clone()),
                cmd_rx,
                reattach.clone(),
                evt_tx,
            ));

            // Issue #535 (d): starts Idle (empty pending queue), then the
            // initial prompt arrives as an ordinary `HarnessCommand::Prompt`.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "first".into(),
                })
                .await
                .unwrap();

            // Turn 1 is in flight: RunStarted + assistant text, no result yet.
            let r1 = expect_run_started(&mut evt_rx).await;
            expect_agent_message(&mut evt_rx, "working").await;
            let pid_before = read_pid(&pidfile).await;

            // Interrupt → control_request → the abort `result` → RunInterrupted.
            cmd_tx.send(HarnessCommand::Interrupt).await.unwrap();
            match evt_rx.recv().await {
                Some(HarnessEvent::RunInterrupted { run_id }) => assert_eq!(run_id, r1),
                other => panic!("expected RunInterrupted, got {other:?}"),
            }
            // No queued prompt → Idle (NOT a crash / RunCompleted{ok:false}).
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // The decisive assertion: claude was NOT respawned — same warm
            // process, so conversation context is intact.
            assert_eq!(
                pid_before,
                read_pid(&pidfile).await,
                "control_request interrupt must keep claude alive (no respawn)",
            );

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 5 })
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), engine).await;
            let _ = tokio::fs::remove_file(&script).await;
            let _ = tokio::fs::remove_file(&pidfile).await;
        }

        // Phase 3 regression for session ba3ae8d5: interrupting WITH a queued
        // message must abort the turn and run the queued message on the SAME
        // warm process — RunInterrupted then RunStarted{queued}, no respawn,
        // and crucially NO abnormal-exit / RunCompleted{ok:false}. (The old
        // SIGINT path respawned `--resume` into a context-less process and
        // misread its immediate exit as a crash — the "An error occurred".)
        #[tokio::test]
        async fn interrupt_with_queued_message_steers_on_same_process() {
            let pidfile = temp_pidfile();
            let script = write_interrupt_aware_fake_claude(&pidfile).await;

            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(16);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(
                test_cli(script.clone()),
                cmd_rx,
                reattach.clone(),
                evt_tx,
            ));

            // Issue #535 (d): starts Idle (empty pending queue), then the
            // initial prompt arrives as an ordinary `HarnessCommand::Prompt`.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "first".into(),
                })
                .await
                .unwrap();

            // Turn 1 in flight.
            let r1 = expect_run_started(&mut evt_rx).await;
            expect_agent_message(&mut evt_rx, "working").await;
            let pid_before = read_pid(&pidfile).await;

            // Queue a message mid-turn, then interrupt.
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
            cmd_tx.send(HarnessCommand::Interrupt).await.unwrap();

            // Turn 1 aborts cleanly...
            match evt_rx.recv().await {
                Some(HarnessEvent::RunInterrupted { run_id }) => assert_eq!(run_id, r1),
                other => panic!("expected RunInterrupted, got {other:?}"),
            }
            // ...and the queued message runs as the NEXT turn — a fresh run_id
            // carrying its prompt_id, on the same process. The very next events
            // are RunStarted{p2} + its assistant text: no crash artifact, no
            // RunCompleted{ok:false} ever appears (the bug).
            let (r2, pid2) = expect_run_started_id(&mut evt_rx).await;
            assert_ne!(r1, r2, "the steered turn gets a fresh run_id");
            assert_eq!(
                pid2,
                Some("p2".to_string()),
                "the queued prompt is consumed"
            );
            expect_agent_message(&mut evt_rx, "working").await;

            assert_eq!(
                pid_before,
                read_pid(&pidfile).await,
                "interrupt + queued message must run on the same warm process",
            );

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 5 })
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), engine).await;
            let _ = tokio::fs::remove_file(&script).await;
            let _ = tokio::fs::remove_file(&pidfile).await;
        }

        #[tokio::test]
        async fn duplicate_prompt_replay_is_ignored() {
            let script = write_persistent_fake_claude(&[
                r#"{"type":"assistant","message":{"id":"m","content":[{"type":"text","text":"ok"}]}}"#,
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
            ));

            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // p1 runs to completion.
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "hi".into(),
                })
                .await
                .unwrap();
            let (_r1, pid1) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(pid1.as_deref(), Some("p1"));
            expect_agent_message(&mut evt_rx, "ok").await;
            let _ = expect_run_completed(&mut evt_rx).await;
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // Replay p1 (a duplicate from the host's at-least-once path),
            // then send a fresh p2. The duplicate must NOT start a run — the
            // next RunStarted we observe must be p2's, proving p1 was deduped.
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "hi".into(),
                })
                .await
                .unwrap();
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p2".into(),
                    text: "yo".into(),
                })
                .await
                .unwrap();
            let (_r2, pid2) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(
                pid2.as_deref(),
                Some("p2"),
                "the duplicate p1 must be ignored; the next run is p2",
            );

            // Closing the command channel ends the engine cleanly.
            drop(cmd_tx);
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine should exit when the command channel closes")
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
            ));

            // Issue #535 (d): starts Idle (empty pending queue), then the
            // initial prompt arrives as an ordinary `HarnessCommand::Prompt`.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p1".into(),
                    text: "first".into(),
                })
                .await
                .unwrap();

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

        // ── ADR 0054 Flavor B: hook↔harness socket + answer-resume ──────

        fn sample_q(text: &str, multi: bool) -> Question {
            Question {
                question: text.into(),
                header: "H".into(),
                multi_select: multi,
                options: vec![QuestionOption {
                    label: "A".into(),
                    description: "a".into(),
                }],
            }
        }

        /// Spin up `hook_server::serve` on a fresh, isolated temp socket (NOT
        /// the production `/workspace` path — tests run in parallel). Returns
        /// the path plus the shared state and the event receiver.
        async fn spawn_hook_server() -> (
            String,
            hook_server::AnswersInHand,
            hook_server::CurrentRunId,
            mpsc::Receiver<HarnessEvent>,
            hook_server::DuplicateDeferredIds,
        ) {
            let sock = std::env::temp_dir()
                .join(format!("engram-hooktest-{}.sock", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned();
            let answers: hook_server::AnswersInHand =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let run_id: hook_server::CurrentRunId =
                Arc::new(tokio::sync::Mutex::new(Some("run-x".into())));
            let outstanding: hook_server::QuestionOutstanding =
                Arc::new(tokio::sync::Mutex::new(false));
            let dup_ids: hook_server::DuplicateDeferredIds =
                Arc::new(tokio::sync::Mutex::new(HashSet::new()));
            let results: mcp_server::ResultsInHand =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let deferred: hook_server::DeferredCalls =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let (evt_tx, evt_rx) = mpsc::channel::<HarnessEvent>(16);
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            tokio::spawn(hook_server::serve(
                listener,
                hook_server::State {
                    answers_in_hand: answers.clone(),
                    current_run_id: run_id.clone(),
                    evt_tx,
                    question_outstanding: outstanding,
                    duplicate_deferred_ids: dup_ids.clone(),
                    manifest: Arc::new(Vec::new()),
                    results_in_hand: results,
                    deferred_calls: deferred,
                },
            ));
            (sock, answers, run_id, evt_rx, dup_ids)
        }

        async fn spawn_manifest_hook_server(
            manifest: ToolManifest,
        ) -> (
            String,
            mcp_server::ResultsInHand,
            hook_server::DeferredCalls,
            mpsc::Receiver<HarnessEvent>,
            hook_server::DuplicateDeferredIds,
        ) {
            let sock = std::env::temp_dir()
                .join(format!(
                    "engram-hook-manifest-{}.sock",
                    uuid::Uuid::new_v4()
                ))
                .to_string_lossy()
                .into_owned();
            let answers: hook_server::AnswersInHand =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let results: mcp_server::ResultsInHand =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let pending: hook_server::DeferredCalls =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let run_id: hook_server::CurrentRunId =
                Arc::new(tokio::sync::Mutex::new(Some("run-x".into())));
            let outstanding: hook_server::QuestionOutstanding =
                Arc::new(tokio::sync::Mutex::new(false));
            let duplicates: hook_server::DuplicateDeferredIds =
                Arc::new(tokio::sync::Mutex::new(HashSet::new()));
            let (evt_tx, evt_rx) = mpsc::channel(16);
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            tokio::spawn(hook_server::serve(
                listener,
                hook_server::State {
                    answers_in_hand: answers,
                    current_run_id: run_id,
                    evt_tx,
                    question_outstanding: outstanding,
                    duplicate_deferred_ids: duplicates.clone(),
                    manifest: Arc::new(manifest),
                    results_in_hand: results.clone(),
                    deferred_calls: pending.clone(),
                },
            ));
            (sock, results, pending, evt_rx, duplicates)
        }

        /// A fake `PreToolUse` hook client: one request line, one verdict.
        async fn hook_fire(
            sock: &str,
            tool_use_id: &str,
            questions: &[Question],
        ) -> hook_server::HookVerdict {
            let stream = tokio::net::UnixStream::connect(sock).await.unwrap();
            let (r, mut w) = stream.into_split();
            let req = serde_json::json!({ "tool_use_id": tool_use_id, "questions": questions });
            let mut line = serde_json::to_string(&req).unwrap();
            line.push('\n');
            w.write_all(line.as_bytes()).await.unwrap();
            w.flush().await.unwrap();
            let mut lines = BufReader::new(r).lines();
            let resp = lines.next_line().await.unwrap().unwrap();
            serde_json::from_str(&resp).unwrap()
        }

        async fn hook_fire_named(
            sock: &str,
            tool_use_id: &str,
            tool_name: &str,
            tool_input: Value,
        ) -> hook_server::HookVerdict {
            let stream = tokio::net::UnixStream::connect(sock).await.unwrap();
            let (r, mut w) = stream.into_split();
            let request = serde_json::json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "tool_input": tool_input,
                "questions": []
            });
            let mut line = serde_json::to_vec(&request).unwrap();
            line.push(b'\n');
            w.write_all(&line).await.unwrap();
            w.flush().await.unwrap();
            let response = BufReader::new(r)
                .lines()
                .next_line()
                .await
                .unwrap()
                .unwrap();
            serde_json::from_str(&response).unwrap()
        }

        fn generic_tool(name: &str, execution: ToolExecution) -> ManifestTool {
            ManifestTool {
                name: name.into(),
                description: format!("{name} description"),
                input_schema: serde_json::json!({"type":"object"}),
                execution,
                native_bindings: NativeBindings::default(),
            }
        }

        #[tokio::test]
        async fn hook_defers_manifest_deferred_tool_and_emits_request() {
            let manifest = vec![generic_tool("save_memory", ToolExecution::Deferred)];
            let (sock, results, pending, mut evt_rx, _duplicates) =
                spawn_manifest_hook_server(manifest).await;
            assert!(matches!(
                hook_fire_named(
                    &sock,
                    "toolu_deferred",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Defer
            ));
            match evt_rx.recv().await {
                Some(HarnessEvent::ToolCallRequested {
                    run_id,
                    call_id,
                    name,
                    args_json,
                }) => {
                    assert_eq!(run_id, "run-x");
                    assert_eq!(call_id, "toolu_deferred");
                    assert_eq!(name, "save_memory");
                    assert_eq!(
                        serde_json::from_str::<Value>(&args_json).unwrap(),
                        serde_json::json!({"text":"remember"})
                    );
                }
                other => panic!("expected ToolCallRequested, got {other:?}"),
            }
            assert!(pending.lock().await.contains_key("toolu_deferred"));
            assert!(results.lock().await.is_empty());
            let _ = tokio::fs::remove_file(sock).await;
        }

        #[tokio::test]
        async fn hook_dedups_duplicate_manifest_deferred_tool_in_same_turn() {
            let manifest = vec![generic_tool("save_memory", ToolExecution::Deferred)];
            let (sock, _results, pending, mut evt_rx, duplicates) =
                spawn_manifest_hook_server(manifest).await;

            assert!(matches!(
                hook_fire_named(
                    &sock,
                    "toolu_first",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Defer
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::ToolCallRequested { call_id, .. })
                    if call_id == "toolu_first"
            ));

            assert!(matches!(
                hook_fire_named(
                    &sock,
                    "toolu_duplicate",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Defer
            ));
            assert!(
                evt_rx.try_recv().is_err(),
                "the duplicate must not emit another ToolCallRequested"
            );
            assert_eq!(
                *duplicates.lock().await,
                HashSet::from(["toolu_duplicate".to_string()]),
                "the duplicate id is recorded for transcript scrubbing"
            );
            assert_eq!(
                pending.lock().await.keys().cloned().collect::<HashSet<_>>(),
                HashSet::from(["toolu_first".to_string()]),
                "only the original host-visible call remains outstanding"
            );
            let _ = tokio::fs::remove_file(sock).await;
        }

        #[tokio::test]
        async fn hook_allows_manifest_sync_tool_without_emitting() {
            let manifest = vec![generic_tool("save_memory", ToolExecution::Sync)];
            let (sock, _results, pending, mut evt_rx, _duplicates) =
                spawn_manifest_hook_server(manifest).await;
            assert!(matches!(
                hook_fire_named(
                    &sock,
                    "toolu_sync",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Allow
            ));
            assert!(pending.lock().await.is_empty());
            assert!(evt_rx.try_recv().is_err());
            let _ = tokio::fs::remove_file(sock).await;
        }

        #[tokio::test]
        async fn hook_never_defers_tool_search() {
            let manifest = vec![generic_tool("save_memory", ToolExecution::Deferred)];
            let (sock, _results, pending, mut evt_rx, _duplicates) =
                spawn_manifest_hook_server(manifest).await;
            assert!(matches!(
                hook_fire_named(
                    &sock,
                    "toolu_search",
                    "ToolSearch",
                    serde_json::json!({"query":"save_memory"}),
                )
                .await,
                hook_server::HookVerdict::Allow
            ));
            assert!(pending.lock().await.is_empty());
            assert!(evt_rx.try_recv().is_err());
            let _ = tokio::fs::remove_file(sock).await;
        }

        #[tokio::test]
        async fn hook_allows_deferred_refire_with_result_in_hand() {
            let manifest = vec![generic_tool("save_memory", ToolExecution::Deferred)];
            let (sock, results, pending, mut evt_rx, _duplicates) =
                spawn_manifest_hook_server(manifest).await;
            results
                .lock()
                .await
                .insert("toolu_refire".into(), r#"{"saved":true}"#.into());
            assert!(matches!(
                hook_fire_named(
                    &sock,
                    "toolu_refire",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Allow
            ));
            assert!(
                results.lock().await.contains_key("toolu_refire"),
                "the MCP bridge, not the hook, consumes the result stash"
            );
            assert!(pending.lock().await.is_empty());
            assert!(evt_rx.try_recv().is_err());
            let _ = tokio::fs::remove_file(sock).await;
        }

        // No answer in hand → defer + UserQuestion. After an answer is
        // stashed, a re-fire of the SAME tool_use_id → answer +
        // QuestionAnswered.
        #[tokio::test]
        async fn hook_defers_then_answers_after_stash() {
            let (sock, answers, _rid, mut evt_rx, _dup_ids) = spawn_hook_server().await;
            let questions = vec![sample_q("Pick one?", false)];

            let v = hook_fire(&sock, "toolu_1", &questions).await;
            assert!(matches!(v, hook_server::HookVerdict::Defer));
            match evt_rx.recv().await {
                Some(HarnessEvent::UserQuestion {
                    run_id,
                    tool_call_id,
                    questions: qs,
                }) => {
                    assert_eq!(run_id, "run-x");
                    assert_eq!(tool_call_id, "toolu_1");
                    assert_eq!(qs.len(), 1);
                }
                other => panic!("expected UserQuestion, got {other:?}"),
            }

            {
                let mut m = answers.lock().await;
                let mut a = Answers::new();
                a.insert("Pick one?".into(), vec!["A".into()]);
                m.insert("toolu_1".into(), a);
            }
            let v = hook_fire(&sock, "toolu_1", &questions).await;
            match v {
                hook_server::HookVerdict::Answer { answers } => {
                    assert_eq!(answers.get("Pick one?"), Some(&vec!["A".to_string()]));
                }
                _ => panic!("expected answer after stash"),
            }
            match evt_rx.recv().await {
                Some(HarnessEvent::QuestionAnswered {
                    tool_call_id,
                    answers,
                    ..
                }) => {
                    assert_eq!(tool_call_id, "toolu_1");
                    assert_eq!(answers.get("Pick one?"), Some(&vec!["A".to_string()]));
                }
                other => panic!("expected QuestionAnswered, got {other:?}"),
            }
        }

        // One AUQ call carrying TWO questions (one multi, one single) → one
        // request, one answers map keyed by question text (finding #8).
        #[tokio::test]
        async fn hook_multi_question_one_roundtrip() {
            let (sock, answers, _rid, mut evt_rx, _dup_ids) = spawn_hook_server().await;
            let questions = vec![sample_q("Languages?", true), sample_q("Editor?", false)];
            {
                let mut m = answers.lock().await;
                let mut a = Answers::new();
                a.insert("Languages?".into(), vec!["Python".into(), "Rust".into()]);
                a.insert("Editor?".into(), vec!["VS Code".into()]);
                m.insert("toolu_multi".into(), a);
            }
            match hook_fire(&sock, "toolu_multi", &questions).await {
                hook_server::HookVerdict::Answer { answers } => {
                    assert_eq!(answers.len(), 2, "one answers map for N questions");
                    assert_eq!(answers.get("Languages?").unwrap().len(), 2);
                    assert_eq!(
                        answers.get("Editor?").unwrap(),
                        &vec!["VS Code".to_string()]
                    );
                }
                _ => panic!("expected answer"),
            }
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::QuestionAnswered { .. })
            ));
        }

        // Idempotency: the deferred tool yields exactly one tool_result, so
        // a duplicate re-fire after consumption DEFERS (consumed once).
        #[tokio::test]
        async fn hook_duplicate_refire_defers_after_consumption() {
            let (sock, answers, _rid, mut evt_rx, _dup_ids) = spawn_hook_server().await;
            let questions = vec![sample_q("One?", false)];
            {
                let mut m = answers.lock().await;
                let mut a = Answers::new();
                a.insert("One?".into(), vec!["A".into()]);
                m.insert("toolu_dup".into(), a);
            }
            assert!(matches!(
                hook_fire(&sock, "toolu_dup", &questions).await,
                hook_server::HookVerdict::Answer { .. }
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::QuestionAnswered { .. })
            ));
            assert!(matches!(
                hook_fire(&sock, "toolu_dup", &questions).await,
                hook_server::HookVerdict::Defer
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::UserQuestion { .. })
            ));
            assert!(
                answers.lock().await.is_empty(),
                "the answer was consumed exactly once"
            );
        }

        // ADR 0054: SESSION-level dedup of the #64389 double-fire. The first
        // AUQ defers + cards and marks a question outstanding; any further AUQ
        // while one is outstanding — INCLUDING one in a different run (the
        // cross-run double-fire) — still DEFERS (parked, no deny poison) but
        // emits NO card and is recorded for the scrub. Once the outstanding
        // question is ANSWERED, the next genuine question is carded afresh.
        #[tokio::test]
        async fn hook_dedups_duplicate_auq_session_wide() {
            let (sock, answers, run_id, mut evt_rx, dup_ids) = spawn_hook_server().await;
            let questions = vec![sample_q("Color?", false)];

            // First AUQ in run-x → defer + card.
            assert!(matches!(
                hook_fire(&sock, "toolu_a", &questions).await,
                hook_server::HookVerdict::Defer
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::UserQuestion { .. })
            ));

            // Second, distinct AUQ in the SAME run → defer (parked), NO card,
            // and its id is recorded for the scrub.
            assert!(matches!(
                hook_fire(&sock, "toolu_b", &questions).await,
                hook_server::HookVerdict::Defer
            ));
            assert!(
                evt_rx.try_recv().is_err(),
                "the same-run duplicate emits no card"
            );

            // A duplicate in a DIFFERENT run (the cross-run double-fire) is
            // STILL deduped — the invariant is session-wide, not per-run.
            *run_id.lock().await = Some("run-y".into());
            assert!(matches!(
                hook_fire(&sock, "toolu_c", &questions).await,
                hook_server::HookVerdict::Defer
            ));
            assert!(
                evt_rx.try_recv().is_err(),
                "the cross-run duplicate emits no card either"
            );
            assert_eq!(
                *dup_ids.lock().await,
                HashSet::from(["toolu_b".to_string(), "toolu_c".to_string()]),
                "both duplicates (same- and cross-run) are recorded for the scrub"
            );

            // Answer the outstanding question (its id is the carded toolu_a) →
            // clears `question_outstanding`, so the NEXT genuine question cards.
            {
                let mut a = Answers::new();
                a.insert("Color?".into(), vec!["A".into()]);
                answers.lock().await.insert("toolu_a".to_string(), a);
            }
            assert!(matches!(
                hook_fire(&sock, "toolu_a", &questions).await,
                hook_server::HookVerdict::Answer { .. }
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::QuestionAnswered { .. })
            ));
            assert!(matches!(
                hook_fire(&sock, "toolu_d", &questions).await,
                hook_server::HookVerdict::Defer
            ));
            assert!(
                matches!(evt_rx.recv().await, Some(HarnessEvent::UserQuestion { .. })),
                "after the answer, a fresh question is carded again"
            );
        }

        // The hook-bridge's claude-specific denormalization (finding #6): a
        // single-select answer is a bare string, a multiSelect answer is an
        // array, keyed by question text — driven by each question's flag.
        #[test]
        fn denormalize_single_is_string_multi_is_array() {
            let questions = vec![sample_q("M?", true), sample_q("S?", false)];
            let mut a = Answers::new();
            a.insert("M?".into(), vec!["x".into(), "y".into()]);
            a.insert("S?".into(), vec!["z".into()]);
            let out = hook_bridge::denormalize(&questions, &a);
            assert_eq!(out["M?"], serde_json::json!(["x", "y"]), "multi → array");
            assert_eq!(out["S?"], serde_json::json!("z"), "single → bare string");
        }

        async fn write_deferred_refire_fake_claude(counter: &Path) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let body = format!(
                "#!/bin/sh\n\
                 n=0\n\
                 if [ -f '{counter}' ]; then n=$(sed -n '1p' '{counter}'); fi\n\
                 n=$((n + 1))\n\
                 printf '%s\\n' \"$n\" > '{counter}'\n\
                 printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\"}}'\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                   IFS= read -r _line\n\
                   sleep 0.3\n\
                   printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-defer\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_deferred\",\"name\":\"mcp__engrams__save_memory\",\"input\":{{\"text\":\"remember\"}}}}]}}}}'\n\
                   printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"terminal_reason\":\"tool_deferred\"}}'\n\
                   exit 0\n\
                 fi\n\
                 while IFS= read -r _line; do :; done\n",
                counter = counter.display(),
            );
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        async fn write_fresh_restore_refire_fake_claude(
            counter: &Path,
            invocations: &Path,
            completion_gate: &Path,
            session_id: &str,
        ) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let body = format!(
                "#!/bin/sh\n\
                 trap 'exit 0' INT TERM\n\
                 n=0\n\
                 if [ -f '{counter}' ]; then n=$(sed -n '1p' '{counter}'); fi\n\
                 n=$((n + 1))\n\
                 printf '%s\\n' \"$n\" > '{counter}'\n\
                 printf '%s\\n' \"$*\" >> '{invocations}'\n\
                 printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{session_id}\"}}'\n\
                 if [ \"$n\" -ge 2 ]; then\n\
                   printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-restored-refire\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_restored\",\"name\":\"mcp__engrams__save_memory\",\"input\":{{\"text\":\"remember\"}}}}]}}}}'\n\
                   while [ ! -f '{completion_gate}' ]; do sleep 0.05; done\n\
                   printf '%s\\n' '{{\"type\":\"user\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_restored\",\"content\":\"{{\\\"saved\\\":true}}\"}}]}}}}'\n\
                   printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-restored-done\",\"content\":[{{\"type\":\"text\",\"text\":\"continued after restore\"}}]}}}}'\n\
                   printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"terminal_reason\":\"completed\"}}'\n\
                 fi\n\
                 while IFS= read -r _line; do :; done\n",
                counter = counter.display(),
                invocations = invocations.display(),
                completion_gate = completion_gate.display(),
            );
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        async fn write_deferred_alive_refire_fake_claude(counter: &Path) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let body = format!(
                "#!/bin/sh\n\
                 trap 'exit 0' INT TERM\n\
                 n=0\n\
                 if [ -f '{counter}' ]; then n=$(sed -n '1p' '{counter}'); fi\n\
                 n=$((n + 1))\n\
                 printf '%s\\n' \"$n\" > '{counter}'\n\
                 printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\"}}'\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                   IFS= read -r _prompt\n\
                   sleep 0.3\n\
                   printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-defer\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_alive\",\"name\":\"mcp__engrams__save_memory\",\"input\":{{\"text\":\"remember\"}}}}]}}}}'\n\
                   printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"terminal_reason\":\"tool_deferred\"}}'\n\
                 fi\n\
                 while IFS= read -r _line; do :; done\n",
                counter = counter.display(),
            );
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        async fn write_deferred_fallback_fake_claude(counter: &Path, captured: &Path) -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            // Branch on --resume in argv, NOT on a spawn counter: the engine
            // SIGINTs the idle generation, which can die before a counter
            // increment lands (observed flake). The init line carries a
            // session_id so the engine can build the --resume respawn at all.
            let body = format!(
                "#!/bin/sh\n\
                 trap 'exit 0' INT TERM\n\
                 printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-fallback\"}}'\n\
                 case \"$*\" in\n\
                 *--resume*)\n\
                   printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-resumed\",\"content\":[{{\"type\":\"text\",\"text\":\"resumed without refire\"}}]}}}}'\n\
                   printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"terminal_reason\":\"completed\"}}'\n\
                   IFS= read -r delivered\n\
                   printf '%s\\n' \"$delivered\" > '{captured}'\n\
                   printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-fallback\",\"content\":[{{\"type\":\"text\",\"text\":\"got deferred result\"}}]}}}}'\n\
                   printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"terminal_reason\":\"completed\"}}'\n\
                   while IFS= read -r _l; do :; done\n\
                   ;;\n\
                 *)\n\
                   n=0\n\
                   if [ -f '{counter}' ]; then n=$(sed -n '1p' '{counter}'); fi\n\
                   n=$((n + 1))\n\
                   printf '%s\\n' \"$n\" > '{counter}'\n\
                   if [ \"$n\" -eq 1 ]; then\n\
                     IFS= read -r _prompt\n\
                     sleep 0.3\n\
                     printf '%s\\n' '{{\"type\":\"assistant\",\"message\":{{\"id\":\"m-defer\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_fallback\",\"name\":\"mcp__engrams__save_memory\",\"input\":{{\"text\":\"remember\"}}}}]}}}}'\n\
                     printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"terminal_reason\":\"tool_deferred\"}}'\n\
                     exit 0\n\
                   fi\n\
                   while :; do sleep 0.2; done\n\
                   ;;\n\
                 esac\n",
                counter = counter.display(),
                captured = captured.display(),
            );
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        async fn deferred_engine_cli(script: String, call_id: &str) -> (Cli, String, String) {
            let base = std::env::temp_dir().join(format!(
                "engram-deferred-{call_id}-{}",
                uuid::Uuid::new_v4()
            ));
            tokio::fs::create_dir_all(&base).await.unwrap();
            let hook = base.join("hook.sock").to_string_lossy().into_owned();
            let mcp = base.join("mcp.sock").to_string_lossy().into_owned();
            let mut cli = test_cli(script);
            cli.hook_sock_path = Some(hook.clone());
            cli.mcp_sock_path = Some(mcp.clone());
            cli.tool_manifest = vec![generic_tool("save_memory", ToolExecution::Deferred)];
            (cli, hook, mcp)
        }

        #[tokio::test]
        async fn deferred_result_after_process_death_resumes_and_serves_refire() {
            let counter =
                std::env::temp_dir().join(format!("fake-claude-count-{}", uuid::Uuid::new_v4()));
            let script = write_deferred_refire_fake_claude(&counter).await;
            let (cli, hook, mcp) = deferred_engine_cli(script.clone(), "refire").await;
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            let (evt_tx, mut evt_rx) = mpsc::channel(64);
            let engine = tokio::spawn(run_engine(cli, cmd_rx, Arc::new(Notify::new()), evt_tx));

            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p-deferred".into(),
                    text: "remember this".into(),
                })
                .await
                .unwrap();
            let _ = expect_run_started(&mut evt_rx).await;
            assert!(matches!(
                hook_fire_named(
                    &hook,
                    "toolu_deferred",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Defer
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::ToolCallRequested { call_id, .. })
                    if call_id == "toolu_deferred"
            ));

            // Drain the deferred turn's tool log/completion and both Idle
            // announcements: one at turn-end, one from the respawned process.
            let mut idle_count = 0;
            tokio::time::timeout(Duration::from_secs(8), async {
                while idle_count < 2 {
                    if matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)) {
                        idle_count += 1;
                    }
                }
            })
            .await
            .expect("the dead first process should respawn");

            cmd_tx
                .send(HarnessCommand::ToolResult {
                    call_id: "toolu_deferred".into(),
                    result_json: r#"{"saved":true}"#.into(),
                })
                .await
                .unwrap();
            let (_run, prompt_id) =
                tokio::time::timeout(Duration::from_secs(5), expect_run_started_id(&mut evt_rx))
                    .await
                    .expect("result for a dead generation should trigger a continuation resume");
            assert_eq!(prompt_id, None);
            assert!(matches!(
                hook_fire_named(
                    &hook,
                    "toolu_deferred",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Allow
            ));
            assert_eq!(
                fire_main_mcp_call(&mcp, "toolu_deferred").await["result_json"],
                r#"{"saved":true}"#
            );

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 1 })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine exits")
                .expect("engine task does not panic");
            let _ = tokio::fs::remove_file(script).await;
            let _ = tokio::fs::remove_file(counter).await;
        }

        #[tokio::test]
        async fn deferred_result_after_fresh_restore_resumes_and_serves_refire() {
            let nonce = uuid::Uuid::new_v4();
            let counter = std::env::temp_dir().join(format!("fake-claude-count-{nonce}"));
            let invocations = std::env::temp_dir().join(format!("fake-claude-invocations-{nonce}"));
            let completion_gate =
                std::env::temp_dir().join(format!("fake-claude-complete-{nonce}"));
            let session_id = format!("fresh-restore-{nonce}");
            let script = write_fresh_restore_refire_fake_claude(
                &counter,
                &invocations,
                &completion_gate,
                &session_id,
            )
            .await;
            let (cli, hook, mcp) = deferred_engine_cli(script.clone(), "fresh-restore").await;
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            let (evt_tx, mut evt_rx) = mpsc::channel(64);
            let engine = tokio::spawn(run_engine(cli, cmd_rx, Arc::new(Notify::new()), evt_tx));

            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if tokio::fs::read_to_string(CLAUDE_SESSION_ID_FILE)
                        .await
                        .is_ok_and(|contents| contents.trim() == session_id)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the fresh process should persist its resumable session id");

            // This is the post-restore boundary: the durable transcript knows
            // about toolu_restored, but this fresh harness has never seen its
            // hook, so deferred_calls is empty when the result arrives.
            cmd_tx
                .send(HarnessCommand::ToolResult {
                    call_id: "toolu_restored".into(),
                    result_json: r#"{"saved":true}"#.into(),
                })
                .await
                .unwrap();

            let (_run, prompt_id) =
                tokio::time::timeout(Duration::from_secs(5), expect_run_started_id(&mut evt_rx))
                    .await
                    .expect("result for a fresh engine should trigger a continuation resume");
            assert_eq!(prompt_id, None);
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if tokio::fs::read_to_string(&invocations)
                        .await
                        .is_ok_and(|contents| contents.lines().count() >= 2)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the resumed claude invocation should be recorded");
            let argv = tokio::fs::read_to_string(&invocations).await.unwrap();
            let resumed_argv = argv.lines().last().unwrap();
            assert!(
                resumed_argv.contains(&format!("--resume {session_id}")),
                "second claude invocation must resume the durable transcript: {resumed_argv}"
            );

            assert!(matches!(
                hook_fire_named(
                    &hook,
                    "toolu_restored",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Allow
            ));
            assert_eq!(
                fire_main_mcp_call(&mcp, "toolu_restored").await["result_json"],
                r#"{"saved":true}"#
            );
            tokio::fs::write(&completion_gate, b"served").await.unwrap();

            let mut continued = false;
            loop {
                match tokio::time::timeout(Duration::from_secs(5), evt_rx.recv())
                    .await
                    .expect("the resumed deferred turn should complete")
                {
                    Some(HarnessEvent::AgentMessage { text, .. })
                        if text == "continued after restore" =>
                    {
                        continued = true;
                    }
                    Some(HarnessEvent::RunCompleted { ok, .. }) => {
                        assert!(ok, "the resumed deferred turn should complete normally");
                        break;
                    }
                    Some(_) => {}
                    None => panic!("engine event channel closed before turn completion"),
                }
            }
            assert!(
                continued,
                "claude continued after consuming the stashed result"
            );

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 1 })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine exits")
                .expect("engine task does not panic");
            let _ = tokio::fs::remove_file(script).await;
            let _ = tokio::fs::remove_file(counter).await;
            let _ = tokio::fs::remove_file(invocations).await;
            let _ = tokio::fs::remove_file(completion_gate).await;
        }

        #[tokio::test]
        async fn deferred_result_with_live_process_resumes_and_serves_refire() {
            let counter =
                std::env::temp_dir().join(format!("fake-claude-count-{}", uuid::Uuid::new_v4()));
            let script = write_deferred_alive_refire_fake_claude(&counter).await;
            let (cli, hook, mcp) = deferred_engine_cli(script.clone(), "alive").await;
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            let (evt_tx, mut evt_rx) = mpsc::channel(64);
            let engine = tokio::spawn(run_engine(cli, cmd_rx, Arc::new(Notify::new()), evt_tx));

            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p-alive".into(),
                    text: "remember this".into(),
                })
                .await
                .unwrap();
            let _ = expect_run_started(&mut evt_rx).await;
            assert!(matches!(
                hook_fire_named(
                    &hook,
                    "toolu_alive",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Defer
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::ToolCallRequested { call_id, .. }) if call_id == "toolu_alive"
            ));
            while !matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)) {}

            cmd_tx
                .send(HarnessCommand::ToolResult {
                    call_id: "toolu_alive".into(),
                    result_json: r#"{"saved":true}"#.into(),
                })
                .await
                .unwrap();
            let (_run, prompt_id) =
                tokio::time::timeout(Duration::from_secs(5), expect_run_started_id(&mut evt_rx))
                    .await
                    .expect("result for a live generation should trigger a continuation resume");
            assert_eq!(prompt_id, None, "delivery is a continuation, not a prompt");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if tokio::fs::read_to_string(&counter)
                        .await
                        .is_ok_and(|contents| contents.trim() == "2")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the live process should be replaced for an id-stable refire");
            assert!(matches!(
                hook_fire_named(
                    &hook,
                    "toolu_alive",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Allow
            ));
            assert_eq!(
                fire_main_mcp_call(&mcp, "toolu_alive").await["result_json"],
                r#"{"saved":true}"#
            );

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 1 })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine exits")
                .expect("engine task does not panic");
            let _ = tokio::fs::remove_file(script).await;
            let _ = tokio::fs::remove_file(counter).await;
        }

        #[tokio::test]
        async fn deferred_result_abandoned_refire_fallback_emits_completion() {
            let nonce = uuid::Uuid::new_v4();
            let counter = std::env::temp_dir().join(format!("fake-claude-count-{nonce}"));
            let captured = std::env::temp_dir().join(format!("fake-claude-fallback-{nonce}"));
            let script = write_deferred_fallback_fake_claude(&counter, &captured).await;
            let (cli, hook, _mcp) = deferred_engine_cli(script.clone(), "fallback").await;
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            let (evt_tx, mut evt_rx) = mpsc::channel(64);
            let engine = tokio::spawn(run_engine(cli, cmd_rx, Arc::new(Notify::new()), evt_tx));

            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));
            cmd_tx
                .send(HarnessCommand::Prompt {
                    prompt_id: "p-fallback".into(),
                    text: "remember this".into(),
                })
                .await
                .unwrap();
            let _ = expect_run_started(&mut evt_rx).await;
            assert!(matches!(
                hook_fire_named(
                    &hook,
                    "toolu_fallback",
                    "mcp__engrams__save_memory",
                    serde_json::json!({"text":"remember"}),
                )
                .await,
                hook_server::HookVerdict::Defer
            ));
            assert!(matches!(
                evt_rx.recv().await,
                Some(HarnessEvent::ToolCallRequested { call_id, .. })
                    if call_id == "toolu_fallback"
            ));

            let mut idle_count = 0;
            tokio::time::timeout(Duration::from_secs(8), async {
                while idle_count < 2 {
                    if matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)) {
                        idle_count += 1;
                    }
                }
            })
            .await
            .expect("the dead deferred process should respawn before result delivery");

            cmd_tx
                .send(HarnessCommand::ToolResult {
                    call_id: "toolu_fallback".into(),
                    result_json: r#"{"saved":true}"#.into(),
                })
                .await
                .unwrap();

            let (_resume_run_id, prompt_id) =
                tokio::time::timeout(Duration::from_secs(10), expect_run_started_id(&mut evt_rx))
                    .await
                    .expect("resume run for the delivered result never started");
            assert_eq!(prompt_id, None, "delivery resume carries no prompt_id");
            tokio::time::timeout(
                Duration::from_secs(10),
                expect_agent_message(&mut evt_rx, "resumed without refire"),
            )
            .await
            .expect("resumed-without-refire message never arrived");
            let _ =
                tokio::time::timeout(Duration::from_secs(10), expect_run_completed(&mut evt_rx))
                    .await
                    .expect("abandoned resume run never completed");

            let (fallback_run_id, prompt_id) =
                tokio::time::timeout(Duration::from_secs(10), expect_run_started_id(&mut evt_rx))
                    .await
                    .expect("fallback delivery run never started");
            assert_eq!(prompt_id, None, "fallback delivery is not a user prompt");
            match evt_rx.recv().await {
                Some(HarnessEvent::ToolCallCompleted {
                    run_id,
                    tool_call_id,
                    tool_name,
                    ok,
                    duration_ms,
                    result_summary,
                }) => {
                    assert_eq!(run_id, fallback_run_id);
                    assert_eq!(tool_call_id, "toolu_fallback");
                    assert_eq!(tool_name, "save_memory");
                    assert!(ok);
                    assert_eq!(duration_ms, 0);
                    assert_eq!(result_summary, None);
                }
                other => panic!("expected fallback ToolCallCompleted ack, got {other:?}"),
            }
            expect_agent_message(&mut evt_rx, "got deferred result").await;
            let delivered: Value =
                serde_json::from_slice(&tokio::fs::read(&captured).await.unwrap()).unwrap();
            assert!(delivered["message"]["content"]
                .as_str()
                .is_some_and(|text| text.contains("toolu_fallback") && text.contains("saved")));
            let _ = expect_run_completed(&mut evt_rx).await;
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 1 })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine exits")
                .expect("engine task does not panic");
            let _ = tokio::fs::remove_file(script).await;
            let _ = tokio::fs::remove_file(counter).await;
            let _ = tokio::fs::remove_file(captured).await;
        }

        /// A fake claude for the answer-resume + Part B fallback cycle. The
        /// first turn it emits (init + "resumed" + `result`) models the
        /// NARRATE-PAST: on the `--resume` it does NOT re-fire the deferred
        /// tool — it just recaps and ends — so the stashed answer is left
        /// UNCONSUMED in `answers_in_hand`. Then (unlike the old fake) it
        /// RESPONDS to each stdin line with "got your answer" + `result`, so
        /// the harness's fallback delivery (a fresh user message) produces a
        /// completing turn. On the first (idle) spawn there's no in-flight
        /// turn so the recap is dropped and only the startup `Idle` surfaces;
        /// the continuation turn after `AnswerQuestion` captures the recap.
        async fn write_answer_fallback_fake_claude() -> String {
            use std::os::unix::fs::PermissionsExt;
            let path =
                std::env::temp_dir().join(format!("fake-claude-{}.sh", uuid::Uuid::new_v4()));
            let body = "#!/bin/sh\n\
                printf '%s\\n' '{\"type\":\"system\",\"subtype\":\"init\"}'\n\
                printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"id\":\"m2\",\"content\":[{\"type\":\"text\",\"text\":\"resumed\"}]}}'\n\
                printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
                while IFS= read -r _l; do\n\
                  printf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"id\":\"m3\",\"content\":[{\"type\":\"text\",\"text\":\"got your answer\"}]}}'\n\
                  printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n\
                done\n";
            tokio::fs::write(&path, body).await.unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            path.to_string_lossy().into_owned()
        }

        // ADR 0054 Part B: an `AnswerQuestion` stashes the answer, SIGINTs
        // claude, and respawns `--resume` into a CONTINUATION turn (RunStarted
        // with NO prompt_id). When claude NARRATE-PASTs, it does NOT re-fire
        // the deferred tool on resume (verified: an abandoned tool is not
        // re-presented), so the answer is left UNCONSUMED in `answers_in_hand`
        // after the continuation turn. The engine must then DELIVER the answer
        // as a fresh user message (a new no-prompt_id turn) and mark the card
        // answered (`QuestionAnswered`), instead of going idle with the
        // question hanging. (The fake never consumes the answer, exactly like
        // the abandoned-tool case; the first idle spawn's recap is dropped.)
        #[tokio::test]
        async fn answer_resume_unconsumed_falls_back_to_user_message() {
            let script = write_answer_fallback_fake_claude().await;
            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(8);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(
                test_cli(script.clone()),
                cmd_rx,
                reattach.clone(),
                evt_tx,
            ));

            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            let mut a = Answers::new();
            a.insert("Q?".into(), vec!["A".into()]);
            cmd_tx
                .send(HarnessCommand::AnswerQuestion {
                    tool_call_id: "toolu_1".into(),
                    answers: a,
                })
                .await
                .unwrap();

            // 1) the answer-resume continuation turn — claude recaps ("resumed")
            //    and ends WITHOUT re-firing the deferred tool: answer unconsumed.
            let (_rid, pid) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(pid, None, "continuation turn carries no prompt_id");
            expect_agent_message(&mut evt_rx, "resumed").await;
            let _ = expect_run_completed(&mut evt_rx).await;

            // 2) Part B fallback: a fresh turn (no prompt_id) delivers the answer
            //    as a user message, and the card is marked answered.
            let (_fid, fpid) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(
                fpid, None,
                "the fallback answer delivery is not a user prompt"
            );
            match evt_rx.recv().await {
                Some(HarnessEvent::QuestionAnswered {
                    tool_call_id,
                    answers,
                    ..
                }) => {
                    assert_eq!(tool_call_id, "toolu_1");
                    assert_eq!(answers.get("Q?"), Some(&vec!["A".to_string()]));
                }
                other => panic!("expected QuestionAnswered, got {other:?}"),
            }
            expect_agent_message(&mut evt_rx, "got your answer").await;
            let _ = expect_run_completed(&mut evt_rx).await;
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

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

        /// ADR 0054 regression — the exact failure mode of session
        /// `3b9b9dc5`: a narrate-past on the ANSWER-RESUME turn drops into the
        /// Part B fallback, which delivers the answer as a fresh user message
        /// but (before the fix) never cleared `question_outstanding` — the
        /// hook's clear at the answer verdict (`HookVerdict::Answer`) is
        /// unreachable on this path because the deferred tool was never
        /// re-fired. The leaked flag then makes the NEXT genuine
        /// AskUserQuestion look like a #64389 duplicate: deferred with NO
        /// card, scrubbed, silently swallowed — re-introducing the very bug
        /// this ADR fixed. In the live session that surfaced as a first
        /// "red or green" card answered via the fallback (no
        /// `tool_call_completed`), then a "cats or dogs" prompt that produced
        /// `run_started`/`run_completed` with no `user_question` at all.
        ///
        /// This drives the real seam end to end — the engine's fallback and
        /// the real hook server share ONE `question_outstanding` over an
        /// isolated socket — so the assertion exercises the actual coupling,
        /// not a stand-in: Q1 is carded, answered, narrate-past'd into the
        /// fallback; then a distinct Q2 must STILL be carded.
        #[tokio::test]
        async fn fallback_clears_outstanding_so_later_question_still_cards() {
            let script = write_answer_fallback_fake_claude().await;
            // An isolated hook socket: this test FIRES real hooks, so it can't
            // share the fixed production path the other `run_engine` tests bind
            // (they never fire, so they tolerate the collision; we can't).
            let sock = std::env::temp_dir()
                .join(format!("engram-regr-{}.sock", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .into_owned();
            let mut cli = test_cli(script.clone());
            cli.hook_sock_path = Some(sock.clone());

            let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessCommand>(8);
            let (evt_tx, mut evt_rx) = mpsc::channel::<HarnessEvent>(64);
            let reattach = Arc::new(Notify::new());
            let engine = tokio::spawn(run_engine(cli, cmd_rx, reattach.clone(), evt_tx));

            // Startup idle. The socket is bound before the first run, so once
            // Idle lands a hook fire can connect.
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // Q1: the hook cards it and sets `question_outstanding` true.
            let q1 = vec![sample_q("Q?", false)];
            assert!(matches!(
                hook_fire(&sock, "toolu_1", &q1).await,
                hook_server::HookVerdict::Defer
            ));
            match evt_rx.recv().await {
                Some(HarnessEvent::UserQuestion { tool_call_id, .. }) => {
                    assert_eq!(tool_call_id, "toolu_1", "Q1 is carded");
                }
                other => panic!("expected UserQuestion for Q1, got {other:?}"),
            }

            // Answer Q1. Claude narrate-past's on the resume (never re-fires
            // the deferred tool → the hook never consumes the answer, so its
            // flag-clear never runs), and the engine drops into the fallback.
            let mut a = Answers::new();
            a.insert("Q?".into(), vec!["A".into()]);
            cmd_tx
                .send(HarnessCommand::AnswerQuestion {
                    tool_call_id: "toolu_1".into(),
                    answers: a,
                })
                .await
                .unwrap();

            // The narrate-past resume turn: recap, no tool re-fire.
            let (_rid, pid) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(pid, None, "continuation turn carries no prompt_id");
            expect_agent_message(&mut evt_rx, "resumed").await;
            let _ = expect_run_completed(&mut evt_rx).await;

            // The Part B fallback delivers the answer as a fresh user message.
            let (_fid, fpid) = expect_run_started_id(&mut evt_rx).await;
            assert_eq!(fpid, None, "the fallback answer delivery is not a prompt");
            match evt_rx.recv().await {
                Some(HarnessEvent::QuestionAnswered { tool_call_id, .. }) => {
                    assert_eq!(tool_call_id, "toolu_1");
                }
                other => panic!("expected QuestionAnswered, got {other:?}"),
            }
            expect_agent_message(&mut evt_rx, "got your answer").await;
            let _ = expect_run_completed(&mut evt_rx).await;
            assert!(matches!(evt_rx.recv().await, Some(HarnessEvent::Idle)));

            // The regression: a brand-new, DISTINCT question. With the flag
            // leaked it is deduped (Defer, no card) and swallowed; with the
            // fallback's clear it cards again. Both outcomes return Defer, so
            // only the EMITTED card distinguishes them — and on the buggy path
            // no event is ever emitted, so we bound the wait to fail loudly
            // instead of hanging.
            let q2 = vec![sample_q("Q2?", false)];
            assert!(matches!(
                hook_fire(&sock, "toolu_2", &q2).await,
                hook_server::HookVerdict::Defer
            ));
            match tokio::time::timeout(Duration::from_secs(5), evt_rx.recv()).await {
                Ok(Some(HarnessEvent::UserQuestion { tool_call_id, .. })) => {
                    assert_eq!(
                        tool_call_id, "toolu_2",
                        "the later genuine question must still be carded"
                    );
                }
                Ok(other) => panic!("expected UserQuestion for Q2, got {other:?}"),
                Err(_) => panic!(
                    "Q2 was silently swallowed: no card emitted — \
                     `question_outstanding` leaked across the Part B fallback"
                ),
            }

            cmd_tx
                .send(HarnessCommand::Shutdown { grace_secs: 5 })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), engine)
                .await
                .expect("engine should exit on shutdown")
                .expect("engine task should not panic");
            let _ = tokio::fs::remove_file(&script).await;
            let _ = tokio::fs::remove_file(&sock).await;
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    // ADR 0054: git/busybox argv dispatch — the SAME binary is the socket
    // SERVER (normal mode) and the transient PreToolUse hook CLIENT. Peek
    // argv[1] BEFORE clap sees it: `Cli` requires `--session-id`, which a
    // hook invocation never has, so a hook must bypass the parser entirely.
    if std::env::args().nth(1).as_deref() == Some("hook-bridge") {
        return adapter::hook_bridge::run().await;
    }
    if std::env::args().nth(1).as_deref() == Some("mcp-bridge") {
        return adapter::mcp_bridge::run().await;
    }
    adapter::entry().await
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::adapter::*;
    use engram_harness_proto::{FileChange, HarnessEvent};
    use std::collections::HashMap;

    #[test]
    fn system_init_emits_nothing_and_assistant_text_carries_run_id() {
        // `system`/`init` no longer synthesizes RunStarted (the session
        // loop owns run lifecycle) — it yields no events. Assistant text
        // is tagged with the caller-provided run_id.
        let init = r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#;
        let asst = r#"{"type":"assistant","message":{"id":"msg_1","content":[{"type":"text","text":"hi there"}]}}"#;
        let mut tc = 0u32;
        let mut fc = HashMap::new();
        let evs = translate_jsonl(
            init,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(evs.is_empty(), "init emits no HarnessEvent");

        let evs = translate_jsonl(
            asst,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
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
    fn ai_title_line_becomes_title_suggested() {
        // Claude Code's `ai-title` line surfaces as a `TitleSuggested` event.
        // It carries no run_id, so the empty run_id here (the out-of-turn call
        // shape) is fine.
        let line = r#"{"type":"ai-title","aiTitle":"Fix the flaky test","sessionId":"abc-123"}"#;
        let mut tc = 0u32;
        let mut fc = HashMap::new();
        let evs = translate_jsonl(
            line,
            "",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            HarnessEvent::TitleSuggested { title } => assert_eq!(title, "Fix the flaky test"),
            other => panic!("expected TitleSuggested, got {other:?}"),
        }

        // A blank title is dropped (no phantom empty-title event).
        let blank = r#"{"type":"ai-title","aiTitle":"   ","sessionId":"abc-123"}"#;
        let evs = translate_jsonl(
            blank,
            "",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(evs.is_empty(), "blank ai-title emits nothing");
    }

    #[test]
    fn stream_event_text_deltas_become_chunks_keyed_on_message_id() {
        // Phase 1c: partial-message streaming. `message_start` captures the
        // assistant message id; each `content_block_delta`/`text_delta`
        // becomes an `AgentMessageChunk` carrying that id — the SAME id the
        // terminal `assistant` message uses, so the UI reconciles in place.
        let msg_start =
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_42"}}}"#;
        let block_start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        let d1 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}}"#;
        let d2 = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}}"#;
        let complete = r#"{"type":"assistant","message":{"id":"msg_42","content":[{"type":"text","text":"hello"}]}}"#;

        let mut tc = 0u32;
        let mut mid: Option<String> = None;
        let mut fc = HashMap::new();

        // message_start: no event, but the id is now tracked.
        assert!(translate_jsonl(
            msg_start,
            "run-9",
            &mut tc,
            50,
            &mut mid,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new()
        )
        .unwrap()
        .is_empty());
        assert_eq!(mid.as_deref(), Some("msg_42"));

        // content_block_start: no chunk (empty text), no event.
        assert!(translate_jsonl(
            block_start,
            "run-9",
            &mut tc,
            50,
            &mut mid,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new()
        )
        .unwrap()
        .is_empty());

        for (line, want) in [(d1, "hel"), (d2, "lo")] {
            let evs = translate_jsonl(
                line,
                "run-9",
                &mut tc,
                50,
                &mut mid,
                &mut fc,
                &mut HashMap::new(),
                &mut std::collections::HashSet::new(),
                &mut Vec::new(),
            )
            .unwrap();
            assert_eq!(evs.len(), 1);
            match &evs[0] {
                HarnessEvent::AgentMessageChunk {
                    run_id,
                    message_id,
                    chunk,
                } => {
                    assert_eq!(run_id, "run-9");
                    assert_eq!(message_id, "msg_42", "chunk shares the terminal message id");
                    assert_eq!(chunk, want);
                }
                other => panic!("expected AgentMessageChunk, got {other:?}"),
            }
        }

        // The terminal `assistant` message uses the SAME id — the durable
        // record that supersedes the chunks downstream.
        let evs = translate_jsonl(
            complete,
            "run-9",
            &mut tc,
            50,
            &mut mid,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        match &evs[0] {
            HarnessEvent::AgentMessage {
                message_id, text, ..
            } => {
                assert_eq!(message_id, "msg_42");
                assert_eq!(text, "hello");
            }
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn translates_tool_use_and_tool_result_pair() {
        let asst = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls /workspace"}}]}}"#;
        let user = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"file1.txt\nfile2.py","is_error":false}]}}"#;

        let mut tc = 0u32;
        let mut fc = HashMap::new();
        let evs = translate_jsonl(
            asst,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], HarnessEvent::ToolCallStarted { .. }));
        assert_eq!(tc, 1);

        let evs = translate_jsonl(
            user,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
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
        // A non-file tool stashes no pending change.
        assert!(fc.is_empty());
    }

    // ADR 0054 Flavor A: an Edit whose tool_result succeeds emits a
    // FileChanged (alongside ToolCallCompleted), carrying the normalized hunk.
    #[test]
    fn successful_edit_emits_file_changed_with_hunks() {
        let asst = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_e","name":"Edit","input":{"file_path":"src/main.rs","old_string":"let x = 1;","new_string":"let x = 2;"}}]}}"#;
        let user = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_e","content":"ok","is_error":false}]}}"#;

        let mut tc = 0u32;
        let mut fc = HashMap::new();
        // tool_use stashes the pending change; only ToolCallStarted is emitted.
        let evs = translate_jsonl(
            asst,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert!(matches!(
            evs.as_slice(),
            [HarnessEvent::ToolCallStarted { .. }]
        ));
        assert_eq!(fc.len(), 1);

        // The successful tool_result emits ToolCallCompleted THEN FileChanged,
        // and retires the pending entry.
        let evs = translate_jsonl(
            user,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], HarnessEvent::ToolCallCompleted { .. }));
        match &evs[1] {
            HarnessEvent::FileChanged {
                tool_call_id,
                path,
                change,
                ..
            } => {
                assert_eq!(tool_call_id, "toolu_e");
                assert_eq!(path, "src/main.rs");
                match change {
                    FileChange::Edit { hunks } => {
                        assert_eq!(hunks.len(), 1);
                        assert_eq!(hunks[0].old, "let x = 1;");
                        assert_eq!(hunks[0].new, "let x = 2;");
                    }
                    other => panic!("expected Edit, got {other:?}"),
                }
            }
            other => panic!("expected FileChanged, got {other:?}"),
        }
        assert!(fc.is_empty(), "pending change retired on result");
    }

    // ADR 0054 Flavor A: a FAILED edit emits no FileChanged (no phantom diff)
    // but still drops the stashed change.
    #[test]
    fn failed_edit_emits_no_file_changed() {
        let asst = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"toolu_w","name":"Write","input":{"file_path":"a.txt","content":"hello"}}]}}"#;
        let user = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_w","content":"String to replace not found","is_error":true}]}}"#;

        let mut tc = 0u32;
        let mut fc = HashMap::new();
        translate_jsonl(
            asst,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(fc.len(), 1);

        let evs = translate_jsonl(
            user,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut fc,
            &mut HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut Vec::new(),
        )
        .unwrap();
        // Only the (failed) ToolCallCompleted — no FileChanged.
        assert!(matches!(
            evs.as_slice(),
            [HarnessEvent::ToolCallCompleted { ok: false, .. }]
        ));
        assert!(fc.is_empty(), "stashed change dropped on failure");
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
    fn detect_result_marker_captures_terminal_reason() {
        // ADR 0054: a genuinely deferred AUQ ends the turn subtype=success
        // but terminal_reason="tool_deferred"; a narrate-past ends
        // terminal_reason="completed". `subtype`/`is_error` are identical in
        // both, so `terminal_reason` is the only discriminator (matches the
        // Agent SDK's SDKResultSuccess contract).
        let deferred = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"tool_deferred"}"#;
        assert_eq!(
            detect_result_marker(deferred)
                .unwrap()
                .terminal_reason
                .as_deref(),
            Some("tool_deferred")
        );
        let completed = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed"}"#;
        assert_eq!(
            detect_result_marker(completed)
                .unwrap()
                .terminal_reason
                .as_deref(),
            Some("completed")
        );
        // Absent (older CLI / non-AUQ paths) → None.
        let bare = r#"{"type":"result","subtype":"success","is_error":false}"#;
        assert_eq!(detect_result_marker(bare).unwrap().terminal_reason, None);
    }

    #[test]
    fn narrate_past_assistant_text_after_deferred_auq_is_suppressed() {
        // ADR 0054 (observed on claude-sonnet-4-6, ~1/8): after an
        // AskUserQuestion is deferred (the hook returns `defer`, no
        // tool_result follows), claude sometimes narrates a bogus "internal
        // error retrieving your answer" in a LATER assistant message and ends
        // the turn `completed` instead of `tool_deferred`. That hallucination
        // must NOT reach the UI — the UserQuestion card (emitted by the hook)
        // is the source of truth and the session stays awaiting-answer.
        let mut tc = 0u32;
        let mut auq = std::collections::HashSet::new();

        // 1) claude proposes the AUQ; the hook defers it (no tool_result).
        let auq_use = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_AUQ","name":"AskUserQuestion","input":{"questions":[]}}]}}"#;
        let evs = translate_jsonl(
            auq_use,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, HarnessEvent::ToolCallStarted { .. })),
            "the AUQ tool call still surfaces (the UI dedups it against UserQuestion)"
        );

        // 2) the narrate-past: a NEW assistant message with bogus error text.
        let bogus = r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"It seems there was an internal error retrieving your answer."}]}}"#;
        let evs = translate_jsonl(
            bogus,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !evs.iter()
                .any(|e| matches!(e, HarnessEvent::AgentMessage { .. })),
            "the hallucinated post-defer message must be suppressed, got {evs:?}"
        );
    }

    #[test]
    fn narrate_past_after_deferred_mcp_tool_is_suppressed() {
        let manifest = vec![ManifestTool {
            name: "save_memory".into(),
            description: "Save a memory".into(),
            input_schema: serde_json::json!({"type":"object"}),
            execution: ToolExecution::Deferred,
            native_bindings: NativeBindings::default(),
        }];
        let deferred_names = deferred_tool_names(&manifest);
        let mut tool_calls = 0;
        let mut pending = std::collections::HashSet::new();

        let tool_use = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_MCP","name":"mcp__engrams__save_memory","input":{"text":"remember"}}]}}"#;
        let events = translate_jsonl_with_deferred_tools(
            tool_use,
            "run-1",
            &mut tool_calls,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &deferred_names,
            &mut pending,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(events
            .iter()
            .any(|event| matches!(event, HarnessEvent::ToolCallStarted { .. })));
        assert!(pending.contains("toolu_MCP"));

        let bogus = r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"The tool result was missing due to an internal error."}]}}"#;
        let events = translate_jsonl_with_deferred_tools(
            bogus,
            "run-1",
            &mut tool_calls,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &deferred_names,
            &mut pending,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, HarnessEvent::AgentMessage { .. })),
            "post-defer MCP narrate-past text must be suppressed: {events:?}"
        );
    }

    #[test]
    fn suppressed_narrate_past_id_is_recorded_for_scrub() {
        // ADR 0054 Part C: when a post-defer assistant message is suppressed,
        // its message-id is recorded so `scrub_transcript` can remove that
        // exact line from claude's transcript before the answer-resume.
        let mut tc = 0u32;
        let mut auq = std::collections::HashSet::new();
        let mut suppressed: Vec<String> = Vec::new();

        let auq_use = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_AUQ","name":"AskUserQuestion","input":{}}]}}"#;
        translate_jsonl(
            auq_use,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut suppressed,
        )
        .unwrap();
        assert!(
            suppressed.is_empty(),
            "the AUQ message itself is not a narrate-past, so nothing recorded yet"
        );

        let bogus = r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"It seems there was an internal error."}]}}"#;
        translate_jsonl(
            bogus,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut suppressed,
        )
        .unwrap();
        assert_eq!(
            suppressed,
            vec!["m2".to_string()],
            "the suppressed narrate-past message-id is recorded for the scrub"
        );
    }

    #[test]
    fn narrate_past_chunks_after_deferred_auq_are_suppressed() {
        // The hallucinated message also streams as live token chunks
        // (`--include-partial-messages`); those must be suppressed too, or the
        // user watches the bogus error type out live.
        let mut tc = 0u32;
        let mut mid: Option<String> = None;
        let mut auq = std::collections::HashSet::new();

        let auq_use = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_AUQ","name":"AskUserQuestion","input":{}}]}}"#;
        translate_jsonl(
            auq_use,
            "run-1",
            &mut tc,
            50,
            &mut mid,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();

        let chunk = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"internal error"}}}"#;
        let evs = translate_jsonl(
            chunk,
            "run-1",
            &mut tc,
            50,
            &mut mid,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            !evs.iter()
                .any(|e| matches!(e, HarnessEvent::AgentMessageChunk { .. })),
            "post-defer chunks must be suppressed, got {evs:?}"
        );
    }

    #[test]
    fn answered_auq_does_not_suppress_following_text() {
        // Regression guard: the LEGIT answer path is AUQ tool_use →
        // tool_result (the answer) → assistant text ("You selected Red").
        // The tool_result clears the pending AUQ, so the text MUST be emitted.
        let mut tc = 0u32;
        let mut auq = std::collections::HashSet::new();

        let auq_use = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_AUQ","name":"AskUserQuestion","input":{}}]}}"#;
        translate_jsonl(
            auq_use,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_AUQ","content":"Do you prefer red or blue?=Red","is_error":false}]}}"#;
        translate_jsonl(
            result,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();

        let after = r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"You selected Red."}]}}"#;
        let evs = translate_jsonl(
            after,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            evs.iter().any(|e| matches!(e, HarnessEvent::AgentMessage { text, .. } if text == "You selected Red.")),
            "text after an ANSWERED AUQ must be emitted, got {evs:?}"
        );
    }

    #[test]
    fn preamble_in_same_message_as_deferred_auq_is_kept() {
        // Regression guard: a preamble in the SAME assistant message as the
        // AUQ tool_use is a legitimate "I have a question:" — only LATER
        // messages (after the defer) are the hallucination. The suppress flag
        // is captured at line start, so same-message text survives.
        let mut tc = 0u32;
        let mut auq = std::collections::HashSet::new();
        let line = r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Let me ask:"},{"type":"tool_use","id":"toolu_AUQ","name":"AskUserQuestion","input":{}}]}}"#;
        let evs = translate_jsonl(
            line,
            "run-1",
            &mut tc,
            50,
            &mut None,
            &mut std::collections::HashMap::new(),
            &mut HashMap::new(),
            &mut auq,
            &mut Vec::new(),
        )
        .unwrap();
        assert!(
            evs.iter().any(
                |e| matches!(e, HarnessEvent::AgentMessage { text, .. } if text == "Let me ask:")
            ),
            "same-message preamble must be kept, got {evs:?}"
        );
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
