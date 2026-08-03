//! Wire types for the harness ↔ host vsock channel.
//!
//! The "harness" is whatever runs the agent process inside the
//! sandbox: an Engram-aware first-party harness (`engram-harness-noop`
//! for tests; agent-specific adapters in production), or a thin
//! adapter wrapping a vendor agent like Claude Code (`engram-harness-claude`).
//!
//! This is deliberately separate from the existing exec channel
//! (`engram-agentd::proto`). The exec channel is host-driven: the host
//! sends one [`engram_agentd::proto::WireRequest`] per connection,
//! gets back a stream or one-shot reply. The harness channel is
//! _harness_-driven: the agent inside the sandbox dials the host and
//! pushes [`HarnessEvent`]s as it makes tool calls; the host can send
//! [`HarnessCommand`]s back over the same connection (Checkpoint /
//! Shutdown) which the harness handles at safe boundaries.
//!
//! Conversation shape:
//!
//! ```text
//!   harness ──[ HarnessAttach { session_id, harness_version } ]──► host
//!   host    ──[ HarnessAttachAck { ok, message } ]──► harness
//!     (host validates session_id is known; rejects unknown sessions)
//!
//!   harness ──[ HarnessFrame::Event(HarnessEvent::*) ]──► host  (0+ times)
//!   host    ──[ HarnessFrame::Command(HarnessCommand::*) ]──► harness (0+ times)
//!     (full duplex from here on)
//!
//!   <connection closed>
//! ```
//!
//! Framing: same scheme as `engram-agentd::proto` — 4-byte big-endian
//! length prefix + bincode body. Reusing the shape keeps the agent
//! image small (one codec).

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use engram_core::{SandboxId, SessionId};

/// Vsock port the in-guest harness dials to reach the host. Distinct
/// from the agentd exec port (1024) so the host can demux at `accept`
/// time. Direction: guest-to-host (host listens on a UDS at
/// `<vsock_uds>_<port>.sock`; guest dials AF_VSOCK CID=2 port=1026).
pub const HARNESS_VSOCK_PORT: u32 = 1026;

/// Single-frame size cap. Same as `engram-agentd::proto::MAX_MSG_BYTES`.
/// `transcript_delta` payloads are typically a few KB (one JSONL line
/// per tool call); 16 MiB gives plenty of headroom for outliers.
pub const MAX_MSG_BYTES: usize = 16 * 1024 * 1024;

/// Reserved env key carrying the harness's working directory from
/// coord → agentd. The cwd rides the *existing* `SpawnHarnessRequest.env`
/// field rather than a new wire field on purpose: the host↔agentd frame
/// is positional bincode (`engram-agentd::proto`) and agentd is baked
/// into the image, so a freshly-deployed host can talk to an *older*
/// agentd inside an already-baked base snapshot. Adding a struct field
/// would risk breaking SpawnHarness for every pre-existing image; an env
/// entry an old agentd simply ignores (harness stays in `/`) and a new
/// agentd honors (`current_dir`). Set by `resolve_harness` from the
/// image manifest's `workdir`; consumed (and stripped from the child
/// env) by `engram-agentd`'s harness supervisor.
pub const HARNESS_CWD_ENV: &str = "ENGRAM_HARNESS_CWD";

/// First frame the harness sends after dialing the host. Carries the
/// full attach token (ADR 0073): the hub validates it against the
/// host-durable binding record — never an in-memory map — and rejects
/// with a typed [`AttachReject`] on mismatch.
///
/// Wire note: this is a clean bincode break from the pre-0067 two-field
/// frame (zero users; a stale harness bundle cannot attach, which is
/// the correct failure — the bundle re-publish rides the same PR).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessAttach {
    pub session_id: SessionId,
    /// The sandbox this harness believes it lives in. From
    /// `ENGRAM_SANDBOX_ID`, stamped into the harness env at spawn by
    /// the host-agent (the only party that knows it pre-boot).
    pub sandbox_id: SandboxId,
    /// Binding generation. From `ENGRAM_BINDING_EPOCH`, minted by the
    /// coordinator when it committed to this (re)bind. A presented
    /// epoch older than the on-disk record is `Superseded` — fatal.
    pub binding_epoch: u64,
    /// Free-form identifier for the harness build (e.g.
    /// `"engram-harness-claude/0.1.0"`). Logged at debug; no semantics.
    pub harness_version: String,
}

/// Typed attach rejection (ADR 0073). The harness's reconnect policy
/// is derived from the variant, never from string matching.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttachReject {
    /// No binding record for this session on this host (create/restore
    /// bind still in flight, or the sandbox was torn down). TRANSIENT:
    /// back off and retry.
    UnknownBinding,
    /// The presented epoch is older than the host's durable record —
    /// a newer generation owns this session. FATAL: exit; retrying
    /// can never succeed (ADR 0073 makes the fbd3794c competing-bind
    /// loop unrepresentable via exactly this arm).
    Superseded,
    /// Token internally inconsistent (session/sandbox pair does not
    /// match the record). TRANSIENT: treated like `UnknownBinding`.
    SessionMismatch,
}

/// Host's reply to [`HarnessAttach`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessAttachAck {
    pub ok: bool,
    /// Typed rejection when `ok = false` (ADR 0073). Drives the
    /// harness's reconnect-vs-exit decision deterministically.
    pub reject: Option<AttachReject>,
    /// Short human-readable reason on `ok = false`. Plaintext on the
    /// vsock — never include sensitive context.
    pub message: Option<String>,
}

/// One frame on the steady-state full-duplex channel after the
/// handshake. The harness sends `Event`s, the host sends `Command`s.
/// Wrapping in a single enum keeps a single bincode-deserializer at
/// each end.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HarnessFrame {
    Event(HarnessEvent),
    Command(HarnessCommand),
}

// ---- Rich file changes (ADR 0054 Flavor A) -----------------------------
//
// A harness-agnostic mirror of an agent's file-mutating tool (Claude's
// `Write` / `Edit` / `MultiEdit`). The harness recognizes such a tool and,
// on its **successful** `tool_result`, emits a [`HarnessEvent::FileChanged`]
// the UI renders as a rich diff (red/green hunks for an edit, an all-green
// block for a write) — in place of the generic tool card.

/// Per-string truncation budget for a [`FileChange`]. Each inner string
/// (`Write.content`, every `EditHunk.old`/`new`) is clipped to this *before*
/// serialization — never the JSON blob — so a single huge write can't blow
/// the frame. Distinct from (and far larger than) the 1 KB `args_summary`
/// cap used for every other tool's generic event.
pub const MAX_FILE_CHANGE_BYTES: usize = 64 * 1024;

/// What changed about a file. **Externally tagged** (the default serde enum
/// representation), NOT `#[serde(tag = "op")]`: this type rides the *bincode*
/// harness wire as a field of [`HarnessEvent::FileChanged`], and bincode is
/// positional with no `deserialize_any`, so an internally-tagged (`tag`) or
/// untagged enum **panics at decode** — the same constraint that forces
/// positional wire cannot deserialize those representations. `rename_all`
/// keeps the JSON the coordinator re-emits over SSE clean:
/// `{ "write": { "content": … } }` /
/// `{ "edit": { "hunks": [ … ] } }`, which the web discriminates by key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// A whole-file write (Claude's `Write`). The UI renders `content` as an
    /// all-green (all-added) block.
    Write { content: String },
    /// One or more search/replace edits (Claude's `Edit` = one hunk;
    /// `MultiEdit` = many). The UI renders red/green hunks.
    Edit { hunks: Vec<EditHunk> },
    /// A complete unified diff, as emitted by agents whose native file-change
    /// protocol is patch-oriented (Codex app-server). Keeping the patch intact
    /// preserves line numbers, context, renames, and multi-hunk edits that
    /// cannot be represented truthfully as search/replace pairs.
    Patch { unified_diff: String },
}

/// One search/replace edit. Maps from Claude's `{ old_string, new_string }`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditHunk {
    /// The text being replaced (Claude's `old_string`). Empty for a pure
    /// insertion.
    pub old: String,
    /// The replacement text (Claude's `new_string`).
    pub new: String,
}

/// Events the harness emits as the agent inside it does work. The
/// host forwards each into `session_events`; consumers (Slack bot,
/// web UI, audit log) read `GET /sessions/:id/events` SSE and render
/// directly from the structured fields below — no per-agent decoder.
///
/// **Shape choices for chat consumers:**
/// - Tool call events carry small structured `args_summary` /
///   `result_summary` strings (≤ a few KB), not the agent's native
///   bytes. Slack / web UIs render them straight.
/// - `AgentMessage` is the assistant's **complete** text response
///   between tool calls (or a system message) — the durable record,
///   persisted to `session_events`. `AgentMessageChunk` carries the
///   incremental token deltas of that same message for live-typing UIs
///   (ADR 0052 Phase 1c); it is EPHEMERAL — streamed live, never
///   persisted — and is always superseded by the `AgentMessage` (same
///   `message_id`) that follows.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HarnessEvent {
    /// New agent run started (typically: user prompt arrived). The
    /// harness assigns `run_id`; subsequent events in this run carry
    /// the same `run_id`. `prompt_summary` is the first ~1 KB of
    /// the prompt for Slack/UI rendering.
    ///
    /// `prompt_id` is the client/coord-minted id of the prompt that started
    /// this run (the id carried on `HarnessCommand::Prompt`). It is the
    /// **"queued prompt consumed" signal**: a UI that rendered a greyed
    /// type-ahead composer item with this id moves it into the
    /// conversation when this event arrives. Issue #535 (d): every prompt
    /// — including the session's create-time initial one — arrives via
    /// `HarnessCommand::Prompt` now, so this is `Some` in practice; kept
    /// `Option` rather than tightening the wire type (baked-harness compat
    /// — see the field-order note below).
    RunStarted {
        run_id: String,
        prompt_summary: Option<String>,
        // APPENDED (trailing) for Phase 1b — keeps the struct's field
        // order stable for older baked harnesses; see wire_golden.rs.
        prompt_id: Option<String>,
    },
    /// Assistant / user / system text emitted by the agent. v1 is
    /// per-final-message: streaming agents (Claude, OpenCode)
    /// consolidate text content within one logical message before
    /// emitting. `text` is truncated to 64 KB at the wire — bigger
    /// gets clipped with a `…[truncated N bytes]` suffix the adapter
    /// owns.
    AgentMessage {
        run_id: String,
        /// Adapter-local id for de-dup / threading. Often the
        /// underlying agent's message id (Claude's `message.id`,
        /// OpenCode's event ulid).
        message_id: String,
        role: AgentRole,
        text: String,
    },
    /// Tool call beginning. `tool_call_id` ties Started/Completed.
    /// `args_summary` is a human-readable rendering (≤ 1 KB) of the
    /// tool's input — the adapter picks the format (e.g. for Bash:
    /// the command line; for Read: the file path).
    ToolCallStarted {
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        args_summary: Option<String>,
    },
    /// Tool call finished. `result_summary` is a human-readable
    /// rendering (≤ 4 KB) of the tool's output — what a Slack bot
    /// or UI would show after the tool-call header.
    ToolCallCompleted {
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        ok: bool,
        duration_ms: u64,
        result_summary: Option<String>,
    },
    /// Run finished cleanly (or failed terminally). After this, the
    /// adapter MUST emit `Idle` to mark "awaiting user input."
    RunCompleted { run_id: String, ok: bool },
    /// ADR 0030: the in-flight run was stopped by an operator
    /// interrupt (the adapter SIGINT'd its current child in response
    /// to `HarnessCommand::Interrupt`). Distinct from a clean/failed
    /// `RunCompleted` so the transcript can show an "interrupted"
    /// marker. Like `RunCompleted`, the adapter MUST follow this with
    /// `Idle` — the session stays alive and the next prompt resumes it.
    RunInterrupted { run_id: String },
    /// Explicit "I'm awaiting user input." Engram's idle-eviction
    /// soft TTL fires N seconds after this. Adapters MUST emit it
    /// after every `RunCompleted`; emitting redundantly (no run in
    /// between) just resets the soft timer, which is fine but
    /// wasteful — don't do it.
    Idle,
    // ── Phase 1b: queued/steered prompts (ADR 0052). APPENDED after
    //    `Idle` so existing bincode variant indices (RunStarted=0 …
    //    Idle=6) never shift — see tests/wire_golden.rs.
    /// A prompt arrived while a run was in flight and was QUEUED (not
    /// yet consumed) — type-ahead / steering. The harness is the
    /// single-writer owner of the queue; it holds the prompt in memory
    /// and writes it to the agent only at the consumption boundary
    /// (the running turn's end, or right after an interrupt). The web
    /// renders this as a greyed, **editable** composer item keyed on
    /// `prompt_id`; `summary` is the first ~1 KB for rendering.
    PromptQueued {
        prompt_id: String,
        summary: Option<String>,
    },
    /// A still-queued prompt's text was edited before consumption (via
    /// `HarnessCommand::EditQueued`). Carries the new `summary`.
    PromptEdited {
        prompt_id: String,
        summary: Option<String>,
    },
    /// A still-queued prompt was removed from the queue before
    /// consumption (via `HarnessCommand::DequeueQueued`) — the user
    /// pulled it back into the composer to edit, or cancelled it.
    PromptDequeued { prompt_id: String },
    // ── Phase 1c: live token streaming (ADR 0052). APPENDED after
    //    `PromptDequeued` so existing bincode variant indices never
    //    shift (… PromptDequeued=9, AgentMessageChunk=10) — see
    //    tests/wire_golden.rs.
    /// An incremental token delta of the in-flight assistant message —
    /// the live-typing payload. EPHEMERAL by contract: the adapter
    /// streams it to live subscribers but NEVER persists it, and always
    /// follows the message with a complete [`AgentMessage`] carrying the
    /// same `message_id`, which is the durable record and supersedes all
    /// of this message's chunks. `chunk` is the raw text delta (≤ a few
    /// KB; the underlying SSE chunks are already token-batched). A
    /// consumer that misses chunks (lag, a reconnect, a replica hop)
    /// loses only animation — the final `AgentMessage` makes it whole.
    AgentMessageChunk {
        run_id: String,
        /// The agent's message id (Claude's `message.id`) — the SAME id
        /// the terminal `AgentMessage` carries, so a UI keys the live
        /// bubble on it and the final message reconciles in place.
        message_id: String,
        chunk: String,
    },
    // ── ADR 0054 Flavor A: rich file-change rendering. ADR 0089 P5d removed
    //    the bespoke question variants that preceded this one, deliberately
    //    shifting FileChanged 13→11 in the sanctioned flag-day wire break.
    /// The agent successfully changed a file via a `Write`/`Edit`/`MultiEdit`
    /// tool. Emitted on the **successful** `tool_result` only (truthful —
    /// never a phantom diff for a failed edit), correlated to the originating
    /// tool call by `tool_call_id` (the UI renders this rich diff in place of
    /// that tool's generic card). `path` is the file; `change` carries the
    /// write content or the edit hunks (each inner string truncated to
    /// [`MAX_FILE_CHANGE_BYTES`]).
    FileChanged {
        run_id: String,
        tool_call_id: String,
        path: String,
        change: FileChange,
    },
    // ── Session titles: LLM-generated title suggestion. APPENDED after
    //    `FileChanged` so existing bincode variant indices never shift
    //    (… FileChanged=11, TitleSuggested=12) — see tests/wire_golden.rs.
    /// The harness proposes a short human-readable title for the session
    /// (Claude Code emits these as `ai-title` lines). Not tied to a run —
    /// it can arrive mid- or between-turns. The coordinator records the
    /// latest one on the session; the orchestrator uses it as a task's
    /// display title unless the user has set a sticky custom title.
    TitleSuggested { title: String },
    /// A prompt was accepted into the agent's currently-running turn rather
    /// than queued for a later turn. `prompt_id` is the delivery confirmation
    /// used to retire the coordinator outbox row and un-grey the optimistic
    /// user message without synthesizing another `RunStarted`.
    PromptSteered { prompt_id: String },
    // ── ADR 0089: generic tool protocol. APPENDED after PromptSteered so
    //    ADR 0089 P5d shifts ToolCallRequested 16→14 with the question break.
    /// A model-facing tool registered by the orchestrator was invoked. The
    /// JSON arguments remain opaque across the harness wire and coordinator.
    ToolCallRequested {
        run_id: String,
        call_id: String,
        name: String,
        args_json: String,
    },
    // ── ADR 0089 P4: parked eviction. APPENDED after ToolCallRequested so
    //    ADR 0089 P5d shifts Parked 17→15 with the question break.
    /// The harness's only outstanding work is deferred tool calls or a
    /// pending user question held open by the agent. The session is eligible
    /// for eviction (the coordinator's soft TTL treats this like `Idle`), but
    /// unlike `Idle` the agent's turn remains open awaiting an external result.
    Parked,
    /// A shell tool call is driving the shared browser. Emitted immediately
    /// before its generic [`HarnessEvent::ToolCallStarted`] with the same id,
    /// so clients can replace the raw Bash/Shell card with a browser-specific
    /// presenter while continuing to use `ToolCallCompleted` for outcome.
    /// `intent` is concise agent-authored display text, sanitized and bounded
    /// by the shared harness SDK.
    BrowserActivity {
        run_id: String,
        tool_call_id: String,
        intent: String,
    },
}

/// Who emitted an [`HarnessEvent::AgentMessage`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// The agent talking to the user / driving the run.
    Assistant,
    /// User-injected content visible in the agent's view (rare;
    /// some adapters surface the prompt itself this way).
    User,
    /// Adapter / agent-system messages (init banners, errors, etc.).
    System,
}

impl HarnessEvent {
    /// Discriminant string used as the `kind` column in `session_events`.
    /// Stable across coordinator restarts — clients that subscribe by
    /// kind (Slackbot, Web UI) match on these strings.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::AgentMessage { .. } => "agent_message",
            Self::ToolCallStarted { .. } => "tool_call_started",
            Self::ToolCallCompleted { .. } => "tool_call_completed",
            Self::RunCompleted { .. } => "run_completed",
            Self::RunInterrupted { .. } => "run_interrupted",
            Self::PromptQueued { .. } => "prompt_queued",
            Self::PromptEdited { .. } => "prompt_edited",
            Self::PromptDequeued { .. } => "prompt_dequeued",
            Self::AgentMessageChunk { .. } => "agent_message_chunk",
            Self::FileChanged { .. } => "file_changed",
            Self::TitleSuggested { .. } => "title_suggested",
            Self::PromptSteered { .. } => "prompt_steered",
            Self::ToolCallRequested { .. } => "tool_call_requested",
            Self::Idle => "harness_idle",
            Self::Parked => "harness_parked",
            Self::BrowserActivity { .. } => "browser_activity",
        }
    }

    /// Tool-call ID if this event carries one. Useful for callers
    /// that want to correlate Started/Completed pairs (e.g. a Web UI
    /// rendering the agent's play-by-play timeline).
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolCallStarted { tool_call_id, .. }
            | Self::ToolCallCompleted { tool_call_id, .. }
            | Self::FileChanged { tool_call_id, .. }
            | Self::BrowserActivity { tool_call_id, .. } => Some(tool_call_id.as_str()),
            Self::ToolCallRequested { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        }
    }
}

/// Commands the host sends to the harness mid-stream. The harness
/// responds at a safe boundary (i.e. between tool calls), not in the
/// middle of an in-flight call.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HarnessCommand {
    /// Flush any pending transcript writes; ack when the transcript
    /// is durably on disk (so `git add -A && git push` would capture
    /// it). Reason is informational — logged on the host side.
    Checkpoint { reason: CheckpointReason },
    /// Clean shutdown. `grace_secs` is how long the host will wait
    /// for the harness to exit before escalating. Each adapter
    /// translates this into the right signal sequence for its agent
    /// (SIGINT then SIGKILL for Claude Code; whatever for others).
    Shutdown { grace_secs: u32 },
    /// User prompt for the agent — the next run's input. `prompt_id` is
    /// a client-minted id (the coordinator mints one if the client
    /// didn't) that correlates this prompt with its eventual
    /// `RunStarted{prompt_id}` and, while queued, with
    /// `PromptQueued`/`PromptEdited`/`PromptDequeued`. The adapter either
    /// starts a run immediately (if Idle) or, if a run is in flight,
    /// QUEUES it (type-ahead) and emits `PromptQueued` — the harness is
    /// the single-writer owner of that queue and writes it to the agent
    /// only at the consumption boundary. Engram is opaque to the agent's
    /// internal session shape — `text` is just plumbed through.
    ///
    /// `mode` (ADR 0107) is an optional session-mode directive that
    /// applies from this prompt's turn onward (e.g. `plan`). The harness
    /// latches it to `/workspace/.engrams/mode` — the latch, not this
    /// field, is the durable source of truth across evict/resume — and
    /// maps it to native behavior at the turn boundary. `None` means "no
    /// change". Adding this field was a sanctioned positional-bincode
    /// wire break (ADR 0089 P5d shape), re-pinned in
    /// tests/wire_golden.rs and shipped with a same-train coordinator +
    /// host + harness-bundle deploy.
    Prompt {
        text: String,
        prompt_id: String,
        mode: Option<String>,
    },
    /// ADR 0030: operator interrupt — stop the in-flight run but keep
    /// the session alive. The adapter SIGINTs its current child (for
    /// Claude: the per-prompt `claude` process), emits
    /// `HarnessEvent::RunInterrupted` + `Idle`, and stays attached to
    /// accept the next prompt (which resumes via `--resume`). A no-op
    /// if no run is in flight. NOT a process kill — unlike `Shutdown`,
    /// the adapter does not exit.
    Interrupt,
    // ── Phase 1b: queue mutation (ADR 0052). ADR 0073 phase 4 removed
    //    `Rehandshake` (the desync watchdog's in-band nudge — the outbox
    //    redelivery loop subsumed the heal), shifting these indices
    //    (EditQueued 5→4, DequeueQueued 6→5, AnswerQuestion 7→6): a
    //    deliberate clean wire break, re-pinned in tests/wire_golden.rs
    //    and shipped with the same-PR harness bundle re-publish.
    /// Edit the text of a still-queued prompt (by its `prompt_id`),
    /// before it is consumed. No-op if already consumed: the harness is
    /// the single writer, so the `RunStarted{prompt_id}` that consumed
    /// it already won. Emits `HarnessEvent::PromptEdited` on success.
    EditQueued { prompt_id: String, text: String },
    /// Remove a still-queued prompt from the queue (by its `prompt_id`)
    /// before consumption — the user pulled it back into the composer or
    /// cancelled it. No-op if already consumed. Emits
    /// `HarnessEvent::PromptDequeued` on success.
    DequeueQueued { prompt_id: String },
    // ── ADR 0089: generic tool result. ADR 0089 P5d removed the preceding
    //    AnswerQuestion variant, shifting ToolResult 7→6 in the sanctioned
    //    flag-day wire break.
    /// The opaque JSON result for an orchestrator-registered tool call.
    ToolResult {
        call_id: String,
        result_json: String,
    },
}

/// Why the host is asking for a checkpoint. Logged in `session_events`
/// so operators can tell the flush flavors apart.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// Host's idle TTL elapsed; we're about to hot-suspend.
    Idle,
    /// RESERVED — the retired cloud-preemption drain (removed 2026-07-21;
    /// the fleet runs no preemptible nodes). Never sent; kept only so the
    /// wire indices of the variants below stay stable for baked in-guest
    /// agentd binaries.
    Preempt,
    /// Operator invoked `POST /sessions/:id/checkpoint`.
    Manual,
    /// Run finished; opportunistic checkpoint to align workspace
    /// with conversation state.
    RunCompleted,
}

/// Harness's reply to a [`HarnessCommand::Checkpoint`]. `ok = false`
/// means the harness couldn't reach a safe boundary in time; the host
/// should retry or fall through to its escalation path.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointAck {
    pub ok: bool,
    pub message: Option<String>,
}

// ---- Session credential-control bridge (ADRs 0023, 0106) ---------------
//
// A *separate* guest→host channel from the harness one above: the
// in-guest `GIT_ASKPASS` helper dials the host on `FORGE_VSOCK_PORT`,
// sends one [`ForgeRequest`], reads one [`ForgeResponse`], and closes.
// The host validates the per-session broker token, mints a fresh git
// credential via the provider [`Integration`], and replies. Same
// 4-byte-length + bincode framing (`read_msg`/`write_msg`).
//
// ADR 0056 P3 folded API access + PR-open onto the egress inject+observe
// plane. ADR 0107 reuses this isolated, one-shot, session-authenticated
// transport for opaque OAuth cache fetch/CAS without adding another VMM port.

/// Vsock port the in-guest forge helper dials (guest→host). Distinct
/// from the harness channel (1026), agentd exec (1024), and agentd
/// ready (1027) ports so the host can demux at accept time.
pub const FORGE_VSOCK_PORT: u32 = 1028;

/// One-shot request from an in-guest forge helper to the host.
/// Authenticated by the per-session credential-broker token (injected
/// into the guest as `ENGRAM_FORGE_TOKEN`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeRequest {
    pub session_id: SessionId,
    pub broker_token: String,
    pub op: ForgeOp,
}

/// The credential operation requested. The historical Forge name stays wire
/// stable for already-baked guests; new operations are trailing variants.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ForgeOp {
    /// Mint a fresh git credential for `host` (optionally scoping the
    /// installation to `owner`). The reply is [`ForgeResponse::Credential`].
    FetchCredential { host: String, owner: Option<String> },
    /// Fetch the OAuth credential bound to this session. The caller cannot
    /// select a provider or subject; both come from the durable binding.
    FetchOAuthCredential,
    /// Publish a refreshed provider cache using compare-and-swap.
    UpdateOAuthCredential {
        expected_version: i64,
        opaque_bundle: Vec<u8>,
    },
}

/// The host's reply to a [`ForgeRequest`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ForgeResponse {
    Credential {
        username: String,
        password: String,
    },
    /// Auth failure, unknown session, no forge configured, or a
    /// provider error — `message` is safe to surface to the guest.
    Error {
        message: String,
    },
    OAuthCredential {
        provider: String,
        version: i64,
        opaque_bundle: Vec<u8>,
    },
}

// ---- Artifact upload bridge (ADR 0026) ---------------------------------
//
// Another *separate* guest→host channel: the in-guest `engram-share`
// helper dials the host on `UPLOAD_VSOCK_PORT`, sends one
// [`UploadRequest`] header frame, then streams the **raw file body**
// (exactly `size_bytes` bytes, NOT length-prefixed or per-chunk framed)
// on the same connection, then reads one [`UploadResponse`] frame.
//
// The raw body is deliberately un-framed: the coord pipes it straight
// into `BlobStorage::put_streaming`, so wrapping each chunk in bincode
// only to unwrap it is pure overhead. `size_bytes` from the header is
// the boundary — the host reads exactly that many bytes, then the
// response frame. `MAX_MSG_BYTES` still caps the header/response frames;
// the body is bounded by `MAX_ARTIFACT_BYTES`, which the coord enforces
// while draining (and aborts + deletes the partial object on exceed).
//
// Authenticated by the same per-session broker token as the forge
// bridge (injected as `ENGRAM_UPLOAD_TOKEN`), but the upload path is NOT
// git-gated — every baked image gets it.

/// Vsock port the in-guest `engram-share` helper dials (guest→host).
/// Distinct from agentd exec (1024), harness (1026), agentd ready
/// (1027), and forge (1028) so the host can demux at accept time.
pub const UPLOAD_VSOCK_PORT: u32 = 1029;

/// Hard cap on a single artifact's body, enforced host-side by the
/// coord while draining the stream. 512 MiB gives headroom for short
/// screen recordings; tune as real usage lands. Distinct from
/// `MAX_MSG_BYTES` (which still caps the header/response *frames*).
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Header frame from the in-guest `engram-share` helper to the host,
/// sent before the raw body stream. Authenticated by the per-session
/// broker token (injected into the guest as `ENGRAM_UPLOAD_TOKEN`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadRequest {
    pub session_id: SessionId,
    pub broker_token: String,
    pub op: UploadOp,
}

/// The artifact operation requested. An enum for symmetry with
/// [`ForgeOp`] and room for future ops (e.g. delete).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UploadOp {
    /// Share a file that will surface in the session's conversation
    /// history. `ext` is the raw filename extension (no leading dot)
    /// the guest derived. The coord stamps the persisted media type
    /// (magic-byte sniff first, declared extension for types with no
    /// magic bytes) and never trusts a guest-supplied MIME.
    /// `size_bytes` is the exact length of the raw body that follows.
    ///
    /// Legacy frame from agentd builds that predate file names on the
    /// wire; new builds send [`UploadOp::ShareFileNamed`]. Kept
    /// decodable forever (bincode variant index 0 — baked images roll
    /// slowly).
    ShareFile {
        ext: String,
        caption: Option<String>,
        size_bytes: u64,
    },
    /// [`UploadOp::ShareFile`] plus the file's basename (e.g.
    /// `report.html`), so the artifact serves and displays under its
    /// real name. Appended as bincode variant index 1 — old coords
    /// reject it cleanly at decode; old guests keep sending variant 0.
    ShareFileNamed {
        ext: String,
        file_name: String,
        caption: Option<String>,
        size_bytes: u64,
    },
}

impl UploadOp {
    /// Exact length of the raw body that follows the header frame —
    /// the transports cap their body reads at this.
    pub fn size_bytes(&self) -> u64 {
        match self {
            Self::ShareFile { size_bytes, .. } | Self::ShareFileNamed { size_bytes, .. } => {
                *size_bytes
            }
        }
    }
}

/// The host's reply to an [`UploadRequest`] (after the body stream).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UploadResponse {
    /// Stored. `media_type` is the coord-detected type, `artifact_id`
    /// the server-generated UUID (string form).
    Shared {
        artifact_id: String,
        media_type: String,
        size_bytes: u64,
    },
    /// Auth failure, unknown session, disallowed media type, over the
    /// size cap / quota, or a storage error — `message` is safe to
    /// surface to the guest.
    Error { message: String },
}

// ---- ADR 0066: port relay (host→guest) ---------------------------------
//
// Unlike the forge (1028) / upload (1029) bridges — guest→host dials by an
// untrusted guest, so token-gated — the port relay is host→guest: the
// host-agent dials the in-guest agentd on `PROXY_PORT_VSOCK_PORT`, sends one
// [`RelayConnect`] frame naming the guest TCP port, reads one [`RelayAck`],
// then the connection carries the raw dev-server bytes (no further framing).
// agentd dials `127.0.0.1:target_port` INSIDE the guest — reaching loopback-
// bound dev servers (Vite, Tilt, `next dev`) a direct dial_ip dial
// cannot. One vsock connection per forwarded TCP connection (no muxing) →
// no head-of-line blocking (ADR 0066).

/// Vsock port the in-guest agentd relay listener binds (host→guest).
/// Distinct from agentd exec (1024), harness (1026), agentd ready (1027),
/// forge (1028), and upload (1029) so the guest can demux at accept time.
pub const PROXY_PORT_VSOCK_PORT: u32 = 1030;

/// First frame the host-agent sends on a relay connection: the guest TCP
/// port to dial on `127.0.0.1`. [`read_msg`] consumes exactly this frame
/// (via `read_exact`), so the raw byte stream that follows is untouched.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayConnect {
    pub target_port: u16,
}

/// agentd's reply after attempting the `127.0.0.1:target_port` dial, sent
/// before any bytes are spliced. `ok: false` (with `error`) lets the host
/// surface "dev server unreachable" as a synchronous `proxy_port` error →
/// a clean 502, preserving ADR 0064's fail-fast contract.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayAck {
    pub ok: bool,
    pub error: Option<String>,
}

// ---- Framing -----------------------------------------------------------

/// Read one length-prefixed bincode frame. Mirrors
/// `engram-agentd::proto::read_msg` so adapters can share a single
/// codec.
pub async fn read_msg<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: AsyncReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("harness frame length {len} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}")))
}

/// Bincode-encode `msg` and write as a length-prefixed frame.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}"))
    })?;
    if body.len() > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "encoded harness frame {} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})",
                body.len()
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: T) {
        let mut buf = Vec::new();
        let fut = write_msg(&mut buf, &value);
        futures_block_on(fut).unwrap();
        let mut cur = Cursor::new(buf);
        let got: T = futures_block_on(read_msg(&mut cur)).unwrap();
        assert_eq!(got, value);
    }

    fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(f)
    }

    #[test]
    fn attach_round_trip() {
        round_trip(HarnessAttach {
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            binding_epoch: 5,
            harness_version: "engram-harness-noop/0.1.0".into(),
        });
    }

    #[test]
    fn attach_ack_round_trip() {
        round_trip(HarnessAttachAck {
            ok: true,
            reject: None,
            message: None,
        });
        round_trip(HarnessAttachAck {
            ok: false,
            reject: Some(AttachReject::Superseded),
            message: Some("unknown session".into()),
        });
    }

    #[test]
    fn event_variants_round_trip() {
        round_trip(HarnessFrame::Event(HarnessEvent::RunStarted {
            run_id: "r1".into(),
            prompt_id: Some("p1".into()),
            prompt_summary: Some("fix the test".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::PromptQueued {
            prompt_id: "p2".into(),
            summary: Some("and then deploy".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::PromptEdited {
            prompt_id: "p2".into(),
            summary: Some("and then deploy to staging".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::PromptDequeued {
            prompt_id: "p2".into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::ToolCallStarted {
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "Bash".into(),
            args_summary: Some("cargo test".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::BrowserActivity {
            run_id: "r".into(),
            tool_call_id: "t".into(),
            intent: "Clicking Sign in".into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::AgentMessage {
            run_id: "r1".into(),
            message_id: "m1".into(),
            role: AgentRole::Assistant,
            text: "hello".into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::ToolCallCompleted {
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "Bash".into(),
            ok: true,
            duration_ms: 12345,
            result_summary: Some("done".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::RunCompleted {
            run_id: "r1".into(),
            ok: true,
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::RunInterrupted {
            run_id: "r1".into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::Idle));
        round_trip(HarnessFrame::Event(HarnessEvent::TitleSuggested {
            title: "Fix the flaky test".into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::ToolCallRequested {
            run_id: "r1".into(),
            call_id: "call_1".into(),
            name: "save_memory".into(),
            args_json: r#"{"text":"remember this"}"#.into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::Parked));
    }

    #[test]
    fn command_variants_round_trip() {
        round_trip(HarnessFrame::Command(HarnessCommand::Checkpoint {
            reason: CheckpointReason::Idle,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Checkpoint {
            reason: CheckpointReason::Preempt,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Shutdown {
            grace_secs: 5,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Prompt {
            prompt_id: "p1".into(),
            text: "do the thing".into(),
            mode: None,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Prompt {
            prompt_id: "p2".into(),
            text: "plan the thing".into(),
            mode: Some("plan".into()),
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::EditQueued {
            prompt_id: "p1".into(),
            text: "do the thing, carefully".into(),
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::DequeueQueued {
            prompt_id: "p1".into(),
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Interrupt));
        round_trip(HarnessFrame::Command(HarnessCommand::ToolResult {
            call_id: "call_1".into(),
            result_json: r#"{"saved":true}"#.into(),
        }));
    }

    #[test]
    fn forge_request_response_round_trip() {
        round_trip(ForgeRequest {
            session_id: SessionId::new(),
            broker_token: "tok".into(),
            op: ForgeOp::FetchCredential {
                host: "github.com".into(),
                owner: Some("cortexapps".into()),
            },
        });
        round_trip(ForgeResponse::Credential {
            username: "x-access-token".into(),
            password: "ghs_x".into(),
        });
        round_trip(ForgeResponse::Error {
            message: "nope".into(),
        });
    }

    #[test]
    fn upload_named_round_trip() {
        round_trip(UploadRequest {
            session_id: SessionId::new(),
            broker_token: "tok".into(),
            op: UploadOp::ShareFileNamed {
                ext: "html".into(),
                file_name: "report.html".into(),
                caption: None,
                size_bytes: 2048,
            },
        });
    }

    #[test]
    fn upload_request_response_round_trip() {
        round_trip(UploadRequest {
            session_id: SessionId::new(),
            broker_token: "tok".into(),
            op: UploadOp::ShareFile {
                ext: "png".into(),
                caption: Some("the dashboard after my change".into()),
                size_bytes: 4096,
            },
        });
        round_trip(UploadResponse::Shared {
            artifact_id: "0190f3a2c0f17e2cba12".into(),
            media_type: "image/png".into(),
            size_bytes: 4096,
        });
        round_trip(UploadResponse::Error {
            message: "unsupported media type".into(),
        });
    }

    #[test]
    fn agent_message_round_trips_with_unicode() {
        let ev = HarnessEvent::AgentMessage {
            run_id: "r".into(),
            message_id: "m".into(),
            role: AgentRole::Assistant,
            text: "I see — let me check the workspace 🔧".into(),
        };
        let mut buf = Vec::new();
        futures_block_on(write_msg(&mut buf, &ev)).unwrap();
        let mut cur = Cursor::new(buf);
        let got: HarnessEvent = futures_block_on(read_msg(&mut cur)).unwrap();
        match got {
            HarnessEvent::AgentMessage { text, role, .. } => {
                assert_eq!(text, "I see — let me check the workspace 🔧");
                assert_eq!(role, AgentRole::Assistant);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn read_rejects_oversized_length() {
        let bad = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let mut cur = Cursor::new(bad.to_vec());
        let err = futures_block_on(read_msg::<_, HarnessFrame>(&mut cur)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    }

    #[test]
    fn event_kind_strings_are_stable() {
        // Persisted clients (Slackbot, audit log) match on these.
        // Changing them is a breaking change — this test forces a
        // deliberate update if anyone tries.
        assert_eq!(
            HarnessEvent::RunStarted {
                run_id: "x".into(),
                prompt_id: None,
                prompt_summary: None,
            }
            .kind(),
            "run_started"
        );
        assert_eq!(
            HarnessEvent::ToolCallStarted {
                run_id: "x".into(),
                tool_call_id: "t".into(),
                tool_name: "B".into(),
                args_summary: None,
            }
            .kind(),
            "tool_call_started"
        );
        assert_eq!(
            HarnessEvent::ToolCallCompleted {
                run_id: "x".into(),
                tool_call_id: "t".into(),
                tool_name: "B".into(),
                ok: true,
                duration_ms: 0,
                result_summary: None,
            }
            .kind(),
            "tool_call_completed"
        );
        assert_eq!(
            HarnessEvent::AgentMessage {
                run_id: "x".into(),
                message_id: "m".into(),
                role: AgentRole::Assistant,
                text: "hi".into(),
            }
            .kind(),
            "agent_message"
        );
        assert_eq!(
            HarnessEvent::RunCompleted {
                run_id: "x".into(),
                ok: true,
            }
            .kind(),
            "run_completed"
        );
        assert_eq!(
            HarnessEvent::RunInterrupted { run_id: "x".into() }.kind(),
            "run_interrupted"
        );
        assert_eq!(
            HarnessEvent::PromptQueued {
                prompt_id: "p".into(),
                summary: None,
            }
            .kind(),
            "prompt_queued"
        );
        assert_eq!(
            HarnessEvent::PromptEdited {
                prompt_id: "p".into(),
                summary: None,
            }
            .kind(),
            "prompt_edited"
        );
        assert_eq!(
            HarnessEvent::PromptDequeued {
                prompt_id: "p".into()
            }
            .kind(),
            "prompt_dequeued"
        );
        assert_eq!(HarnessEvent::Idle.kind(), "harness_idle");
        assert_eq!(HarnessEvent::Parked.kind(), "harness_parked");
        assert_eq!(
            HarnessEvent::BrowserActivity {
                run_id: "r".into(),
                tool_call_id: "t".into(),
                intent: "Clicking Sign in".into(),
            }
            .kind(),
            "browser_activity"
        );
        assert_eq!(
            HarnessEvent::TitleSuggested { title: "t".into() }.kind(),
            "title_suggested"
        );
        assert_eq!(
            HarnessEvent::ToolCallRequested {
                run_id: "x".into(),
                call_id: "call_1".into(),
                name: "save_memory".into(),
                args_json: r#"{"text":"remember this"}"#.into(),
            }
            .kind(),
            "tool_call_requested"
        );
    }

    #[test]
    fn tool_call_id_only_set_for_tool_call_events() {
        assert_eq!(HarnessEvent::Idle.tool_call_id(), None);
        assert_eq!(
            HarnessEvent::RunStarted {
                run_id: "x".into(),
                prompt_id: None,
                prompt_summary: None,
            }
            .tool_call_id(),
            None
        );
        let id = "tool-42";
        assert_eq!(
            HarnessEvent::ToolCallStarted {
                run_id: "x".into(),
                tool_call_id: id.into(),
                tool_name: "Read".into(),
                args_summary: None,
            }
            .tool_call_id(),
            Some(id)
        );
        assert_eq!(
            HarnessEvent::ToolCallRequested {
                run_id: "x".into(),
                call_id: id.into(),
                name: "save_memory".into(),
                args_json: "{}".into(),
            }
            .tool_call_id(),
            Some(id)
        );
        assert_eq!(
            HarnessEvent::BrowserActivity {
                run_id: "x".into(),
                tool_call_id: id.into(),
                intent: "Inspecting page".into(),
            }
            .tool_call_id(),
            Some(id)
        );
    }
}
