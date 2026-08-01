use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_core::types::{ExecRusage, SessionState};
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use engram_harness_proto::{FileChange, HarnessEvent};
use engram_host_agent::harness::{EventSink, HarnessHub};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::CoordinatorConfig;
use crate::host_registry::HostRegistry;
use crate::Services;

// ---------------------------------------------------------------------
// SessionEvent — typed feed for any client that wants to follow a
// session. Web app, Slack thread, CLI, IDE — they all subscribe to the
// same stream of these events for a given SessionId. Designed so
// reconnect / multi-subscriber / late-join all work without changing
// the wire format.
// ---------------------------------------------------------------------

/// A single ordered event in a session's lifetime. Ordering across
/// the persistent log is total: every event has a unique per-session
/// `idx` allocated atomically by `MetadataStore::append_session_event`.
/// In-memory bus delivery is best-effort under load (broadcast lag).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Session moved between lifecycle states.
    StatusChanged {
        from: SessionState,
        to: SessionState,
        at: DateTime<Utc>,
    },
    /// Issue #527 Phase 1: the durable "the user asked at time T" fact.
    /// Emitted as the FIRST PG write of `send_prompt_core`, before the
    /// outbox enqueue whose deliver op auto-resumes the session — unlike
    /// the user-echo `HarnessAgentMessage`, which is deliberately ordered
    /// AFTER the receipt to satisfy ADR 0052 type-ahead rendering. This
    /// event exists purely for measurement: it is the receipt anchor
    /// `engram_prompt_to_run_started_seconds` joins against
    /// `run_started{prompt_id}` to compute true prompt→first-token
    /// latency, replacing the `idle→created` proxy (which is a lower
    /// bound because it post-dates the resume). Coordinator-authoritative
    /// — stays true across a guest-state rewind, so
    /// `rewind_session_to_cursor` excludes this kind from its tombstone
    /// UPDATE (the user genuinely did send the prompt).
    PromptReceived {
        prompt_id: String,
        at: DateTime<Utc>,
    },
    /// ADR 0089: the coordinator accepted a tool result for durable
    /// delivery. This is intentionally visible before the harness consumes
    /// it so surfaces can resolve pending UI immediately.
    ToolResultSubmitted {
        tool_call_id: String,
        result_json: String,
        at: DateTime<Utc>,
    },
    /// `POST /sessions/:id/exec*` started a new command. `exec_id` is
    /// the sandbox-side identifier; downstream Stdout/Stderr/Exit
    /// events for this run carry the same value so multiplexed clients
    /// can demux.
    ExecStarted {
        exec_id: String,
        command: Vec<String>,
        at: DateTime<Utc>,
    },
    ExecCompleted {
        exec_id: String,
        exit_status: Option<i32>,
        rusage: ExecRusage,
        at: DateTime<Utc>,
    },
    Stdout {
        exec_id: String,
        /// UTF-8 lossy: bytes that aren't valid UTF-8 are replaced.
        /// Subscribers wanting raw bytes use the per-exec stream
        /// endpoint (which preserves them).
        chunk: String,
        /// ADR 0103: absolute RAW byte range of this chunk within the
        /// exec's stdout stream (wire offsets, not lossy-string lengths).
        /// Recording is observation-independent: a re-attach skips
        /// persisting at or below the ticket's recorded `bytes_end`
        /// high-water mark, so attaching N times records the same rows as
        /// attaching once.
        bytes_start: u64,
        bytes_end: u64,
    },
    Stderr {
        exec_id: String,
        chunk: String,
        bytes_start: u64,
        bytes_end: u64,
    },
    SnapshotTaken {
        snapshot_id: SnapshotId,
        size_bytes: u64,
        at: DateTime<Utc>,
    },
    Evicted {
        at: DateTime<Utc>,
    },
    Resumed {
        snapshot_id: SnapshotId,
        at: DateTime<Utc>,
    },
    /// The coordinator began waking an idle/parked session back up (the
    /// resume op claimed and started restoring). Emitted BEFORE the
    /// multi-second restore + harness (re)attach, so a surface has
    /// something to render the instant a user prompts an evicted session
    /// instead of dead air until `StatusChanged{→Active}` lands (the
    /// 2026-07-17 incident UX: prompt → 40 minutes of nothing visible).
    /// Coordinator-authoritative — like `prompt_received`/`status_changed`
    /// it stays true across an ADR-0028 recovery rewind (the coordinator
    /// genuinely started the resume), so `rewind_session_to_cursor`
    /// EXCLUDES it from the tombstone UPDATE; without that a
    /// resume-with-rollback would grey the marker and inflate
    /// `rolled_back` by one (the same class ADR 0091 fixed for
    /// `harness_idle`). The web renders it as a transient "waking up…"
    /// indicator that resolves when the run's first event arrives.
    ResumeStarted {
        at: DateTime<Utc>,
    },
    /// Phase 4: harness-emitted events. Web UI and Slackbot
    /// subscribe to these to render the agent's play-by-play.
    /// Track B reshape: structured summary fields replace the
    /// opaque `transcript_delta` bytes — chat consumers render
    /// directly without parsing agent-native formats. New
    /// `HarnessAgentMessage` variant carries assistant text
    /// between tool calls.
    HarnessRunStarted {
        run_id: String,
        prompt_summary: Option<String>,
        /// Phase 1b: client-minted id of the prompt that started this run
        /// — the "queued prompt consumed" signal the web uses to move a
        /// greyed type-ahead item into the conversation. `None` for the
        /// env-seeded initial prompt. `#[serde(default)]` so events
        /// persisted before this field decode as `None`.
        #[serde(default)]
        prompt_id: Option<String>,
        at: DateTime<Utc>,
    },
    HarnessAgentMessage {
        run_id: String,
        message_id: String,
        role: engram_harness_proto::AgentRole,
        text: String,
        /// Phase 1b: set on the coord-emitted USER echo to the client's
        /// `prompt_id`, so the web dedupes its optimistic bubble against
        /// this event (the double-render fix). `None` for assistant/system
        /// messages the harness emits. `#[serde(default)]` for back-compat.
        #[serde(default)]
        prompt_id: Option<String>,
        at: DateTime<Utc>,
    },
    HarnessToolCallStarted {
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        args_summary: Option<String>,
        at: DateTime<Utc>,
    },
    HarnessToolCallCompleted {
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        ok: bool,
        duration_ms: u64,
        result_summary: Option<String>,
        at: DateTime<Utc>,
    },
    /// A native shell tool is driving the shared browser. Correlates to the
    /// generic tool start/completion via `tool_call_id`; web clients replace
    /// the raw shell card with a browser presenter.
    HarnessBrowserActivity {
        run_id: String,
        tool_call_id: String,
        intent: String,
        at: DateTime<Utc>,
    },
    /// ADR 0089: an orchestrator-registered tool was invoked. The
    /// coordinator preserves the JSON arguments verbatim and never parses
    /// their tool-specific shape.
    HarnessToolCallRequested {
        run_id: String,
        tool_call_id: String,
        name: String,
        args_json: String,
        at: DateTime<Utc>,
    },
    HarnessRunCompleted {
        run_id: String,
        ok: bool,
        at: DateTime<Utc>,
    },
    /// ADR 0030: the in-flight run was stopped by an operator interrupt
    /// (`POST /sessions/:id/interrupt` → the harness SIGINT'd its child).
    /// Distinct from `HarnessRunCompleted` so the transcript shows an
    /// "interrupted" marker; the session stays alive and resumable.
    HarnessRunInterrupted {
        run_id: String,
        at: DateTime<Utc>,
    },
    HarnessIdle {
        at: DateTime<Utc>,
    },
    /// The harness has an open agent turn whose only outstanding work is
    /// deferred external calls. Eviction-eligible like `HarnessIdle`, but the
    /// turn remains open until a result arrives.
    HarnessParked {
        at: DateTime<Utc>,
    },
    /// Phase 1b: a prompt arrived while a run was in flight and was
    /// queued (type-ahead / steering). The harness owns the queue; the
    /// web renders this as a greyed, editable composer item keyed on
    /// `prompt_id` until `HarnessRunStarted{prompt_id}` consumes it.
    HarnessPromptQueued {
        prompt_id: String,
        summary: Option<String>,
        at: DateTime<Utc>,
    },
    /// Phase 1b: a still-queued prompt's text was edited before consumption.
    HarnessPromptEdited {
        prompt_id: String,
        summary: Option<String>,
        at: DateTime<Utc>,
    },
    /// Phase 1b: a still-queued prompt was removed before consumption
    /// (pulled back to the composer or cancelled).
    HarnessPromptDequeued {
        prompt_id: String,
        at: DateTime<Utc>,
    },
    /// The harness injected this prompt into the currently-running turn.
    HarnessPromptSteered {
        prompt_id: String,
        at: DateTime<Utc>,
    },
    /// Phase 1c (ADR 0052): one live token delta of the in-flight
    /// assistant message. EPHEMERAL — fanned out to live SSE subscribers
    /// only (never appended to `session_events`); the terminal
    /// `HarnessAgentMessage` with the same `message_id` is the durable
    /// record and supersedes every chunk. Carried cross-replica inline on
    /// the `session_event_deltas` NOTIFY channel (NOT the persisted-row
    /// path), so a replica without the harness connection still streams.
    HarnessAgentMessageChunk {
        run_id: String,
        message_id: String,
        chunk: String,
        at: DateTime<Utc>,
    },
    /// ADR 0054 Flavor A: the agent successfully changed a file via a
    /// `Write`/`Edit`/`MultiEdit` tool. The web renders a rich diff (red/green
    /// hunks for an edit, all-green for a write) in place of the generic tool
    /// card, correlated by `tool_call_id`. Opaque JSONB on `session_events`
    /// like every other passthrough event — no migration.
    HarnessFileChanged {
        run_id: String,
        tool_call_id: String,
        path: String,
        change: FileChange,
        at: DateTime<Utc>,
    },
    /// Session titles: the harness proposed a short LLM-generated title for the
    /// session (Claude Code's `ai-title`). Persisted to the log for history AND
    /// materialized onto `sessions.suggested_title` by the event sink, so the
    /// orchestrator can surface it as a task's display title. Opaque JSONB like
    /// the other passthrough events — no `session_events` migration.
    HarnessTitleSuggested {
        title: String,
        at: DateTime<Utc>,
    },
    /// ADR 0056: a third-party integration surfaced a typed asset/action
    /// into the session. Subsumes the retired `PullRequestOpened` — an
    /// opened PR is now `provider: "forge"`, `asset_kind: "pull_request"`.
    /// `provider` + `asset_kind` discriminate; the web keys its renderer on
    /// that pair (with a generic fallback). The wire carries NO rendering
    /// instructions — presentation lives in the web/orchestrator layer
    /// (ADR 0056 §4). `data` is the typed semantic payload the renderer
    /// reads. Always emitted coordinator-side at the broker seam, never a
    /// guest self-report of arbitrary metadata.
    IntegrationAsset {
        /// The integration that produced this, e.g. `"forge"`. Namespaces
        /// `asset_kind` so two providers can't collide.
        provider: String,
        /// Provider-scoped type, e.g. `"pull_request"`.
        asset_kind: String,
        /// Durable asset (survives an ADR 0028 recovery rewind as a
        /// side-effect) vs. transient action log.
        surface: AssetSurface,
        /// The typed semantic payload the web renderer reads — for a PR,
        /// `{repo, title, number, head_branch, base_branch}`.
        data: serde_json::Value,
        /// Where the asset's bytes / URL live: an ADR 0026 artifact the
        /// platform serves, or an external link it doesn't host (a PR page).
        #[serde(default)]
        fetchable: Option<FetchableRef>,
        at: DateTime<Utc>,
    },
    /// ADR 0026: a file artifact was shared into this session and
    /// surfaces in the conversation history. `media_type` is the
    /// coord-detected type (never the guest-supplied one); the web
    /// transcript renders an image/video inline and anything else as a
    /// download chip. `artifact_id` keys the serve endpoint
    /// `GET /sessions/:id/artifacts/:artifact_id`.
    FileShared {
        artifact_id: String,
        media_type: String,
        size_bytes: u64,
        caption: Option<String>,
        at: DateTime<Utc>,
    },
    /// ADR 0028 A.log: a rung-1 recovery rewound the live transcript
    /// to a checkpoint. The boundary the web renders ("↩ Recovered
    /// from a checkpoint after a host failure; ~N messages after this
    /// point were rolled back") + the surviving outside-world
    /// side-effects the platform can't undo (opened PRs, shared
    /// files). The FIRST event of the post-recovery epoch — everything
    /// before it with `idx > through_idx` is tombstoned (rendered
    /// collapsed/greyed), everything after is the resumed thread.
    RecoveredFromCheckpoint {
        /// The new recovery epoch (events after this carry it).
        recovery_epoch: i64,
        /// The checkpoint's `events_cursor` — the live head reset here.
        through_idx: i64,
        /// How many events were rolled back.
        rolled_back: u64,
        /// Outside-world side-effects in the rolled-back span that
        /// survive (one human-readable line each).
        surviving_side_effects: Vec<String>,
        /// ADR 0045 F1: why the rewind happened, so the web doesn't
        /// label a planned operator move as a host failure. `#[serde(default)]`
        /// → events persisted before this field default to the historical
        /// meaning (`HostFailureRecovery`).
        #[serde(default)]
        cause: RecoveryCause,
        at: DateTime<Utc>,
    },
    /// ADR 0090 (2026-07-20 durability-rollback incident): a
    /// quarantined-survivor eviction exhausted its retry budget, so the
    /// coordinator DESTROYED the structurally-crippled VM. The session then
    /// converges HostLost → Idle and its NEXT resume rewinds to the last
    /// published disk manifest — silently discarding any guest writes the
    /// host acked but never uploaded past that manifest version (the
    /// incident: 134/100/50 MiB tails across three sessions, previously
    /// inferable only by hand-diffing host spool logs against manifest
    /// versions). This event makes that data loss LOUD and durable so the
    /// web timeline / CLI can surface it and alerting can key on it.
    /// Coordinator-authoritative — it records a fact the destroy already
    /// made true, so it survives the very rewind it warns about
    /// (`rewind_session_to_cursor` EXCLUDES this kind from its tombstone
    /// UPDATE, like the other control-plane facts).
    DurabilityRollback {
        /// The crippled sandbox the coordinator destroyed.
        sandbox_id: SandboxId,
        /// The last published disk manifest the next resume rewinds to
        /// (the session's `live_disk_manifest`). `None` when the session
        /// never got a live publish — the resume falls back to the
        /// snapshot's disk lineage.
        rewind_disk_manifest: Option<engram_core::types::manifest::ManifestRef>,
        /// Human-readable cause (e.g. "quarantined-survivor evict budget
        /// exhausted; VM destroyed").
        reason: String,
        at: DateTime<Utc>,
    },
    /// ADR 0107: a validated session-mode directive rode a prompt (e.g.
    /// `plan`). Coordinator-authoritative — the user genuinely selected the
    /// mode — so `rewind_session_to_cursor` excludes this kind from its
    /// tombstone UPDATE, like `prompt_received`. The web derives the current
    /// mode chip from the latest of these plus plan-approval results.
    HarnessModeChanged {
        mode: String,
        at: DateTime<Utc>,
    },
}

/// ADR 0045 F1: why a rung-1 recovery rewound the transcript. Drives the
/// web copy on the recovery boundary — a planned operator relocation
/// (drain / teleport) must not read as "recovered after a host failure",
/// because no host failed. Defaults to [`RecoveryCause::HostFailureRecovery`]
/// for events persisted before this field existed (the card's original
/// meaning) and for the unplanned `/resume`-after-death path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCause {
    /// An operator deliberately relocated a live session (drain or
    /// teleport, ADR 0045 Phase F). The snapshot-rehome resumes from the
    /// last durable checkpoint, so the post-checkpoint tail is rolled
    /// back — but nothing failed; it was a planned move.
    PlannedRelocation,
    /// A host died (or a session was resumed from `Idle` after one did),
    /// so the restored checkpoint lags the lost live head — the original
    /// meaning of the recovery boundary.
    #[default]
    HostFailureRecovery,
    /// ADR 0091: a routine resume from `Idle` whose latest checkpoint
    /// nevertheless lags real guest activity (e.g. the eviction's
    /// terminal capture failed and recovery fell back to a prior
    /// periodic checkpoint). Nothing "failed" at resume time and no host
    /// died — the copy must say "resumed from an earlier checkpoint",
    /// not cry host-failure. Clean cycles emit NO recovery event at all
    /// (zero rolled-back rows short-circuit before the cause is used).
    CheckpointLag,
}

impl RecoveryCause {
    /// The serde wire spelling, for log fields and metric labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlannedRelocation => "planned_relocation",
            Self::HostFailureRecovery => "host_failure_recovery",
            Self::CheckpointLag => "checkpoint_lag",
        }
    }
}

/// ADR 0056: an [`SessionEvent::IntegrationAsset`] is either a durable noun
/// (a PR, a shared file) that survives an ADR 0028 recovery rewind as a
/// side-effect the platform can't undo, or a transient verb (a query the
/// agent ran) that does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetSurface {
    /// A verb the agent performed — a transient log line, not a surviving
    /// side-effect on recovery.
    Action,
    /// A durable noun that persists in the session; counted as a surviving
    /// side-effect on an ADR 0028 recovery rewind.
    Asset,
}

/// ADR 0056: where an [`SessionEvent::IntegrationAsset`]'s bytes / URL live.
/// `Artifact` reuses the ADR 0026 artifact serve endpoint
/// (`GET /sessions/:id/artifacts/:artifact_id`); `External` is a link the
/// platform doesn't host (a PR page). Serialized tagged on `kind` so the
/// web can discriminate without positional knowledge.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FetchableRef {
    External {
        url: String,
    },
    Artifact {
        artifact_id: String,
        media_type: String,
        size_bytes: u64,
    },
}

impl SessionEvent {
    /// Discriminant string used both as the SSE `event:` field and as
    /// the `kind` column in `session_events`. Stable across
    /// coordinator restarts; persisted clients (Slack, etc.) match on
    /// this.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::StatusChanged { .. } => "status_changed",
            Self::PromptReceived { .. } => "prompt_received",
            Self::ToolResultSubmitted { .. } => "tool_result_submitted",
            Self::HarnessModeChanged { .. } => "harness_mode_changed",
            Self::ExecStarted { .. } => "exec_started",
            Self::ExecCompleted { .. } => "exec_completed",
            Self::Stdout { .. } => "stdout",
            Self::Stderr { .. } => "stderr",
            Self::SnapshotTaken { .. } => "snapshot_taken",
            Self::Evicted { .. } => "evicted",
            Self::Resumed { .. } => "resumed",
            Self::ResumeStarted { .. } => "resume_started",
            Self::HarnessRunStarted { .. } => "run_started",
            Self::HarnessAgentMessage { .. } => "agent_message",
            Self::HarnessToolCallStarted { .. } => "tool_call_started",
            Self::HarnessToolCallCompleted { .. } => "tool_call_completed",
            Self::HarnessBrowserActivity { .. } => "browser_activity",
            Self::HarnessToolCallRequested { .. } => "tool_call_requested",
            Self::HarnessRunCompleted { .. } => "run_completed",
            Self::HarnessRunInterrupted { .. } => "run_interrupted",
            Self::HarnessIdle { .. } => "harness_idle",
            Self::HarnessParked { .. } => "harness_parked",
            Self::HarnessPromptQueued { .. } => "prompt_queued",
            Self::HarnessPromptEdited { .. } => "prompt_edited",
            Self::HarnessPromptDequeued { .. } => "prompt_dequeued",
            Self::HarnessPromptSteered { .. } => "prompt_steered",
            Self::HarnessAgentMessageChunk { .. } => "agent_message_chunk",
            Self::HarnessFileChanged { .. } => "file_changed",
            Self::HarnessTitleSuggested { .. } => "title_suggested",
            Self::IntegrationAsset { .. } => "integration_asset",
            Self::FileShared { .. } => "file_shared",
            Self::RecoveredFromCheckpoint { .. } => "recovered_from_checkpoint",
            Self::DurabilityRollback { .. } => "durability_rollback",
        }
    }

    /// Convert a wire [`HarnessEvent`] into the coord-side
    /// [`SessionEvent`]. Used by the host-agent → session_events
    /// bridge that Track A.3 wires through `EventSink`.
    pub fn from_harness(ev: HarnessEvent, at: DateTime<Utc>) -> Self {
        match ev {
            HarnessEvent::RunStarted {
                run_id,
                prompt_summary,
                prompt_id,
            } => Self::HarnessRunStarted {
                run_id,
                prompt_summary,
                prompt_id,
                at,
            },
            HarnessEvent::AgentMessage {
                run_id,
                message_id,
                role,
                text,
            } => Self::HarnessAgentMessage {
                run_id,
                message_id,
                role,
                text,
                // Harness-emitted messages are assistant/system; the user
                // echo (which carries a prompt_id) is emitted by the coord.
                prompt_id: None,
                at,
            },
            HarnessEvent::ToolCallStarted {
                run_id,
                tool_call_id,
                tool_name,
                args_summary,
            } => Self::HarnessToolCallStarted {
                run_id,
                tool_call_id,
                tool_name,
                args_summary,
                at,
            },
            HarnessEvent::ToolCallCompleted {
                run_id,
                tool_call_id,
                tool_name,
                ok,
                duration_ms,
                result_summary,
            } => Self::HarnessToolCallCompleted {
                run_id,
                tool_call_id,
                tool_name,
                ok,
                duration_ms,
                result_summary,
                at,
            },
            HarnessEvent::BrowserActivity {
                run_id,
                tool_call_id,
                intent,
            } => Self::HarnessBrowserActivity {
                run_id,
                tool_call_id,
                intent,
                at,
            },
            HarnessEvent::ToolCallRequested {
                run_id,
                call_id,
                name,
                args_json,
            } => Self::HarnessToolCallRequested {
                run_id,
                tool_call_id: call_id,
                name,
                args_json,
                at,
            },
            HarnessEvent::RunCompleted { run_id, ok } => {
                Self::HarnessRunCompleted { run_id, ok, at }
            }
            HarnessEvent::RunInterrupted { run_id } => Self::HarnessRunInterrupted { run_id, at },
            HarnessEvent::Idle => Self::HarnessIdle { at },
            HarnessEvent::Parked => Self::HarnessParked { at },
            HarnessEvent::PromptQueued { prompt_id, summary } => Self::HarnessPromptQueued {
                prompt_id,
                summary,
                at,
            },
            HarnessEvent::PromptEdited { prompt_id, summary } => Self::HarnessPromptEdited {
                prompt_id,
                summary,
                at,
            },
            HarnessEvent::PromptDequeued { prompt_id } => {
                Self::HarnessPromptDequeued { prompt_id, at }
            }
            HarnessEvent::PromptSteered { prompt_id } => {
                Self::HarnessPromptSteered { prompt_id, at }
            }
            HarnessEvent::AgentMessageChunk {
                run_id,
                message_id,
                chunk,
            } => Self::HarnessAgentMessageChunk {
                run_id,
                message_id,
                chunk,
                at,
            },
            HarnessEvent::FileChanged {
                run_id,
                tool_call_id,
                path,
                change,
            } => Self::HarnessFileChanged {
                run_id,
                tool_call_id,
                path,
                change,
                at,
            },
            HarnessEvent::TitleSuggested { title } => Self::HarnessTitleSuggested { title, at },
        }
    }
}

/// In-memory + persisted-log pair. Carries the SessionEvent itself
/// plus the monotonic `idx` allocated when it was persisted, so SSE
/// subscribers can put `id: <idx>` on the wire and reconnecting
/// clients can resume from `Last-Event-ID`.
#[derive(Clone, Debug)]
pub struct IndexedEvent {
    pub idx: i64,
    pub event: SessionEvent,
    /// Phase 1c: this event is EPHEMERAL — a live-only token chunk that
    /// was never persisted to `session_events`, so `idx` is meaningless
    /// (set to 0). The SSE/gRPC merge passes it through unconditionally
    /// (it can't have been replayed) and frames it with NO `idx`, so it
    /// never advances a client's `Last-Event-ID` cursor. Always `false`
    /// for the durable, persisted events that carry a real `idx`.
    pub ephemeral: bool,
}

/// Per-session in-memory event broadcast.
///
/// Backed by `tokio::sync::broadcast`: every subscriber gets every
/// event, slow subscribers can lag (they get an explicit error and can
/// catch up via the persistent log once we add it). Capacity is small
/// — clients should consume eagerly or fall back to the persistent
/// `?since=N` query when that lands.
pub struct SessionEventBus {
    channels: DashMap<SessionId, broadcast::Sender<IndexedEvent>>,
    capacity: usize,
}

impl SessionEventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            channels: DashMap::new(),
            capacity,
        }
    }

    /// Subscribe to events for `session`. Lazily allocates a broadcast
    /// channel on first subscribe / first publish, whichever comes
    /// first. Subscribers from different threads each get an
    /// independent receiver.
    pub fn subscribe(&self, session: SessionId) -> broadcast::Receiver<IndexedEvent> {
        let entry = self
            .channels
            .entry(session)
            .or_insert_with(|| broadcast::channel(self.capacity).0);
        entry.subscribe()
    }

    /// Publish a pre-persisted event to all current subscribers.
    /// Returns `true` if at least one subscriber received it. Most
    /// callers should go through `AppState::emit` instead, which
    /// handles persistence and idx allocation in lockstep.
    pub fn publish(&self, session: SessionId, indexed: IndexedEvent) -> bool {
        if let Some(tx) = self.channels.get(&session) {
            tx.send(indexed).is_ok()
        } else {
            false
        }
    }

    /// Number of currently-known sessions with at least one historical
    /// publish or subscribe. Intended for diagnostics / `/healthz`.
    pub fn active_sessions(&self) -> usize {
        self.channels.len()
    }
}

impl Default for SessionEventBus {
    fn default() -> Self {
        // 256 events per channel: more than enough for normal interactive
        // exec, comfortable margin for chatty agents.
        Self::new(256)
    }
}

/// ADR 0066: a per-session, per-replica cap on concurrent live preview
/// (port-forward) connections. A preview page legitimately opens dozens of
/// connections (HTTP/1.1 without keep-alive + WebSockets); this bounds a
/// runaway or abusive one and fails fast at the coordinator — returning
/// `resource_exhausted` (→ a 503 at the orchestrator) before a would-be-capped
/// connection consumes host→guest fds. A backstop, not a hard global quota: it
/// counts only this replica's connections, and agentd holds a per-guest
/// backstop of its own.
pub struct PreviewConnLimiter {
    per_session: DashMap<SessionId, Arc<tokio::sync::Semaphore>>,
    cap: usize,
}

impl PreviewConnLimiter {
    pub fn new(cap: usize) -> Self {
        Self {
            per_session: DashMap::new(),
            cap,
        }
    }

    /// `ENGRAM_PREVIEW_MAX_CONNS_PER_SESSION` (default 256, matching the
    /// host-agent gRPC `concurrency_limit_per_connection`).
    pub fn from_env() -> Self {
        let cap = std::env::var("ENGRAM_PREVIEW_MAX_CONNS_PER_SESSION")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(256);
        Self::new(cap)
    }

    /// Claim a slot for `session`. `Some(permit)` holds it until dropped
    /// (released on every relay exit path); `None` means the session is at its
    /// cap. The map entry is pruned when a session's last permit drops, so the
    /// map only ever holds sessions with live previews.
    pub fn try_acquire(self: &Arc<Self>, session: SessionId) -> Option<PreviewPermit> {
        let sem = self
            .per_session
            .entry(session)
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.cap)))
            .value()
            .clone();
        let permit = sem.try_acquire_owned().ok()?;
        Some(PreviewPermit {
            limiter: Arc::clone(self),
            session,
            permit: Some(permit),
        })
    }
}

/// Held for the lifetime of one live preview connection; releases the slot on
/// drop and prunes the session's map entry when it was the last one.
pub struct PreviewPermit {
    limiter: Arc<PreviewConnLimiter>,
    session: SessionId,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Drop for PreviewPermit {
    fn drop(&mut self) {
        // Release the slot FIRST so `available_permits` reflects this drop,
        // THEN prune the entry iff no live previews remain for the session.
        // `remove_if` is atomic per key: a concurrent `try_acquire` either
        // keeps the entry (it holds a permit → predicate false) or re-creates a
        // fresh one after removal — never a lost or double-counted slot.
        drop(self.permit.take());
        self.limiter.per_session.remove_if(&self.session, |_, sem| {
            sem.available_permits() >= self.limiter.cap
        });
    }
}

pub struct AppState {
    pub cfg: CoordinatorConfig,
    pub services: Services,
    /// Shared with the `pg_listener` task so cross-replica events
    /// land on the same broadcast bus as locally-emitted ones. Cheap
    /// to clone (`Arc` clone), so handlers freely take a reference and
    /// the listener task takes its own.
    pub events: Arc<SessionEventBus>,
    /// Multi-host routing layer. In `--mode=all` this has exactly one
    /// entry registered at startup (the local backend, wrapped in
    /// `engram_host_agent::pooled_backend::PooledBackend` so the
    /// chunked-OCI / image-cache / egress wiring is consistent with
    /// production host-agents); `--mode=coordinator` fills it in as
    /// hosts dial `/api/hosts/connect`.
    pub host_registry: Arc<HostRegistry>,
    /// Phase 4: harness ↔ host vsock channel hub. Holds one
    /// connection per attached harness; routes inbound HarnessEvents
    /// into the configured EventSink (which forwards them into
    /// session_events). Constructed at AppState creation with a sink
    /// that captures clones of `services.meta` + `events` — the
    /// closure is intentionally short so AppState construction stays
    /// non-circular.
    ///
    /// `--mode=all` and the dev `--mode=coordinator` both populate
    /// this; multi-host production will additionally accept inbound
    /// harness connections off a real vsock listener wired through
    /// the same hub.
    pub harness_hub: Arc<HarnessHub>,
    /// Issue #535 (a): per-enabled-image manifest/snapshot/budget cache +
    /// the fleet bundle-catalog cache, invalidated by `pg_listener` on
    /// `enabled_image_changed` / `fleet_catalog_changed` NOTIFYs. Shared
    /// with the listener task the same way `host_registry`/`integrations`
    /// are (an `Arc` clone at spawn time).
    pub boot_bundles: Arc<crate::boot_bundle::BootBundleCache>,
    /// ADR 0073: wakes the outbox delivery driver on local enqueues.
    pub outbox_wake: Arc<tokio::sync::Notify>,
    /// ADR 0079: wakes the session-op executor on `pg_notify('session_ops', …)`.
    pub session_ops_wake: Arc<tokio::sync::Notify>,
    /// Bound address of the harness TCP listener (set by `lib::run`
    /// once the listener has accepted a port from the OS — `127.0.0.1:0`
    /// becomes e.g. `127.0.0.1:54123`). The session-create handler
    /// reads this to plumb `ENGRAM_HARNESS_ADDR` into the spawned
    /// agent's env. `None` until the listener is up.
    pub harness_listen_addr: parking_lot::Mutex<Option<std::net::SocketAddr>>,
    /// ADR 0009 reconciliation pass. Holds the per-coord strikes
    /// counter and the policy knob (`grace_ticks`). Invoked on
    /// every inbound `NotifyKind::Heartbeat` in `api/hosts.rs`.
    pub reconciler: crate::reconcile::Reconciler,
    /// ADR 0016 Phase A: per-host COW diagnostic cache. The
    /// `GET /api/{hosts,sessions}/:id/cow-state` handlers route
    /// through this to avoid storming the host on web-app polling.
    /// 1s TTL; cache eviction on host unregister.
    pub cow_state_cache: Arc<crate::cow_state::CowStateCache>,
    /// ADR 0023/0047: per-session credential-broker tokens (session →
    /// expected bearer). PURE READ-THROUGH CACHE over the KEK-sealed
    /// `session_broker_tokens` PG rows (the authority — minted
    /// first-writer-wins, so the token is stable for the session's
    /// lifetime on every replica). A miss loads + unseals from PG;
    /// cleared at terminal alongside the row.
    pub git_broker_tokens: Arc<dashmap::DashMap<SessionId, String>>,
    /// ADR 0066: per-session, per-replica cap on concurrent live preview
    /// (port-forward) connections. Kept here (not on `Services`) so the many
    /// test `Services` literals don't need touching.
    pub preview_conns: Arc<PreviewConnLimiter>,
    /// ADR 0056: the configured provider integrations (GitHub App, etc) —
    /// subsumes the old single `forge`. Set on `main`'s run path via
    /// `run_with_registry_and_local`; empty in tests and when `--git-forge`
    /// is unset (the forge endpoints then 501). Kept here rather than on
    /// `Services` so the many test `Services` literals don't need touching.
    pub integrations: crate::integrations::IntegrationBroker,
    /// ADR 0106: shared provider registry, sealed store, and active flow owner.
    pub oauth: Arc<crate::oauth::OAuthManager>,
    // ADR 0051: the per-user auth runtime (`auth: Option<Arc<AuthRuntime>>`)
    // is removed. The coordinator no longer resolves human principals — the
    // orchestrator owns auth/authz and calls the coordinator over the trusted
    // app-gRPC surface. Session attribution lives in the orchestrator's task
    // model, not a coordinator-side `sessions.user_id` column.
    /// ADR 0050 B: graceful-shutdown fanout. Flipped to `true` once
    /// `run`'s SIGTERM/ctrl-c handler fires, BEFORE axum starts draining
    /// connections. Long-lived response handlers (SSE `/events`,
    /// `/exec/stream`) subscribe and end their streams on the flip so
    /// they don't block hyper's graceful shutdown indefinitely
    /// (tokio-rs/axum#2673) — the client reconnects to a healthy replica
    /// and resumes from the PG-backed log via `Last-Event-ID`. The
    /// receiver-less `watch::Sender` is kept alive here; handlers call
    /// `subscribe_shutdown()` for a fresh receiver.
    pub shutdown_tx: tokio::sync::watch::Sender<bool>,
}

impl AppState {
    /// Convenience constructor for tests and `--mode=all`-flavoured
    /// embeddings: builds a fresh `HostRegistry` and pre-registers
    /// `services.host` as the sole host. Callers that want the
    /// chunked-OCI / image-cache / egress wiring in this single-host
    /// setup should pass a `LocalHostClient` wrapping a
    /// `PooledBackend`-flavoured `SandboxBackend`. Production
    /// `--mode=coordinator` should use [`AppState::new_with_registry`]
    /// to thread a registry that hosts dial into via WS.
    pub fn new(cfg: CoordinatorConfig, services: Services) -> Self {
        let registry = Arc::new(HostRegistry::new(services.meta.clone()));
        registry.register(HostId::new(), services.host.clone());
        Self::new_with_registry(cfg, services, registry)
    }

    /// Construct an AppState whose `services.host` already routes
    /// via the supplied `HostRegistry`.
    pub fn new_with_registry(
        cfg: CoordinatorConfig,
        services: Services,
        host_registry: Arc<HostRegistry>,
    ) -> Self {
        let events = Arc::new(SessionEventBus::default());
        // ADR 0073: the coordinator's own hub replays remote-host events
        // (emit_external) and serves mode=all in-process attaches. Its
        // binding records live under the coordinator's local state dir —
        // ephemeral per process is correct here: mode=all sandboxes
        // (Process backend) do not survive a coordinator restart, so
        // there are no survivor re-dials for a fresh store to validate.
        let bindings_dir =
            std::env::temp_dir().join(format!("engram-coord-bindings-{}", std::process::id()));
        let bindings = engram_host_agent::bindings::BindingStore::open(bindings_dir)
            .expect("open coordinator binding store");
        let harness_hub = Arc::new(HarnessHub::new(
            harness_event_sink(
                events.clone(),
                services.meta.clone(),
                services.clock.clone(),
            ),
            bindings,
        ));
        let reconciler =
            crate::reconcile::Reconciler::new(crate::reconcile::grace_ticks_from_env());
        let boot_bundles = Arc::new(crate::boot_bundle::BootBundleCache::new(
            services.clock.clone(),
        ));
        Self {
            oauth: crate::oauth::OAuthManager::new(
                services.meta.clone(),
                services.kek.clone(),
                services.clock.clone(),
                services.entropy.clone(),
            ),
            cfg,
            services,
            events,
            host_registry,
            harness_hub,
            boot_bundles,
            // ADR 0073: local fast-path wake for the outbox delivery
            // driver (the PG NOTIFY covers cross-pod).
            outbox_wake: Arc::new(tokio::sync::Notify::new()),
            session_ops_wake: Arc::new(tokio::sync::Notify::new()),
            harness_listen_addr: parking_lot::Mutex::new(None),
            reconciler,
            cow_state_cache: Arc::new(crate::cow_state::CowStateCache::new()),
            git_broker_tokens: Arc::new(dashmap::DashMap::new()),
            preview_conns: Arc::new(PreviewConnLimiter::from_env()),
            integrations: crate::integrations::IntegrationBroker::new(),
            shutdown_tx: tokio::sync::watch::channel(false).0,
        }
    }

    /// A fresh receiver on the graceful-shutdown signal (ADR 0050 B).
    /// Resolves `true` once shutdown begins (immediately if already
    /// shutting down). Long-lived handlers `take_until` it.
    pub fn subscribe_shutdown(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Signal graceful shutdown: wakes every [`subscribe_shutdown`]
    /// receiver so SSE/stream handlers end. Called by `run`'s signal
    /// handler before axum drains. Idempotent.
    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Where to write per-session snapshot directories on local disk.
    /// Per-host scratch under `cfg.local_path`; not durable across host
    /// loss. Cross-host durability for sessions is git, not snapshots.
    pub fn snapshot_dir(&self) -> std::path::PathBuf {
        self.cfg.local_path.join("snapshots")
    }

    /// Resolve the live sandbox bound to `session` from Postgres
    /// (`sessions.sandbox_id` — the single source of truth).
    ///
    /// ADR 0047: the coordinator holds NO in-memory session→sandbox
    /// authority. `session_boot` persists the binding via
    /// `create_session_created`; every rebind/resume path persists it via
    /// `assign_session_sandbox`; eviction/teardown clears it to `None`.
    /// Any replica answers `/exec` / `/prompt` / `/shell` / `/snapshot`
    /// identically by reading this row — one indexed PK select, sub-ms,
    /// dwarfed by the downstream host exec RPC. A `None` here means the
    /// session genuinely has no live sandbox (idle / terminal / pre-boot).
    pub async fn resolve_sandbox(&self, session: SessionId) -> Option<SandboxId> {
        self.services
            .meta
            .get_session(session)
            .await
            .ok()
            .and_then(|s| s.sandbox_id)
    }

    /// Register a local VMM backend as an in-process host. Wraps it
    /// in a `LocalHostClient` bound to this AppState's `HarnessHub`,
    /// so harness ops (bind/unbind/send_prompt) routed via
    /// `services.host` land on the same hub that
    /// `lib.rs::set_harness_sink` plumbs vsock dials into. Used by
    /// `--mode=all`'s startup wiring after AppState is built.
    pub fn register_local_host(
        &self,
        host_id: engram_core::HostId,
        sandbox: Arc<dyn engram_core::traits::SandboxBackend>,
    ) {
        let client: Arc<dyn engram_core::traits::HostClient> = Arc::new(
            engram_host_agent::LocalHostClient::new(sandbox, self.harness_hub.clone()),
        );
        self.host_registry.register(host_id, client);
    }

    /// Persist `event` to the session's event log, then publish it on
    /// the live bus. Returns the monotonic per-session `idx` assigned
    /// to it. The persistent log is the source of truth: in-memory
    /// subscribers see what was committed to Postgres, and reconnecting
    /// clients can use `?since=<idx>` (or EventSource's
    /// `Last-Event-ID`) to fill any gap before tailing live.
    pub async fn emit(
        &self,
        session: SessionId,
        event: SessionEvent,
    ) -> Result<i64, crate::error::ApiError> {
        let kind = event.kind();
        let payload = serde_json::to_value(&event)
            .map_err(|e| crate::error::ApiError::Internal(format!("event serialize: {e}")))?;
        let idx = self
            .services
            .meta
            .append_session_event(session, kind, payload)
            .await?;
        // ADR 0073: ack any outbox row a DIRECTLY-EMITTED confirming event
        // retires. NOTE: the confirming events (`run_started`/`prompt_queued`/
        // `tool_call_completed`) are HARNESS events, and those ingest via
        // `harness_event_sink` → `append_session_event`, NOT this `emit` — so
        // they ack THERE (see the ack in `harness_event_sink`). This arm is the
        // defensive catch for any confirming event authored/replayed straight
        // through `emit`; a confirming event terminally retires the matching
        // outbox row, and unknown / already-acked ids are no-ops (at-least-once).
        if let Some(ack_id) = outbox_ack_id(session, &event) {
            match self.services.meta.outbox_ack(&ack_id).await {
                Ok(true) => {
                    ::metrics::counter!(crate::metrics::OUTBOX_ACKED_TOTAL).increment(1);
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(session_id = %session, ack_id, error = %e, "outbox ack failed");
                }
            }
        }
        self.events.publish(
            session,
            IndexedEvent {
                idx,
                event,
                ephemeral: false,
            },
        );
        Ok(idx)
    }

    /// Persist an event and the durable command it announces in one metadata
    /// transaction, then publish the committed event on the live bus.
    pub async fn emit_with_outbox(
        &self,
        session: SessionId,
        event: SessionEvent,
        outbox: &engram_core::types::outbox::OutboxRow,
    ) -> Result<i64, crate::error::ApiError> {
        let kind = event.kind();
        let payload = serde_json::to_value(&event)
            .map_err(|e| crate::error::ApiError::Internal(format!("event serialize: {e}")))?;
        let idx = self
            .services
            .meta
            .append_session_event_and_outbox(session, kind, payload, outbox)
            .await?;
        self.events.publish(
            session,
            IndexedEvent {
                idx,
                event,
                ephemeral: false,
            },
        );
        Ok(idx)
    }

    /// ADR 0079 (review finding #6): emit a lifecycle event under an op's
    /// fence. Appends (and re-broadcasts) ONLY when
    /// `sessions.current_epoch == fence.epoch`; a fenced-out predecessor
    /// gets `Ok(None)` and its stale event never lands after the
    /// successor's. `fence.epoch == 0` (an out-of-op caller) falls back to
    /// the unfenced [`Self::emit`]. Use this for every StatusChanged /
    /// Evicted / SnapshotTaken authored from within an op pipeline.
    pub async fn emit_fenced(
        &self,
        session: SessionId,
        fence: engram_core::traits::SessionFence,
        event: SessionEvent,
    ) -> Result<Option<i64>, crate::error::ApiError> {
        if fence.epoch == 0 {
            return self.emit(session, event).await.map(Some);
        }
        let kind = event.kind();
        let payload = serde_json::to_value(&event)
            .map_err(|e| crate::error::ApiError::Internal(format!("event serialize: {e}")))?;
        let idx = match self
            .services
            .meta
            .append_session_event_fenced(session, fence.epoch as i64, kind, payload)
            .await?
        {
            Some(idx) => idx,
            None => {
                // Fenced: a successor owns the event-log tail now.
                crate::metrics::note_fenced_write();
                return Ok(None);
            }
        };
        if let Some(ack_id) = outbox_ack_id(session, &event) {
            match self.services.meta.outbox_ack(&ack_id).await {
                Ok(true) => {
                    ::metrics::counter!(crate::metrics::OUTBOX_ACKED_TOTAL).increment(1);
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(session_id = %session, ack_id, error = %e, "outbox ack failed");
                }
            }
        }
        self.events.publish(
            session,
            IndexedEvent {
                idx,
                event,
                ephemeral: false,
            },
        );
        Ok(Some(idx))
    }
}

/// Serialize lifecycle events to the `(kind, payload)` wire pairs the
/// atomic store methods (`fenced_transition_session_with_events`,
/// `settle_evicted_session_idle`) append in-transaction.
pub(crate) fn wire_events(
    events: &[SessionEvent],
) -> Result<Vec<(String, serde_json::Value)>, engram_core::MetaError> {
    events
        .iter()
        .map(|e| {
            serde_json::to_value(e)
                .map(|payload| (e.kind().to_string(), payload))
                .map_err(|e| engram_core::MetaError::Serialization(format!("event serialize: {e}")))
        })
        .collect()
}

/// ADR 0073: which outbox row (if any) does this event confirm?
/// - `run_started{prompt_id}` / `prompt_queued{prompt_id}` — the
///   harness took ownership of the prompt (running it or holding it in
///   its type-ahead queue; the queue survives via the replay the
///   harness itself does, and an edit/dequeue of a queued prompt keeps
///   its own confirmations).
/// - `tool_call_completed{tool_call_id}` — a generic tool result landed.
pub(crate) fn outbox_ack_id(session_id: SessionId, event: &SessionEvent) -> Option<String> {
    match event {
        SessionEvent::HarnessRunStarted {
            prompt_id: Some(pid),
            ..
        } => Some(pid.clone()),
        SessionEvent::HarnessPromptQueued { prompt_id, .. } => Some(prompt_id.clone()),
        SessionEvent::HarnessPromptSteered { prompt_id, .. } => Some(prompt_id.clone()),
        SessionEvent::HarnessToolCallCompleted { tool_call_id, .. } => Some(
            engram_core::types::outbox::tool_result_outbox_id(session_id, tool_call_id),
        ),
        _ => None,
    }
}

pub type SharedState = Arc<AppState>;

/// Replay a harness event that arrived from a remote host (via
/// `NotifyKind::HarnessEvent`) through the coord's local hub. The
/// hub's `EventSink` — built by `harness_event_sink` below — does
/// the session_events append + SSE publish, dedup, etc. From the
/// perspective of subscribers this is indistinguishable from a
/// mode=all event flowing through the in-proc EventSink.
///
/// `at` is the host's wall-clock at observation time, captured at
/// the source and round-tripped through the WS. We forward it for
/// future use (per-event timestamps on the persisted row); today the
/// sink's `SessionEvent::from_harness` stamps its own coordinator
/// clock read (ADR 0098 D1: the injected `Clock`, not `Utc::now()`)
/// because the persisted event row already has a `created_at`.
pub async fn emit_harness_event(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: engram_core::SandboxId,
    event: engram_harness_proto::HarnessEvent,
    _at: chrono::DateTime<chrono::Utc>,
) -> Result<(), crate::error::ApiError> {
    state
        .harness_hub
        .emit_external(session_id, sandbox_id, event)
        .await;
    Ok(())
}

/// Build the [`EventSink`] that forwards harness events into
/// `session_events` and triggers Track C.9 auto-checkpoints on
/// `Idle` / `RunCompleted` for Git sessions. Captures clones of
/// the bus + meta service + sandbox backend so the closure has no
/// cycles back into AppState.
fn harness_event_sink(
    events: Arc<SessionEventBus>,
    meta: Arc<dyn engram_core::traits::MetadataStore>,
    clock: Arc<dyn engram_core::traits::Clock>,
) -> EventSink {
    // Per-session cache of the most-recent forwarded event kind. Used
    // to drop a `harness_idle` or `harness_parked` that would land
    // back-to-back with the same kind: harnesses re-announce their
    // waiting state on reconnect, so an evict/resume cycle would
    // otherwise append a redundant marker to the log on every cycle.
    let last_kind: Arc<DashMap<SessionId, &'static str>> = Arc::new(DashMap::new());
    Arc::new(move |session_id, _sandbox_id, ev| {
        let events = events.clone();
        let meta = meta.clone();
        let clock = clock.clone();
        let last_kind = last_kind.clone();
        Box::new(Box::pin(async move {
            // Forward every harness event into session_events for live
            // SSE / Web UI / Slackbot timeline. ADR 0005 retired the
            // auto-checkpoint branch this used to trigger on Idle /
            // RunCompleted; durability moved to hot+cold snapshots,
            // not git checkpoints.
            let session_event = SessionEvent::from_harness(ev, clock.now_utc());
            let kind = session_event.kind();

            // Issue #527 Phase 1: a run-started with a client prompt_id is
            // the consuming end of the `prompt_received` receipt — captured
            // here (before `session_event` moves into the published
            // `IndexedEvent` below) so the post-append lookup below can join
            // it against the receipt row and record prompt→run-start
            // latency. `None` for the env-seeded initial prompt, which
            // never gets a receipt.
            let run_started_prompt_id = if let SessionEvent::HarnessRunStarted {
                prompt_id: Some(pid),
                ..
            } = &session_event
            {
                Some(pid.clone())
            } else {
                None
            };

            // ADR 0073 fix: harness events are the CONFIRMING events that retire
            // the durable outbox row (`run_started{prompt_id}` /
            // `prompt_queued{prompt_id}` / `tool_call_completed{tool_call_id}`),
            // but they ingest through THIS sink — NOT `AppState::emit`, where the
            // ack lived — so the ack never fired. An un-acked row is redelivered
            // forever: the delivery driver re-resumes the session and re-runs the
            // prompt on every idle cycle (acked_at NULL, attempts climbing;
            // phantom re-runs + resume/evict churn + a duplicate-turn transcript
            // the web can't render). Capture the ack id here (before
            // `session_event` moves into the published frame) and retire the row
            // after the append. Unknown / already-acked ids are no-ops.
            let ack_id = outbox_ack_id(session_id, &session_event);

            // Session titles: a harness-suggested title is materialized onto
            // `sessions.suggested_title` (in real time, at ingestion) so the
            // orchestrator can read it as the session's display title without
            // walking the log. Captured before `session_event` moves into the
            // published frame; the write happens after `publish` (below) so it
            // never sits in front of the live SSE frame.
            let suggested_title =
                if let SessionEvent::HarnessTitleSuggested { title, .. } = &session_event {
                    Some(title.clone())
                } else {
                    None
                };

            // Drop a back-to-back duplicate waiting-state marker. The
            // upstream TTL bookkeeping in HarnessHub::reader_loop
            // already saw the event, so suppressing it here only
            // affects the persisted log + SSE bus.
            if matches!(kind, "harness_idle" | "harness_parked")
                && last_kind
                    .get(&session_id)
                    .map(|v| *v == kind)
                    .unwrap_or(false)
            {
                return;
            }

            let mut payload = match serde_json::to_value(&session_event) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "harness event serialize failed");
                    return;
                }
            };
            // Postgres `jsonb` cannot represent U+0000 anywhere in a
            // string (SQLSTATE 22P05), and this payload carries raw guest
            // tool output — an `xxd` of a binary file puts literal NULs
            // into `result_summary`, the insert fails, and the event
            // vanishes from the session log (incident 2026-07-10: exactly
            // the two hexdumps of the corrupted files were the events
            // that dropped). Replace NUL with U+FFFD before PG sees it,
            // and rebuild the in-memory event from the sanitized payload
            // so the SSE surface serves the same bytes as the durable log.
            let mut session_event = session_event;
            if strip_jsonb_nul(&mut payload) {
                tracing::debug!(
                    session_id = %session_id,
                    kind,
                    "harness event contained U+0000; sanitized for jsonb",
                );
                match serde_json::from_value::<SessionEvent>(payload.clone()) {
                    Ok(ev) => session_event = ev,
                    // Persisted payload is the authority; a rebuild
                    // failure only leaves the live SSE copy with the
                    // original NULs (legal JSON), never drops the event.
                    Err(e) => {
                        tracing::warn!(error = %e, "sanitized event rebuild failed; SSE keeps original")
                    }
                }
            }

            // Phase 1c: token chunks are EPHEMERAL — they are NEVER appended
            // to `session_events`. Fan them out cross-replica on the dedicated
            // `session_event_deltas` NOTIFY channel; every replica's
            // pg_listener re-broadcasts to its local bus, so a client on a
            // replica WITHOUT the harness connection still streams. The
            // producing replica also LISTENs that channel, so the NOTIFY echo
            // is the single delivery path — we do NOT publish locally here
            // (that would double-emit to this replica's own subscribers).
            // Best-effort: a dropped NOTIFY costs only animation, never
            // correctness — the durable `agent_message` (same message_id) is
            // the authoritative record and supersedes every chunk.
            if kind == "agent_message_chunk" {
                if let Err(e) = meta.notify_session_delta(session_id, &payload).await {
                    tracing::debug!(
                        session_id = %session_id,
                        error = %e,
                        "notify_session_delta failed; dropping ephemeral chunk",
                    );
                }
                return;
            }

            match meta.append_session_event(session_id, kind, payload).await {
                Ok(idx) => {
                    last_kind.insert(session_id, kind);

                    // PR #556 review finding #2: publish FIRST. This is the
                    // live SSE frame the ADR-0052 held user-echo waits on to
                    // un-hold and render — the metric join below is a
                    // synchronous PG round-trip that must never sit in front
                    // of it (worst case: the query's full timeout delays
                    // every run_started delivery, precisely when a
                    // contended Postgres makes that delay most costly).
                    events.publish(
                        session_id,
                        IndexedEvent {
                            idx,
                            event: session_event,
                            ephemeral: false,
                        },
                    );

                    // ADR 0073 fix: retire the durable outbox row this harness
                    // event confirms. Placed AFTER `publish` so the ack's PG
                    // write never sits in front of the live SSE frame (same
                    // rationale as the metric join below). Without this, the
                    // delivery driver never learns the prompt was consumed and
                    // redelivers it on every resume forever.
                    if let Some(ack_id) = &ack_id {
                        match meta.outbox_ack(ack_id).await {
                            Ok(true) => {
                                metrics::counter!(crate::metrics::OUTBOX_ACKED_TOTAL).increment(1);
                            }
                            Ok(false) => {}
                            Err(e) => tracing::warn!(
                                session_id = %session_id,
                                ack_id = %ack_id,
                                error = %e,
                                "outbox ack from harness event failed",
                            ),
                        }
                    }

                    // Session titles: materialize the latest harness-suggested
                    // title onto the session row (idempotent). Best-effort — a
                    // failure only means the display title lags the log; the
                    // event itself is already durably appended above.
                    if let Some(title) = &suggested_title {
                        if let Err(e) = meta.set_session_suggested_title(session_id, title).await {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %e,
                                "set_session_suggested_title failed",
                            );
                        }
                    }

                    // Issue #527 Phase 1: join this run-start against its
                    // `prompt_received` receipt (one PG lookup per run-start —
                    // runs are low-rate, acceptable per-event cost) and
                    // record the true prompt→run-start latency. Skip
                    // silently when there's no receipt (env-seeded initial
                    // prompt) rather than treating it as an error.
                    //
                    // PR #556 review finding #1: the elapsed seconds come
                    // back already computed PG-side (`NOW() - created_at`,
                    // one clock) — no coordinator-process `Utc::now()` is
                    // mixed in, so there's no coordinator/Postgres (or
                    // cross-replica) clock skew to bias or drop samples.
                    if let Some(pid) = &run_started_prompt_id {
                        match meta.prompt_received_seconds_ago(session_id, pid).await {
                            Ok(Some(secs)) if secs >= 0.0 => {
                                metrics::histogram!(crate::metrics::PROMPT_TO_RUN_STARTED_SECONDS)
                                    .record(secs);
                            }
                            Ok(Some(secs)) => {
                                // PG-side computation makes this all but
                                // unreachable in practice (would require
                                // Postgres's own clock to step backward
                                // between the two reads in one query) — kept
                                // as a defensive guard, not a routine branch.
                                tracing::warn!(
                                    session_id = %session_id,
                                    prompt_id = %pid,
                                    secs,
                                    "prompt_received_seconds_ago went negative; \
                                     skipping implausible sample",
                                );
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!(
                                session_id = %session_id,
                                prompt_id = %pid,
                                error = %e,
                                "prompt_received_seconds_ago lookup failed",
                            ),
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "append_session_event for harness event failed",
                    );
                }
            }
        }))
    })
}

/// Replace every U+0000 in the JSON tree's strings (values AND object
/// keys) with U+FFFD, returning whether anything changed. Postgres
/// `jsonb` rejects NUL outright (SQLSTATE 22P05) — see the caller in
/// `harness_event_sink`; guest tool output is the only producer of NULs
/// in practice, but the sweep is total so no future field regresses.
fn strip_jsonb_nul(v: &mut serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::String(s) if s.contains('\0') => {
            *s = s.replace('\0', "\u{FFFD}");
            true
        }
        Value::String(_) => false,
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= strip_jsonb_nul(item);
            }
            changed
        }
        Value::Object(map) => {
            let mut changed = false;
            for (_, val) in map.iter_mut() {
                changed |= strip_jsonb_nul(val);
            }
            if map.keys().any(|k| k.contains('\0')) {
                let entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
                for (k, val) in entries {
                    map.insert(k.replace('\0', "\u{FFFD}"), val);
                }
                changed = true;
            }
            changed
        }
        _ => false,
    }
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
pub(crate) mod tests {
    use super::*;

    /// Incident 2026-07-10: PG jsonb rejects U+0000, so a
    /// `tool_call_completed` whose `result_summary` carried raw binary
    /// (an `xxd` of a corrupted file) failed the insert and silently
    /// vanished from the session log. The sink now sanitizes first.
    #[test]
    fn strip_jsonb_nul_replaces_nul_everywhere_and_reports_change() {
        let mut v = serde_json::json!({
            "result_summary": "before\u{0000}after",
            "nested": { "arr": ["ok", "x\u{0000}y"], "clean": "fine" },
        });
        assert!(strip_jsonb_nul(&mut v));
        assert_eq!(v["result_summary"], "before\u{FFFD}after");
        assert_eq!(v["nested"]["arr"][1], "x\u{FFFD}y");
        assert_eq!(v["nested"]["clean"], "fine");

        // Untouched payloads report no change (the sink skips the
        // event rebuild on this path).
        let mut clean = serde_json::json!({"a": ["b"], "n": 3});
        assert!(!strip_jsonb_nul(&mut clean));

        // NUL in an object KEY is also scrubbed.
        let mut keyed = serde_json::Value::Object(
            [("k\u{0000}ey".to_string(), serde_json::json!("v"))]
                .into_iter()
                .collect(),
        );
        assert!(strip_jsonb_nul(&mut keyed));
        assert!(keyed.get("k\u{FFFD}ey").is_some());
    }

    fn evicted() -> SessionEvent {
        SessionEvent::Evicted {
            at: chrono::Utc::now(),
        }
    }

    /// ADR 0066: the preview-connection limiter caps per session, keeps
    /// sessions independent, frees on release, and prunes idle entries.
    #[test]
    fn preview_limiter_caps_per_session_and_prunes() {
        let lim = Arc::new(PreviewConnLimiter::new(2));
        let s = SessionId::new();

        let p1 = lim.try_acquire(s).expect("1st slot");
        let p2 = lim.try_acquire(s).expect("2nd slot");
        assert!(lim.try_acquire(s).is_none(), "3rd exceeds the cap of 2");

        // A different session has its own independent cap.
        assert!(
            lim.try_acquire(SessionId::new()).is_some(),
            "other session is independent",
        );

        // Releasing a slot frees capacity for the same session.
        drop(p1);
        let p3 = lim.try_acquire(s).expect("slot freed after release");

        // Dropping a session's last permit prunes its map entry (no leak).
        drop(p2);
        drop(p3);
        assert!(
            !lim.per_session.contains_key(&s),
            "idle session entry should be pruned",
        );
    }

    #[test]
    fn run_interrupted_maps_from_harness_with_stable_kind() {
        // ADR 0030: the harness RunInterrupted event maps to the coord
        // SessionEvent and serialises under the stable `run_interrupted`
        // kind that the SSE stream + web transcript key on.
        let ev = SessionEvent::from_harness(
            HarnessEvent::RunInterrupted {
                run_id: "r1".into(),
            },
            chrono::Utc::now(),
        );
        match &ev {
            SessionEvent::HarnessRunInterrupted { run_id, .. } => assert_eq!(run_id, "r1"),
            other => panic!("expected HarnessRunInterrupted, got {other:?}"),
        }
        assert_eq!(ev.kind(), "run_interrupted");
    }

    #[test]
    fn parked_maps_from_harness_with_stable_kind() {
        let ev = SessionEvent::from_harness(HarnessEvent::Parked, chrono::Utc::now());
        assert!(matches!(ev, SessionEvent::HarnessParked { .. }));
        assert_eq!(ev.kind(), "harness_parked");
    }

    #[test]
    fn prompt_steered_maps_and_acks_by_prompt_id() {
        let session_id = SessionId::new();
        let ev = SessionEvent::from_harness(
            HarnessEvent::PromptSteered {
                prompt_id: "p-steer".into(),
            },
            chrono::Utc::now(),
        );
        assert_eq!(ev.kind(), "prompt_steered");
        assert_eq!(outbox_ack_id(session_id, &ev).as_deref(), Some("p-steer"));
    }

    #[test]
    fn title_suggested_maps_from_harness_with_stable_kind_and_round_trips() {
        // Session titles: the harness TitleSuggested event maps to the coord
        // SessionEvent under the stable `title_suggested` kind and round-trips
        // through serde (the JSON form the log + orchestrator ingest see).
        let ev = SessionEvent::from_harness(
            HarnessEvent::TitleSuggested {
                title: "Fix the flaky test".into(),
            },
            chrono::Utc::now(),
        );
        match &ev {
            SessionEvent::HarnessTitleSuggested { title, .. } => {
                assert_eq!(title, "Fix the flaky test")
            }
            other => panic!("expected HarnessTitleSuggested, got {other:?}"),
        }
        // The `kind` column (what SSE + the orchestrator ingest key on) is the
        // stable short name; the payload's serde `type` tag follows the
        // enum-variant convention (`harness_*`), like every other harness event.
        assert_eq!(ev.kind(), "title_suggested");

        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "harness_title_suggested");
        assert_eq!(json["title"], "Fix the flaky test");
    }

    /// Issue #527 Phase 1: `PromptReceived` is coordinator-native (never
    /// constructed via `from_harness`), serialises under the stable
    /// `prompt_received` kind the tombstone-exclusion query in
    /// `engram-postgres` and the `MetadataStore::prompt_received_seconds_ago`
    /// lookup key on, and round-trips through serde untouched.
    #[test]
    fn prompt_received_has_stable_kind_and_round_trips() {
        let ev = SessionEvent::PromptReceived {
            prompt_id: "p-1".into(),
            at: chrono::Utc::now(),
        };
        assert_eq!(ev.kind(), "prompt_received");

        let json = serde_json::to_value(&ev).expect("serialize");
        assert_eq!(json["type"], "prompt_received");
        assert_eq!(json["prompt_id"], "p-1");

        let back: SessionEvent = serde_json::from_value(json).expect("deserialize");
        match back {
            SessionEvent::PromptReceived { prompt_id, .. } => assert_eq!(prompt_id, "p-1"),
            other => panic!("expected PromptReceived, got {other:?}"),
        }
    }

    #[test]
    fn generic_tool_events_have_stable_kinds_and_opaque_payloads() {
        let args_json = r#" { "text": [1, true, null] } "#;
        let requested = SessionEvent::from_harness(
            HarnessEvent::ToolCallRequested {
                run_id: "r1".into(),
                call_id: "call_1".into(),
                name: "save_memory".into(),
                args_json: args_json.into(),
            },
            chrono::Utc::now(),
        );
        match &requested {
            SessionEvent::HarnessToolCallRequested {
                run_id,
                tool_call_id,
                name,
                args_json: mapped_args,
                ..
            } => {
                assert_eq!(run_id, "r1");
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(name, "save_memory");
                assert_eq!(mapped_args, args_json);
            }
            other => panic!("expected HarnessToolCallRequested, got {other:?}"),
        }
        assert_eq!(requested.kind(), "tool_call_requested");

        let result_json = r#" { "saved": true } "#;
        let submitted = SessionEvent::ToolResultSubmitted {
            tool_call_id: "call_1".into(),
            result_json: result_json.into(),
            at: chrono::Utc::now(),
        };
        assert_eq!(submitted.kind(), "tool_result_submitted");
        let json = serde_json::to_value(&submitted).expect("serialize submitted event");
        assert_eq!(json["type"], "tool_result_submitted");
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["result_json"], result_json);
    }

    #[test]
    fn tool_call_completed_acks_tool_result_outbox_row() {
        let session_id = SessionId::new();
        let ev = SessionEvent::from_harness(
            HarnessEvent::ToolCallCompleted {
                run_id: "r1".into(),
                tool_call_id: "call_1".into(),
                tool_name: "save_memory".into(),
                ok: true,
                duration_ms: 1,
                result_summary: None,
            },
            chrono::Utc::now(),
        );
        let expected = format!("tool_result:{session_id}:call_1");
        assert_eq!(
            outbox_ack_id(session_id, &ev).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn browser_activity_maps_from_harness_with_correlation() {
        let ev = SessionEvent::from_harness(
            HarnessEvent::BrowserActivity {
                run_id: "r1".into(),
                tool_call_id: "tool-browser".into(),
                intent: "Clicking Sign in".into(),
            },
            chrono::Utc::now(),
        );
        assert_eq!(ev.kind(), "browser_activity");
        assert!(matches!(
            ev,
            SessionEvent::HarnessBrowserActivity {
                run_id,
                tool_call_id,
                intent,
                ..
            } if run_id == "r1"
                && tool_call_id == "tool-browser"
                && intent == "Clicking Sign in"
        ));
    }

    #[test]
    fn file_changed_maps_from_harness_with_stable_kind() {
        // ADR 0054 Flavor A: the harness FileChanged event maps to the coord
        // SessionEvent under the stable `file_changed` kind the web keys on,
        // carrying the path + change through opaquely.
        let ev = SessionEvent::from_harness(
            HarnessEvent::FileChanged {
                run_id: "r1".into(),
                tool_call_id: "toolu_e".into(),
                path: "src/main.rs".into(),
                change: FileChange::Edit {
                    hunks: vec![engram_harness_proto::EditHunk {
                        old: "a".into(),
                        new: "b".into(),
                    }],
                },
            },
            chrono::Utc::now(),
        );
        match &ev {
            SessionEvent::HarnessFileChanged {
                tool_call_id,
                path,
                change,
                ..
            } => {
                assert_eq!(tool_call_id, "toolu_e");
                assert_eq!(path, "src/main.rs");
                assert!(matches!(change, FileChange::Edit { .. }));
            }
            other => panic!("expected HarnessFileChanged, got {other:?}"),
        }
        assert_eq!(ev.kind(), "file_changed");
        // Externally-tagged FileChange serializes as `{ "edit": { "hunks": … }}`
        // — the snake_case key the web discriminates on (and bincode-safe).
        let json = serde_json::to_value(&ev).unwrap();
        assert!(json["change"]["edit"]["hunks"].is_array());
    }

    #[test]
    fn recovery_cause_is_on_the_wire_and_defaults_for_legacy_rows() {
        // ADR 0045 F1: the recovery boundary carries a machine-readable
        // `cause` the web switches on. A planned operator relocation must
        // serialize as `planned_relocation` so the card stops crying
        // "host failure"; an event persisted before the field existed
        // (no `cause` key) must read back as `HostFailureRecovery` — the
        // card's historical meaning — so legacy transcripts are unchanged.
        let planned = SessionEvent::RecoveredFromCheckpoint {
            recovery_epoch: 1,
            through_idx: 7,
            rolled_back: 4,
            surviving_side_effects: vec![],
            cause: RecoveryCause::PlannedRelocation,
            at: chrono::Utc::now(),
        };
        let v = serde_json::to_value(&planned).expect("serialize");
        assert_eq!(v["type"], "recovered_from_checkpoint");
        assert_eq!(v["cause"], "planned_relocation");

        // A pre-F1 persisted payload omits `cause` entirely.
        let legacy = serde_json::json!({
            "type": "recovered_from_checkpoint",
            "recovery_epoch": 1,
            "through_idx": 7,
            "rolled_back": 4,
            "surviving_side_effects": [],
            "at": chrono::Utc::now(),
        });
        match serde_json::from_value::<SessionEvent>(legacy).expect("deserialize legacy") {
            SessionEvent::RecoveredFromCheckpoint { cause, .. } => {
                assert_eq!(cause, RecoveryCause::HostFailureRecovery);
            }
            other => panic!("expected RecoveredFromCheckpoint, got {other:?}"),
        }
    }

    fn indexed(idx: i64, event: SessionEvent) -> IndexedEvent {
        IndexedEvent {
            idx,
            event,
            ephemeral: false,
        }
    }

    #[tokio::test]
    async fn bus_publishes_to_current_subscribers_only() {
        // Publishes before any subscriber are dropped on the floor.
        // After subscribe, future events are delivered. This matches
        // tokio::sync::broadcast semantics — and is why the persistent
        // event log + ?since=N replay matter for late-join.
        let bus = SessionEventBus::new(8);
        let sid = SessionId::new();

        // No subscribers yet — publish silently fails.
        assert!(!bus.publish(sid, indexed(0, evicted())));

        let mut rx = bus.subscribe(sid);
        assert!(bus.publish(sid, indexed(1, evicted())));
        let recv = rx
            .recv()
            .await
            .expect("subscriber receives published event");
        assert_eq!(recv.idx, 1);
        assert!(matches!(recv.event, SessionEvent::Evicted { .. }));
    }

    #[tokio::test]
    async fn bus_fans_out_to_multiple_subscribers() {
        let bus = SessionEventBus::new(8);
        let sid = SessionId::new();
        let mut a = bus.subscribe(sid);
        let mut b = bus.subscribe(sid);

        let started = SessionEvent::ExecStarted {
            exec_id: "x".into(),
            command: vec!["echo".into(), "hi".into()],
            at: chrono::Utc::now(),
        };
        bus.publish(sid, indexed(42, started));

        for rx in [&mut a, &mut b] {
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.idx, 42);
            match msg.event {
                SessionEvent::ExecStarted { exec_id, .. } => assert_eq!(exec_id, "x"),
                other => panic!("expected ExecStarted, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn bus_session_isolation() {
        let bus = SessionEventBus::new(8);
        let sa = SessionId::new();
        let sb = SessionId::new();
        let mut rx_a = bus.subscribe(sa);
        let mut rx_b = bus.subscribe(sb);

        bus.publish(sa, indexed(0, evicted()));

        // rx_a sees the event, rx_b sees nothing within a timeout.
        let _ = rx_a.recv().await.unwrap();
        let nothing = tokio::time::timeout(std::time::Duration::from_millis(50), rx_b.recv()).await;
        assert!(
            nothing.is_err(),
            "subscribers to other sessions must not receive events"
        );
    }

    // ---------------------------------------------------------------
    // Harness-event-sink test infrastructure: trait-based
    // MetadataStore mock used by both this module's dedup test and
    // the sessions_inspect / api tests below.
    // ---------------------------------------------------------------
    use async_trait::async_trait;
    use engram_core::traits::MetadataStore;
    use engram_core::types::session::SessionMode;
    use engram_core::types::{
        HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SnapshotRecord,
    };
    use engram_core::{HostId, MetaError};
    use parking_lot::Mutex as PlMutex;

    /// In-memory MetadataStore for state-level + idle-evictor tests.
    /// Tracks one session's status/host_id/sandbox_id mutably plus a
    /// snapshot list; everything outside that surface is a benign
    /// no-op rather than unreachable so a test that exercises a
    /// secondary path doesn't panic.
    pub(crate) struct MiniMeta {
        pub(crate) session: PlMutex<Session>,
        pub(crate) events: PlMutex<Vec<PersistedEvent>>,
        next_idx: PlMutex<i64>,
        pub(crate) snapshots: PlMutex<Vec<SnapshotRecord>>,
        /// ADR 0045 C1 tests: host rows for `list_active_hosts` (the
        /// live-migration verb resolves the source's host_addr here).
        pub(crate) hosts: PlMutex<Vec<HostRecord>>,
        /// ADR 0014 issue #1/#2 idle-evictor abort-on-failure tests:
        /// when true, the next `record_snapshot` call returns an error.
        /// Reset to false on use.
        pub(crate) fail_next_record_snapshot: PlMutex<bool>,
        /// ADR 0016 Phase B: in-memory mirror of
        /// `sessions.live_disk_manifest_*` for tests that exercise
        /// the `update_live_disk_manifest` trait. Keyed by
        /// `session_id` and gated by `sandbox_id` match against the
        /// `session.sandbox_id` field above — same correctness
        /// invariant as the PG `WHERE sandbox_id = $2` guard.
        pub(crate) live_disk_manifests: PlMutex<
            std::collections::HashMap<
                SessionId,
                (
                    engram_core::SandboxId,
                    engram_core::types::manifest::ManifestRef,
                ),
            >,
        >,
        /// ADR 0016 Phase C: in-memory mirror of the `chunk_generation`
        /// counter. Bumped in the same critical section that writes
        /// `live_disk_manifests` (or clears it via
        /// `assign_session_sandbox(None)`) so tests can verify the
        /// barrier behaviour Phase C will rely on.
        pub(crate) chunk_generation: PlMutex<u64>,
        /// ADR 0018 commit 12b: in-memory mirror of
        /// `sessions.evac_attempts`. Reset to 0 when the session
        /// transitions into Evacuating; bumped by
        /// `bump_evac_attempts`; observed by the scanner via
        /// `list_evacuating_sessions`.
        pub(crate) evac_attempts: PlMutex<std::collections::HashMap<SessionId, u32>>,
        /// ADR 0034: in-memory mirror of `sessions.evict_attempts`.
        /// Same lifecycle as `evac_attempts`, for the eviction
        /// scanner.
        pub(crate) evict_attempts: PlMutex<std::collections::HashMap<SessionId, u32>>,
        /// ADR 0047: in-memory mirror of `teleport_targets` (the
        /// migration pin honored by evac_resumer as a strict
        /// require_host). Issue #209 tests assert this is cleared on
        /// every verb exit path — the default no-op trait impl would make
        /// such an assertion vacuous, so the mock tracks it for real.
        /// Issue #214: tracks the set-at timestamp alongside the pin so
        /// the aged-pin scanner test can backdate one deterministically.
        pub(crate) teleport_targets: PlMutex<TeleportPinMap>,
        /// Issue #531/PR #564 (ADR 0068 persist-before-reconcile
        /// regression): when true, the NEXT `touch_host_heartbeat` call
        /// fails instead of persisting — tests use this to prove the
        /// heartbeat handler's early-return skips `reconcile_host`
        /// entirely on a persist failure, rather than just happening to
        /// flip nothing. Reset to false on use.
        pub(crate) fail_next_heartbeat_persist: PlMutex<bool>,
        /// Counts `list_resident_sandbox_assignments_on_host` calls — the
        /// entry point `Reconciler::reconcile_with_deps` hits on every
        /// tick it actually runs. A no-op default `apply_missing_sandbox_strikes`
        /// (this mock doesn't override it) would make "no flip happened"
        /// true whether or not reconcile ran at all, so tests assert on
        /// this call count instead to prove reconcile was actually
        /// skipped.
        pub(crate) reconcile_probe_calls: PlMutex<u32>,
        /// ADR 0073 ack-path regression: records every `outbox_ack` id so a
        /// test can prove a harness `run_started{prompt_id}` retires the
        /// durable outbox row (the bug: harness events bypassed the ack, so
        /// the row redelivered forever). The default trait `outbox_ack` is a
        /// no-op returning `Ok(false)`, which would make such an assertion
        /// vacuous — so the mock records for real.
        pub(crate) acked_outbox: PlMutex<Vec<String>>,
        /// ADR 0079: the in-memory op log (claims, epochs, fenced step
        /// writes) — the reference mock in `engram_core::types::session_op`,
        /// so the op-verb entry points (enqueue-and-observe, OpClaim, the
        /// executor's drive) are exercised for real against this mock.
        pub(crate) ops: engram_core::types::session_op::InMemoryOpLog,
        /// ADR 0079 pass 2: an honest in-memory `session_outbox` (rows +
        /// due/deliver/defer/ack semantics) so the DELIVER VERB's drain
        /// loop is exercisable in unit tests — the trait defaults drop
        /// rows on the floor, which would make any deliver-ordering
        /// assertion vacuous.
        pub(crate) outbox: PlMutex<Vec<engram_core::types::outbox::OutboxRow>>,
        /// ADR 0107: the session's persisted harness selection, so
        /// `send_prompt_core`'s `harness_mode` validation is exercisable
        /// (the trait default returns `None`, which rejects every mode).
        pub(crate) harness: PlMutex<Option<String>>,
    }

    /// Alias so `clippy::type_complexity` stays happy on MiniMeta's
    /// `teleport_targets` field. `session_id → (target_host, set_at?)` —
    /// `set_at` is `None` only for a pin staged before issue #214's
    /// migration (the scanner treats such a pin as not-aged).
    pub(crate) type TeleportPinMap = std::collections::HashMap<
        SessionId,
        (engram_core::HostId, Option<chrono::DateTime<chrono::Utc>>),
    >;

    impl MiniMeta {
        /// ADR 0047: placement reads host rows now — tests stage a
        /// schedulable (ready, fresh-heartbeat) host with this.
        pub(crate) fn add_ready_host(&self, id: engram_core::HostId) {
            self.hosts.lock().push(HostRecord {
                id,
                hostname: format!("test-{id}"),
                cloud_metadata: Default::default(),
                capacity: engram_core::types::HostCapacity {
                    total_gb: 0,
                    used_gb: 0,
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: None,
                ready_images: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0,
                stages_images: false,
                capabilities: engram_core::types::host::HostCapabilities::default(),
            });
        }

        pub(crate) fn new(session: Session) -> Self {
            Self {
                session: PlMutex::new(session),
                events: PlMutex::new(Vec::new()),
                next_idx: PlMutex::new(0),
                snapshots: PlMutex::new(Vec::new()),
                hosts: PlMutex::new(Vec::new()),
                fail_next_record_snapshot: PlMutex::new(false),
                live_disk_manifests: PlMutex::new(std::collections::HashMap::new()),
                chunk_generation: PlMutex::new(0),
                evac_attempts: PlMutex::new(std::collections::HashMap::new()),
                evict_attempts: PlMutex::new(std::collections::HashMap::new()),
                teleport_targets: PlMutex::new(std::collections::HashMap::new()),
                fail_next_heartbeat_persist: PlMutex::new(false),
                reconcile_probe_calls: PlMutex::new(0),
                acked_outbox: PlMutex::new(Vec::new()),
                ops: engram_core::types::session_op::InMemoryOpLog::default(),
                outbox: PlMutex::new(Vec::new()),
                harness: PlMutex::new(None),
            }
        }
    }

    /// Shared `AppState` fixture for tests that need a full `Services`
    /// wiring backed by [`MiniMeta`] — same-crate unit test modules
    /// (`api::snapshot`, `api::prompt`, …) share `pub(crate)` fns fine, so
    /// this retires what used to be a per-file ~60-line copy of the same
    /// wiring (finding #4, PR #556 review).
    pub(crate) fn build_state_for_session(
        session: Session,
    ) -> (
        crate::state::SharedState,
        std::sync::Arc<MiniMeta>,
        tempfile::TempDir,
    ) {
        use crate::config::CoordinatorConfig;
        use crate::host_registry::HostRegistry;
        use crate::state::AppState;
        use crate::Services;
        use engram_core::traits::SandboxBackend;
        use engram_secrets_dev::InMemorySecretStore;

        let local = tempfile::TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(
            engram_sandbox_process::ProcessBackend::new(local.path().join("sandboxes")),
        );
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(meta.clone() as Arc<dyn MetadataStore>));
        let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
            engram_host_agent::LocalHostClient::with_noop_hub(backend.clone()),
        );
        host_registry.register(engram_core::HostId::new(), local_host);
        let blobs_dir =
            std::env::temp_dir().join(format!("engram-blobs-test-{}", uuid::Uuid::new_v4()));
        let services = Services {
            meta: meta.clone(),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                blobs_dir.clone(),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(blobs_dir),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, meta, local)
    }

    /// Create a REAL sandbox in the state's process backend and bind it
    /// to the MiniMeta session. Nomination-window fixtures need this
    /// since the ascent's liveness gate (ADR 0101 C settle-window fix):
    /// an `Evicting` row whose bound sandbox does not exist in the
    /// backend is the post-capture settle window and correctly refuses
    /// to ascend — a fixture with a phantom `SandboxId::new()` models
    /// THAT, not the live nomination window.
    pub(crate) async fn bind_live_sandbox(
        state: &crate::state::SharedState,
        mini: &Arc<MiniMeta>,
    ) -> engram_core::SandboxId {
        use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
        let spec = SandboxSpec {
            image: "state-test".into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        };
        let sb = state.services.host.create(spec).await.expect("create");
        mini.session.lock().sandbox_id = Some(sb);
        sb
    }

    #[async_trait]
    impl MetadataStore for MiniMeta {
        /// ADR 0073: derive the idle-scan candidate from the mock's own
        /// session + event rows — the same "newest event" semantics the
        /// PG lateral implements, so idle_detector tests exercise real
        /// classification against realistic state.
        async fn list_idle_scan_candidates(
            &self,
            _soft_ttl_secs: i64,
            _hard_ttl_secs: i64,
        ) -> Result<Vec<engram_core::traits::metadata::IdleScanCandidate>, MetaError> {
            let session = self.session.lock().clone();
            if session.status != SessionState::Active || session.sandbox_id.is_none() {
                return Ok(Vec::new());
            }
            let events = self.events.lock();
            let last = events.last();
            Ok(vec![engram_core::traits::metadata::IdleScanCandidate {
                session_id: session.id,
                sandbox_id: session.sandbox_id,
                host_id: session.host_id,
                last_event_at: last.map(|e| e.created_at).unwrap_or(session.created_at),
                last_event_kind: last.map(|e| e.kind.clone()),
                shell_pinned_until: None,
            }])
        }

        async fn create_session(
            &self,
            _: SessionSpec,
        ) -> Result<engram_core::SessionId, MetaError> {
            unreachable!("create_session not used in state tests")
        }
        async fn transition_session_created(
            &self,
            _: engram_core::SessionId,
            _: engram_core::SandboxId,
        ) -> Result<(), MetaError> {
            unreachable!("transition_session_created not used in state tests")
        }
        async fn reserve_and_persist_create(
            &self,
            _: engram_core::traits::SessionCreateWriteSet,
            _: &[engram_core::HostId],
            _: usize,
        ) -> Result<engram_core::traits::CreateDisposition, MetaError> {
            unreachable!("reserve_and_persist_create not used in state tests")
        }
        async fn get_session(&self, id: engram_core::SessionId) -> Result<Session, MetaError> {
            let s = self.session.lock();
            if id == s.id {
                Ok(s.clone())
            } else {
                Err(MetaError::NotFound)
            }
        }
        async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
            Ok(vec![self.session.lock().clone()])
        }
        async fn transition_session(
            &self,
            id: engram_core::SessionId,
            target: engram_core::types::SessionState,
            _disposition: engram_core::types::BindingDisposition,
        ) -> Result<engram_core::types::SessionState, MetaError> {
            let mut s = self.session.lock();
            if id != s.id {
                return Err(MetaError::NotFound);
            }
            let prev = s.status;
            prev.try_transition_to(target)
                .map_err(|e| MetaError::Conflict(e.to_string()))?;
            s.status = target;
            // ADR 0018 commit 12b: entering Evacuating resets the
            // retry counter so a fresh drain starts the scanner's
            // budget clean. Mirrors the PG `CASE WHEN $2 =
            // 'evacuating' THEN 0` branch in
            // `engram_postgres::transition_session`.
            if matches!(target, engram_core::types::SessionState::Evacuating) {
                self.evac_attempts.lock().insert(id, 0);
            }
            // ADR 0034: same reset-on-entry for Evicting. Mirrors the
            // PG `CASE WHEN $2 = 'evicting' THEN 0` branch.
            if matches!(target, engram_core::types::SessionState::Evicting) {
                self.evict_attempts.lock().insert(id, 0);
            }
            Ok(prev)
        }
        // ADR 0079 (0078 re-review finding #4): mirror the PG semantics —
        // Idle → Queued gated on BOTH `status='idle'` and the fencing
        // epoch — so the resume verb's no-capacity queue arm is testable
        // against a stale fence (the default trait impl returns
        // `Ok(false)`, which would make the happy path vacuous).
        async fn enqueue_session_resume(
            &self,
            id: engram_core::SessionId,
            epoch: i64,
        ) -> Result<bool, MetaError> {
            if self.ops.current_epoch(id) != epoch {
                return Ok(false);
            }
            let mut s = self.session.lock();
            if id != s.id {
                return Err(MetaError::NotFound);
            }
            if s.status != SessionState::Idle {
                return Ok(false);
            }
            s.status = SessionState::Queued;
            Ok(true)
        }
        // ADR 0074 parking ladder: mirror the PG UPDATE into the
        // in-memory session so the reaper/ascent paths read the stamped
        // rung back (the default trait impl is a no-op, which would make
        // any parking assertion vacuous).
        async fn set_session_park_rung(
            &self,
            id: engram_core::SessionId,
            rung: i16,
            parked_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<(), MetaError> {
            let mut s = self.session.lock();
            if id != s.id {
                return Err(MetaError::NotFound);
            }
            s.park_rung = rung;
            s.parked_at = parked_at;
            Ok(())
        }
        async fn fenced_set_session_park_rung(
            &self,
            id: engram_core::SessionId,
            epoch: i64,
            rung: i16,
            parked_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<bool, MetaError> {
            if self.ops.current_epoch(id) != epoch {
                return Ok(false);
            }
            self.set_session_park_rung(id, rung, parked_at).await?;
            Ok(true)
        }
        async fn assign_session_host(
            &self,
            id: engram_core::SessionId,
            host_id: Option<HostId>,
        ) -> Result<(), MetaError> {
            let mut s = self.session.lock();
            if id != s.id {
                return Err(MetaError::NotFound);
            }
            s.host_id = host_id;
            Ok(())
        }
        async fn set_teleport_target(
            &self,
            id: engram_core::SessionId,
            target: Option<HostId>,
        ) -> Result<(), MetaError> {
            let mut t = self.teleport_targets.lock();
            match target {
                Some(h) => {
                    t.insert(id, (h, Some(chrono::Utc::now())));
                }
                None => {
                    t.remove(&id);
                }
            }
            Ok(())
        }
        async fn get_teleport_target(
            &self,
            id: engram_core::SessionId,
        ) -> Result<Option<(HostId, Option<chrono::DateTime<chrono::Utc>>)>, MetaError> {
            Ok(self.teleport_targets.lock().get(&id).copied())
        }
        async fn assign_session_sandbox(
            &self,
            id: engram_core::SessionId,
            sandbox_id: Option<engram_core::SandboxId>,
        ) -> Result<(), MetaError> {
            let mut s = self.session.lock();
            if id != s.id {
                return Err(MetaError::NotFound);
            }
            s.sandbox_id = sandbox_id;
            // ADR 0016 Phase B: unbind clears the live manifest +
            // bumps chunk_generation so Phase C's mid-sweep barrier
            // observes the pin-set shrink atomically. Mirrors the
            // PG path in engram-postgres::assign_session_sandbox.
            if sandbox_id.is_none() && self.live_disk_manifests.lock().remove(&id).is_some() {
                *self.chunk_generation.lock() += 1;
            }
            Ok(())
        }
        async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError> {
            let mut hosts = self.hosts.lock();
            if let Some(existing) = hosts.iter_mut().find(|h| h.id == host.id) {
                *existing = host;
            } else {
                hosts.push(host);
            }
            Ok(())
        }
        async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
            Ok(self.hosts.lock().clone())
        }
        async fn set_host_status(&self, _: HostId, _: HostStatus) -> Result<(), MetaError> {
            Ok(())
        }
        async fn touch_host_heartbeat(
            &self,
            id: HostId,
            hb: engram_core::types::host::HostHeartbeat,
        ) -> Result<(), MetaError> {
            {
                let mut fail = self.fail_next_heartbeat_persist.lock();
                if *fail {
                    *fail = false;
                    return Err(MetaError::Conflict(
                        "MiniMeta fail_next_heartbeat_persist: injected failure".into(),
                    ));
                }
            }
            let mut hosts = self.hosts.lock();
            if let Some(h) = hosts.iter_mut().find(|h| h.id == id) {
                h.status = hb.status;
                h.capacity = hb.capacity;
                h.utilization = hb.utilization;
                h.ready_images = hb.ready_images;
                h.current_bundles = hb.current_bundles;
                h.total_vcpus = hb.total_vcpus;
                h.last_heartbeat_at = chrono::Utc::now();
            }
            Ok(())
        }
        /// Issue #531: overrides the trait's default (which scans
        /// `list_active_sessions`) purely to count invocations — this
        /// is the entry point `Reconciler::reconcile_with_deps` hits on
        /// every tick it actually runs, so the persist-before-reconcile
        /// regression test asserts on this counter. Behavior otherwise
        /// matches the default: this mock only ever tracks one session.
        async fn list_resident_sandbox_assignments_on_host(
            &self,
            host_id: HostId,
        ) -> Result<
            Vec<(
                engram_core::SessionId,
                SandboxId,
                engram_core::types::SessionState,
            )>,
            MetaError,
        > {
            *self.reconcile_probe_calls.lock() += 1;
            let s = self.session.lock();
            Ok(match (s.status, s.host_id, s.sandbox_id) {
                (st, Some(h), Some(sb)) if h == host_id && st.reserves_host_memory() => {
                    vec![(s.id, sb, st)]
                }
                _ => Vec::new(),
            })
        }
        async fn set_host_cordoned(&self, id: HostId, cordoned: bool) -> Result<(), MetaError> {
            let mut hosts = self.hosts.lock();
            match hosts.iter_mut().find(|h| h.id == id) {
                Some(h) => {
                    h.cordoned = cordoned;
                    Ok(())
                }
                None => Err(MetaError::NotFound),
            }
        }
        async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_orphan_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<(engram_core::SessionId, engram_core::types::SessionState)>, MetaError>
        {
            Ok(Vec::new())
        }
        async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<bool, MetaError> {
            let mut should_fail = self.fail_next_record_snapshot.lock();
            if *should_fail {
                *should_fail = false;
                return Err(MetaError::Conflict(
                    "MiniMeta fail_next_record_snapshot: injected failure".into(),
                ));
            }
            drop(should_fail);
            let mut snapshots = self.snapshots.lock();
            let inserted = !snapshots.iter().any(|s| s.id == snap.id);
            snapshots.push(snap);
            Ok(inserted)
        }

        /// ADR 0101 C: mirrors the PG single-statement settle — status
        /// CAS + sandbox match + recoverable-row EXISTS, all-or-nothing
        /// (the D4-conformant twin lives in SimMetadataStore; this mock
        /// keeps the same observable semantics for coordinator tests).
        async fn settle_evicted_session_idle(
            &self,
            session_id: engram_core::SessionId,
            sandbox_id: engram_core::SandboxId,
            snapshot_id: engram_core::types::SnapshotId,
            events: &[(String, serde_json::Value)],
        ) -> Result<Option<Vec<i64>>, MetaError> {
            let row_ok =
                self.snapshots.lock().iter().any(|s| {
                    s.id == snapshot_id && s.session_id == Some(session_id) && s.recoverable
                });
            if !row_ok {
                return Ok(None);
            }
            {
                let mut session = self.session.lock();
                if session.id != session_id
                    || session.status != SessionState::Evicting
                    || session.sandbox_id != Some(sandbox_id)
                {
                    return Ok(None);
                }
                session.status = SessionState::Idle;
                session.sandbox_id = None;
            }
            let mut indices = Vec::with_capacity(events.len());
            for (kind, payload) in events {
                indices.push(
                    self.append_session_event(session_id, kind, payload.clone())
                        .await?,
                );
            }
            Ok(Some(indices))
        }
        async fn get_snapshot(
            &self,
            id: engram_core::types::SnapshotId,
        ) -> Result<Option<SnapshotRecord>, MetaError> {
            // Issue #529: the coordinator's row-watcher polls this to
            // detect a snapshot row landing via the host's heartbeat
            // reconcile (which, in this mock harness, is simulated by a
            // test calling `record_snapshot` directly).
            Ok(self.snapshots.lock().iter().find(|s| s.id == id).cloned())
        }
        async fn list_snapshots_for_session(
            &self,
            sid: engram_core::SessionId,
        ) -> Result<Vec<SnapshotRecord>, MetaError> {
            Ok(self
                .snapshots
                .lock()
                .iter()
                .filter(|s| s.session_id == Some(sid))
                .cloned()
                .collect())
        }
        async fn latest_snapshot_for_session(
            &self,
            sid: engram_core::SessionId,
        ) -> Result<Option<SnapshotRecord>, MetaError> {
            Ok(self
                .snapshots
                .lock()
                .iter()
                .rfind(|s| s.session_id == Some(sid))
                .cloned())
        }
        async fn append_session_event(
            &self,
            _session_id: engram_core::SessionId,
            kind: &str,
            payload: serde_json::Value,
        ) -> Result<i64, MetaError> {
            let mut next = self.next_idx.lock();
            let idx = *next;
            *next += 1;
            self.events.lock().push(PersistedEvent {
                idx,
                kind: kind.to_string(),
                payload,
                created_at: chrono::Utc::now(),
                recovery_epoch: 0,
                rewound_at: None,
            });
            Ok(idx)
        }
        async fn append_session_event_fenced(
            &self,
            session_id: engram_core::SessionId,
            epoch: i64,
            kind: &str,
            payload: serde_json::Value,
        ) -> Result<Option<i64>, MetaError> {
            // Honor the fence (review finding #6): a fenced-out predecessor
            // gets Ok(None), so its stale lifecycle event never lands.
            if self.ops.current_epoch(session_id) != epoch {
                return Ok(None);
            }
            self.append_session_event(session_id, kind, payload)
                .await
                .map(Some)
        }
        async fn outbox_ack(&self, prompt_id: &str) -> Result<bool, MetaError> {
            self.acked_outbox.lock().push(prompt_id.to_string());
            let mut rows = self.outbox.lock();
            match rows.iter_mut().find(|r| r.prompt_id == prompt_id) {
                Some(r) if r.acked_at.is_none() => {
                    r.acked_at = Some(chrono::Utc::now());
                    Ok(true)
                }
                Some(_) => Ok(false),
                // Rows never enqueued through the mock (event-driven
                // acks in tests that don't seed the outbox) still record
                // as "newly acked" — the pre-pass-2 behavior.
                None => Ok(true),
            }
        }
        async fn get_session_harness(
            &self,
            _session_id: SessionId,
        ) -> Result<Option<String>, MetaError> {
            Ok(self.harness.lock().clone())
        }
        async fn outbox_enqueue(
            &self,
            row: &engram_core::types::outbox::OutboxRow,
        ) -> Result<(), MetaError> {
            let mut rows = self.outbox.lock();
            // Idempotent on prompt_id (PG: INSERT … ON CONFLICT DO NOTHING).
            if rows.iter().any(|r| r.prompt_id == row.prompt_id) {
                return Ok(());
            }
            rows.push(row.clone());
            Ok(())
        }
        async fn outbox_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
            let now = chrono::Utc::now();
            let mut out: Vec<SessionId> = self
                .outbox
                .lock()
                .iter()
                .filter(|r| r.acked_at.is_none() && r.not_before <= now)
                .map(|r| r.session_id)
                .collect();
            out.dedup();
            Ok(out)
        }
        async fn outbox_next_due(
            &self,
            session_id: SessionId,
        ) -> Result<Option<engram_core::types::outbox::OutboxRow>, MetaError> {
            let now = chrono::Utc::now();
            Ok(self
                .outbox
                .lock()
                .iter()
                .filter(|r| {
                    r.session_id == session_id && r.acked_at.is_none() && r.not_before <= now
                })
                .min_by_key(|r| r.created_at)
                .cloned())
        }
        async fn outbox_mark_delivered(
            &self,
            prompt_id: &str,
            ack_timeout: std::time::Duration,
        ) -> Result<(), MetaError> {
            let mut rows = self.outbox.lock();
            if let Some(r) = rows.iter_mut().find(|r| r.prompt_id == prompt_id) {
                r.delivered_at = Some(chrono::Utc::now());
                r.attempts += 1;
                r.not_before = chrono::Utc::now()
                    + chrono::Duration::milliseconds(ack_timeout.as_millis() as i64);
            }
            Ok(())
        }
        async fn outbox_defer(
            &self,
            prompt_id: &str,
            delay: std::time::Duration,
        ) -> Result<(), MetaError> {
            let mut rows = self.outbox.lock();
            if let Some(r) = rows.iter_mut().find(|r| r.prompt_id == prompt_id) {
                r.attempts += 1;
                r.not_before =
                    chrono::Utc::now() + chrono::Duration::milliseconds(delay.as_millis() as i64);
            }
            Ok(())
        }
        async fn list_session_events_since(
            &self,
            _: engram_core::SessionId,
            since: i64,
            limit: i64,
        ) -> Result<Vec<PersistedEvent>, MetaError> {
            let limit = if limit < 0 { i64::MAX } else { limit };
            Ok(self
                .events
                .lock()
                .iter()
                .filter(|e| e.idx > since)
                .take(limit as usize)
                .cloned()
                .collect())
        }
        async fn insert_artifact(
            &self,
            _: uuid::Uuid,
            _: engram_core::SessionId,
            _: &str,
            _: &str,
            _: i64,
            _: Option<&str>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn get_artifact(
            &self,
            _: engram_core::SessionId,
            _: uuid::Uuid,
        ) -> Result<Option<engram_core::types::ArtifactRow>, MetaError> {
            Ok(None)
        }
        async fn artifact_usage(&self, _: engram_core::SessionId) -> Result<(i64, i64), MetaError> {
            Ok((0, 0))
        }
        async fn upsert_registry_credential(
            &self,
            _: engram_core::types::RegistryCredential,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_registry_credentials(
            &self,
        ) -> Result<Vec<engram_core::types::RegistryCredential>, MetaError> {
            Ok(Vec::new())
        }
        async fn registry_credential_for_host(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::RegistryCredential>, MetaError> {
            Ok(None)
        }
        async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
            Ok(())
        }
        // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
        async fn upsert_enabled_image(
            &self,
            _: engram_core::types::EnabledImage,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_enabled_images(
            &self,
        ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
            Ok(Vec::new())
        }
        async fn get_enabled_image(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn get_enabled_image_any(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn soft_delete_enabled_image(
            &self,
            _: &str,
        ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
            Ok(engram_core::traits::DisableEnabledImageOutcome::Disabled)
        }
        async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
            Ok(())
        }
        async fn get_session_secrets(
            &self,
            _: SessionId,
        ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
            Ok(None)
        }
        async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
            Ok(())
        }

        // ADR 0079: the op log — delegate to the reference in-memory
        // implementation so claim exclusion, epoch bumps, and fenced
        // writes are exercised end-to-end by the unit tests.
        async fn op_enqueue_and_claim(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
            payload: serde_json::Value,
            idempotency_key: Option<&str>,
            claimed_by: &str,
        ) -> Result<engram_core::types::session_op::EnqueueOutcome, MetaError> {
            Ok(self
                .ops
                .enqueue_and_claim(session_id, kind, payload, idempotency_key, claimed_by))
        }

        async fn op_enqueue_and_claim_exclusive(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
            payload: serde_json::Value,
            claimed_by: &str,
        ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
            Ok(self
                .ops
                .enqueue_and_claim_exclusive(session_id, kind, payload, claimed_by))
        }

        async fn op_claim_head(
            &self,
            session_id: SessionId,
            claimed_by: &str,
        ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
            Ok(self.ops.claim_head(session_id, claimed_by))
        }

        async fn op_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
            Ok(self.ops.due_sessions())
        }

        async fn op_record_step(
            &self,
            op_id: i64,
            epoch: i64,
            step: &str,
        ) -> Result<bool, MetaError> {
            Ok(self.ops.record_step(op_id, epoch, step))
        }

        async fn op_heartbeat(&self, op_id: i64, epoch: i64) -> Result<bool, MetaError> {
            Ok(self.ops.heartbeat(op_id, epoch))
        }

        async fn op_finish(
            &self,
            op_id: i64,
            epoch: i64,
            state: engram_core::types::session_op::OpState,
            error: Option<&str>,
        ) -> Result<bool, MetaError> {
            Ok(self.ops.finish(op_id, epoch, state, error))
        }

        async fn op_requeue_with_backoff(
            &self,
            op_id: i64,
            epoch: i64,
            backoff: std::time::Duration,
            error: &str,
        ) -> Result<bool, MetaError> {
            Ok(self.ops.requeue_with_backoff(op_id, epoch, backoff, error))
        }

        async fn op_cancel_queued(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
        ) -> Result<bool, MetaError> {
            Ok(self.ops.cancel_queued(session_id, kind))
        }

        async fn op_wake_queued_kind(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
        ) -> Result<u64, MetaError> {
            Ok(self.ops.wake_queued_kind(session_id, kind))
        }

        async fn op_cancel_by_id(&self, op_id: i64) -> Result<bool, MetaError> {
            Ok(self.ops.cancel_by_id(op_id))
        }

        async fn op_request_cancel_running(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
        ) -> Result<bool, MetaError> {
            Ok(self.ops.request_cancel_running(session_id, kind))
        }

        async fn op_cancel_requested(&self, op_id: i64) -> Result<bool, MetaError> {
            Ok(self.ops.cancel_requested(op_id))
        }

        async fn op_running_for(
            &self,
            session_id: SessionId,
        ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
            Ok(self.ops.running_for(session_id))
        }

        /// ADR 0101 C: newest mint for `(session, kind)`, any state, any
        /// key — mirrors PG's `ORDER BY id DESC LIMIT 1`.
        async fn op_latest_for_kind(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
        ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
            Ok(self
                .ops
                .all()
                .into_iter()
                .filter(|o| o.session_id == session_id && o.kind == kind)
                .max_by_key(|o| o.id))
        }

        async fn op_get(
            &self,
            op_id: i64,
        ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
            Ok(self.ops.get(op_id))
        }

        async fn op_pending_exists(
            &self,
            session_id: SessionId,
            kind: engram_core::types::session_op::OpKind,
        ) -> Result<bool, MetaError> {
            Ok(self.ops.pending_exists(session_id, kind))
        }

        async fn fenced_transition_session(
            &self,
            session_id: SessionId,
            epoch: i64,
            to: engram_core::types::SessionState,
            _disposition: engram_core::types::BindingDisposition,
        ) -> Result<Option<engram_core::types::SessionState>, MetaError> {
            if self.ops.current_epoch(session_id) != epoch {
                return Ok(None);
            }
            self.transition_session(session_id, to, _disposition)
                .await
                .map(Some)
        }

        async fn fenced_transition_session_with_events(
            &self,
            session_id: SessionId,
            epoch: i64,
            to: engram_core::types::SessionState,
            _disposition: engram_core::types::BindingDisposition,
            events: &[(String, serde_json::Value)],
        ) -> Result<Option<(engram_core::types::SessionState, Vec<i64>)>, MetaError> {
            // In-memory "transaction": the fence gates once, then the flip
            // and the appends run back to back under the test's
            // single-threaded driver — good enough for the unit tests'
            // ordering assertions (the real atomicity is conformance-tested
            // against SimMetadataStore + PostgresStore, ADR 0098 D4).
            if self.ops.current_epoch(session_id) != epoch {
                return Ok(None);
            }
            let prev = self
                .transition_session(session_id, to, _disposition)
                .await?;
            if matches!(_disposition, engram_core::types::BindingDisposition::Detach) {
                self.session.lock().sandbox_id = None;
            }
            let mut indices = Vec::with_capacity(events.len());
            for (kind, payload) in events {
                indices.push(
                    self.append_session_event(session_id, kind, payload.clone())
                        .await?,
                );
            }
            Ok(Some((prev, indices)))
        }

        async fn fenced_assign_sandbox(
            &self,
            session_id: SessionId,
            epoch: i64,
            sandbox_id: Option<engram_core::SandboxId>,
            host_id: Option<HostId>,
        ) -> Result<bool, MetaError> {
            if self.ops.current_epoch(session_id) != epoch {
                return Ok(false);
            }
            self.assign_session_sandbox(session_id, sandbox_id).await?;
            self.assign_session_host(session_id, host_id).await?;
            Ok(true)
        }

        // ADR 0016 Phase B: in-memory mirror of
        // `engram_postgres::update_live_disk_manifest`. The
        // sandbox_id guard mirrors PG's `WHERE sandbox_id = $2`; a
        // mismatch returns `DroppedStale` without touching the
        // generation counter.
        async fn update_live_disk_manifest(
            &self,
            session_id: SessionId,
            sandbox_id: engram_core::SandboxId,
            manifest_ref: engram_core::types::manifest::ManifestRef,
        ) -> Result<engram_core::traits::UpdateOutcome, MetaError> {
            // Match against the current session.sandbox_id under the
            // session lock. NULL → DroppedStale (no binding). Other
            // sandbox_id → DroppedStale (stale publish).
            let bound = self.session.lock().sandbox_id;
            if bound != Some(sandbox_id) {
                return Ok(engram_core::traits::UpdateOutcome::DroppedStale);
            }
            self.live_disk_manifests
                .lock()
                .insert(session_id, (sandbox_id, manifest_ref));
            *self.chunk_generation.lock() += 1;
            Ok(engram_core::traits::UpdateOutcome::Applied)
        }

        async fn chunk_generation(&self) -> Result<u64, MetaError> {
            Ok(*self.chunk_generation.lock())
        }

        // ADR 0018 commit 12b: scanner support. MiniMeta carries one
        // session, so the list-sweep is trivially "is it Evacuating?".
        async fn list_evacuating_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
            let s = self.session.lock().clone();
            if matches!(s.status, engram_core::types::SessionState::Evacuating) {
                let attempts = self.evac_attempts.lock().get(&s.id).copied().unwrap_or(0);
                Ok(vec![(s, attempts)])
            } else {
                Ok(Vec::new())
            }
        }

        async fn bump_evac_attempts(
            &self,
            session_id: engram_core::SessionId,
        ) -> Result<u32, MetaError> {
            let mut map = self.evac_attempts.lock();
            let entry = map.entry(session_id).or_insert(0);
            *entry += 1;
            Ok(*entry)
        }

        // ADR 0034: eviction-scanner support, mirroring the 12b evac
        // trio above. MiniMeta carries one session, so the list-sweep
        // is trivially "is it Evicting?".
        async fn list_evicting_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
            let s = self.session.lock().clone();
            if matches!(s.status, engram_core::types::SessionState::Evicting) {
                let attempts = self.evict_attempts.lock().get(&s.id).copied().unwrap_or(0);
                Ok(vec![(s, attempts)])
            } else {
                Ok(Vec::new())
            }
        }

        async fn list_parked_sessions(&self) -> Result<Vec<Session>, MetaError> {
            let s = self.session.lock().clone();
            if matches!(s.status, engram_core::types::SessionState::Parked) {
                Ok(vec![s])
            } else {
                Ok(Vec::new())
            }
        }

        async fn bump_evict_attempts(
            &self,
            session_id: engram_core::SessionId,
        ) -> Result<u32, MetaError> {
            let mut map = self.evict_attempts.lock();
            let entry = map.entry(session_id).or_insert(0);
            *entry += 1;
            Ok(*entry)
        }

        // ADR 0034 L3 backstop. MiniMeta's one session is "idle past
        // TTL" when its newest event (falling back to the session's
        // created_at — same COALESCE the PG query uses) is older than
        // the cutoff. Tests backdate by pushing a PersistedEvent with
        // an old `created_at` into `events`, or by rewinding
        // `session.created_at` directly (both fields are pub(crate)).
        async fn list_active_sessions_idle_past(
            &self,
            idle_for_secs: i64,
        ) -> Result<
            Vec<(
                engram_core::SessionId,
                engram_core::SandboxId,
                chrono::DateTime<chrono::Utc>,
            )>,
            MetaError,
        > {
            let s = self.session.lock().clone();
            if !matches!(s.status, engram_core::types::SessionState::Active) {
                return Ok(Vec::new());
            }
            let Some(sandbox_id) = s.sandbox_id else {
                return Ok(Vec::new());
            };
            let last_event_at = self
                .events
                .lock()
                .iter()
                .map(|e| e.created_at)
                .max()
                .unwrap_or(s.created_at);
            let cutoff = chrono::Utc::now() - chrono::Duration::seconds(idle_for_secs);
            if last_event_at < cutoff {
                Ok(vec![(s.id, sandbox_id, last_event_at)])
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn harness_event_sink_dedupes_back_to_back_idles_and_parked() {
        // The claude harness re-emits Idle on every reconnect (e.g.
        // after an evict/resume cycle on an already-idle session).
        // Persisting each one would litter the timeline with redundant
        // "awaiting prompt" markers; the sink drops the duplicates.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:idle-dedup".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let mini = Arc::new(MiniMeta::new(session));
        let meta: Arc<dyn MetadataStore> = mini.clone();

        let sandbox_id = engram_core::SandboxId::new();

        let bus = Arc::new(SessionEventBus::default());
        let sink = super::harness_event_sink(
            bus.clone(),
            meta.clone(),
            Arc::new(engram_core::traits::SystemClock::new()),
        );

        // Three back-to-back idles: only the first should land.
        for _ in 0..3 {
            sink(session_id, sandbox_id, HarnessEvent::Idle).await;
        }
        {
            let events = mini.events.lock();
            assert_eq!(events.len(), 1, "consecutive idles must collapse");
            assert_eq!(events[0].kind, "harness_idle");
        }

        // A non-idle event resets the dedup state — the next Idle is
        // a real transition and must persist.
        sink(
            session_id,
            sandbox_id,
            HarnessEvent::RunStarted {
                run_id: "run-1".into(),
                prompt_summary: None,
                prompt_id: None,
            },
        )
        .await;
        sink(session_id, sandbox_id, HarnessEvent::Idle).await;
        sink(session_id, sandbox_id, HarnessEvent::Idle).await;

        let kinds: Vec<String> = mini.events.lock().iter().map(|e| e.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                "harness_idle".to_string(),
                "run_started".to_string(),
                "harness_idle".to_string(),
            ],
        );

        // Parked is also re-announced after reconnect while the agent's
        // turn remains open. Consecutive markers collapse independently
        // from Idle, while an intervening event permits the next marker.
        for _ in 0..3 {
            sink(session_id, sandbox_id, HarnessEvent::Parked).await;
        }
        sink(
            session_id,
            sandbox_id,
            HarnessEvent::RunStarted {
                run_id: "run-2".into(),
                prompt_summary: None,
                prompt_id: None,
            },
        )
        .await;
        sink(session_id, sandbox_id, HarnessEvent::Parked).await;
        sink(session_id, sandbox_id, HarnessEvent::Parked).await;

        let kinds: Vec<String> = mini.events.lock().iter().map(|e| e.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                "harness_idle",
                "run_started",
                "harness_idle",
                "harness_parked",
                "run_started",
                "harness_parked",
            ],
        );
    }

    /// Issue #527 Phase 1: a `run_started{prompt_id}` whose matching
    /// `prompt_received` receipt row doesn't exist (`MiniMeta`'s default
    /// `prompt_received_seconds_ago` — see `MetadataStore`'s default impl —
    /// returns `Ok(None)`, mirroring the env-seeded initial prompt, which
    /// never gets a receipt) must not panic and must still append the
    /// `run_started` event normally. The `engram_prompt_to_run_started_seconds`
    /// join is best-effort telemetry, never load-bearing for delivery.
    #[tokio::test]
    async fn harness_event_sink_skips_metric_when_no_receipt_row_exists() {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:no-receipt".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let mini = Arc::new(MiniMeta::new(session));
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let sandbox_id = engram_core::SandboxId::new();

        let bus = Arc::new(SessionEventBus::default());
        let sink =
            super::harness_event_sink(bus, meta, Arc::new(engram_core::traits::SystemClock::new()));

        sink(
            session_id,
            sandbox_id,
            HarnessEvent::RunStarted {
                run_id: "run-1".into(),
                prompt_summary: None,
                prompt_id: Some("p-missing".into()),
            },
        )
        .await;

        let events = mini.events.lock();
        assert_eq!(
            events.len(),
            1,
            "run_started must append even though its receipt lookup misses",
        );
        assert_eq!(events[0].kind, "run_started");
    }

    // -- ADR 0016 Phase B: live_disk_manifest + chunk_generation -------

    fn build_phase_b_meta(
        sandbox_id: Option<engram_core::SandboxId>,
    ) -> (engram_core::SessionId, Arc<MiniMeta>) {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id,
            image: "test/repo:phase-b".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        (session_id, Arc::new(MiniMeta::new(session)))
    }

    /// ADR 0073 ack-path regression: a harness `run_started{prompt_id}` MUST
    /// retire the durable outbox row it confirms. These events ingest through
    /// `harness_event_sink` (NOT `AppState::emit`, where the ack originally
    /// lived), so the sink has to ack itself. The bug: it didn't — the row
    /// stayed un-acked and the delivery driver re-resumed the session and
    /// re-ran the prompt on every idle cycle (acked_at NULL, attempts
    /// climbing), producing phantom re-runs and a duplicate-turn transcript
    /// that crashed the web. This pins the ack so a future refactor that moves
    /// the harness path off `emit` can't silently drop it again.
    #[tokio::test]
    async fn harness_run_started_with_prompt_id_acks_the_outbox_row() {
        let session = Session {
            id: SessionId::new(),
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:ack".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sid = session.id;
        let mini = Arc::new(MiniMeta::new(session));
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let events = Arc::new(SessionEventBus::new(8));
        let sink = harness_event_sink(
            events,
            meta,
            Arc::new(engram_core::traits::SystemClock::new()),
        );
        let sandbox_id = engram_core::SandboxId::new();

        // A run_started carrying the outbox row's prompt_id retires that row.
        sink(
            sid,
            sandbox_id,
            HarnessEvent::RunStarted {
                run_id: "run-1".into(),
                prompt_summary: None,
                prompt_id: Some("prompt-abc".into()),
            },
        )
        .await;
        assert_eq!(
            mini.acked_outbox.lock().as_slice(),
            ["prompt-abc".to_string()],
            "run_started{{prompt_id}} must ack the matching outbox row",
        );

        // A prompt_id-less run_started (there is no outbox row to confirm)
        // acks nothing — it must not spuriously retire some other row.
        sink(
            sid,
            sandbox_id,
            HarnessEvent::RunStarted {
                run_id: "run-2".into(),
                prompt_summary: None,
                prompt_id: None,
            },
        )
        .await;
        assert_eq!(
            mini.acked_outbox.lock().len(),
            1,
            "a run_started with no prompt_id must ack nothing",
        );

        sink(
            sid,
            sandbox_id,
            HarnessEvent::PromptSteered {
                prompt_id: "prompt-steered".into(),
            },
        )
        .await;
        assert_eq!(
            mini.acked_outbox.lock().as_slice(),
            ["prompt-abc".to_string(), "prompt-steered".to_string()],
            "prompt_steered must ack without opening a second run",
        );
    }

    #[tokio::test]
    async fn update_live_disk_manifest_applies_when_sandbox_matches() {
        let sandbox_id = engram_core::SandboxId::new();
        let (session_id, mini) = build_phase_b_meta(Some(sandbox_id));
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let mref = engram_core::types::manifest::ManifestRef::new();

        assert_eq!(meta.chunk_generation().await.unwrap(), 0);
        let outcome = meta
            .update_live_disk_manifest(session_id, sandbox_id, mref)
            .await
            .unwrap();
        assert_eq!(outcome, engram_core::traits::UpdateOutcome::Applied);
        // Generation bumps atomically with the write.
        assert_eq!(meta.chunk_generation().await.unwrap(), 1);
        // The stored manifest matches what we published.
        let stored = mini
            .live_disk_manifests
            .lock()
            .get(&session_id)
            .copied()
            .unwrap();
        assert_eq!(stored.0, sandbox_id);
        assert_eq!(stored.1, mref);
    }

    #[tokio::test]
    async fn update_live_disk_manifest_drops_stale_on_sandbox_mismatch() {
        let bound_sandbox = engram_core::SandboxId::new();
        let stale_sandbox = engram_core::SandboxId::new();
        let (session_id, mini) = build_phase_b_meta(Some(bound_sandbox));
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let mref = engram_core::types::manifest::ManifestRef::new();

        let outcome = meta
            .update_live_disk_manifest(session_id, stale_sandbox, mref)
            .await
            .unwrap();
        assert_eq!(outcome, engram_core::traits::UpdateOutcome::DroppedStale);
        // Generation does NOT bump on a stale publish — Phase C's
        // barrier must not see false pin-set changes.
        assert_eq!(meta.chunk_generation().await.unwrap(), 0);
        // Nothing stored either.
        assert!(mini.live_disk_manifests.lock().is_empty());
    }

    #[tokio::test]
    async fn update_live_disk_manifest_drops_stale_when_session_unbound() {
        // sandbox_id = None on the session (e.g. between snapshot and
        // resume). Any publish attempt is structurally stale.
        let (session_id, mini) = build_phase_b_meta(None);
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let mref = engram_core::types::manifest::ManifestRef::new();
        let sandbox_id = engram_core::SandboxId::new();

        let outcome = meta
            .update_live_disk_manifest(session_id, sandbox_id, mref)
            .await
            .unwrap();
        assert_eq!(outcome, engram_core::traits::UpdateOutcome::DroppedStale);
        assert_eq!(meta.chunk_generation().await.unwrap(), 0);
    }

    /// The load-bearing eviction-race mitigation: unbind clears the
    /// live manifest in the same step it NULLs sandbox_id. Without
    /// this, a publish that landed pre-unbind would leave a stale
    /// live_disk_manifest_id pointing past the snapshot's manifest;
    /// commit 6's effective_resume_disk_manifest resolver would
    /// then prefer it and restore disk-state AHEAD of memory.
    #[tokio::test]
    async fn assign_session_sandbox_none_clears_live_manifest_and_bumps_generation() {
        let sandbox_id = engram_core::SandboxId::new();
        let (session_id, mini) = build_phase_b_meta(Some(sandbox_id));
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let mref = engram_core::types::manifest::ManifestRef::new();

        // Land a publish first.
        meta.update_live_disk_manifest(session_id, sandbox_id, mref)
            .await
            .unwrap();
        let gen_after_publish = meta.chunk_generation().await.unwrap();
        assert_eq!(gen_after_publish, 1);
        assert!(mini.live_disk_manifests.lock().contains_key(&session_id));

        // Unbind. Live manifest disappears; generation bumps because
        // the pin set shrunk.
        meta.assign_session_sandbox(session_id, None).await.unwrap();
        assert!(!mini.live_disk_manifests.lock().contains_key(&session_id));
        assert_eq!(
            meta.chunk_generation().await.unwrap(),
            gen_after_publish + 1
        );
    }

    /// Rebinding to a new sandbox does NOT clear the live manifest —
    /// the next FlushScheduler publish from the new sandbox
    /// overwrites it (or the sandbox_id guard drops the new publish
    /// as stale if rebinding raced). This is the symmetric case to
    /// the unbind-clears test.
    #[tokio::test]
    async fn assign_session_sandbox_some_does_not_clear_or_bump() {
        let sandbox_id = engram_core::SandboxId::new();
        let (session_id, mini) = build_phase_b_meta(Some(sandbox_id));
        let meta: Arc<dyn MetadataStore> = mini.clone();
        let mref = engram_core::types::manifest::ManifestRef::new();

        meta.update_live_disk_manifest(session_id, sandbox_id, mref)
            .await
            .unwrap();
        let gen_before = meta.chunk_generation().await.unwrap();

        // Rebind to a new sandbox id (the eventual resume path).
        let new_sandbox = engram_core::SandboxId::new();
        meta.assign_session_sandbox(session_id, Some(new_sandbox))
            .await
            .unwrap();
        // Generation does NOT bump on a Some(_) rebind — the live
        // manifest from the prior sandbox is structurally stale and
        // will be either overwritten or guarded-out on the next
        // publish; bumping here would be a false barrier tick.
        assert_eq!(meta.chunk_generation().await.unwrap(), gen_before);
        // The map still has the prior publish (will be replaced on
        // the new sandbox's first publish; the sandbox_id guard
        // ensures no incoherent reads).
        assert!(mini.live_disk_manifests.lock().contains_key(&session_id));
    }
}
