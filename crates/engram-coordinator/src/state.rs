use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_core::types::{ExecRusage, SessionState};
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use engram_harness_proto::HarnessEvent;
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
    },
    Stderr {
        exec_id: String,
        chunk: String,
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
        at: DateTime<Utc>,
    },
    HarnessAgentMessage {
        run_id: String,
        message_id: String,
        role: engram_harness_proto::AgentRole,
        text: String,
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
    /// ADR 0023: the agent opened a change request (PR/MR) via the
    /// in-session forge seam. Surfaces the title + URL to the web UI /
    /// SSE subscribers so the session's output artifact is visible (the
    /// web transcript renders this as a pull-request card). `number` is
    /// the provider's PR number / MR iid.
    PullRequestOpened {
        url: String,
        repo: String,
        title: String,
        number: u64,
        head_branch: String,
        base_branch: String,
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
}

impl SessionEvent {
    /// Discriminant string used both as the SSE `event:` field and as
    /// the `kind` column in `session_events`. Stable across
    /// coordinator restarts; persisted clients (Slack, etc.) match on
    /// this.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::StatusChanged { .. } => "status_changed",
            Self::ExecStarted { .. } => "exec_started",
            Self::ExecCompleted { .. } => "exec_completed",
            Self::Stdout { .. } => "stdout",
            Self::Stderr { .. } => "stderr",
            Self::SnapshotTaken { .. } => "snapshot_taken",
            Self::Evicted { .. } => "evicted",
            Self::Resumed { .. } => "resumed",
            Self::HarnessRunStarted { .. } => "run_started",
            Self::HarnessAgentMessage { .. } => "agent_message",
            Self::HarnessToolCallStarted { .. } => "tool_call_started",
            Self::HarnessToolCallCompleted { .. } => "tool_call_completed",
            Self::HarnessRunCompleted { .. } => "run_completed",
            Self::HarnessRunInterrupted { .. } => "run_interrupted",
            Self::HarnessIdle { .. } => "harness_idle",
            Self::PullRequestOpened { .. } => "pull_request_opened",
            Self::FileShared { .. } => "file_shared",
            Self::RecoveredFromCheckpoint { .. } => "recovered_from_checkpoint",
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
            } => Self::HarnessRunStarted {
                run_id,
                prompt_summary,
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
            HarnessEvent::RunCompleted { run_id, ok } => {
                Self::HarnessRunCompleted { run_id, ok, at }
            }
            HarnessEvent::RunInterrupted { run_id } => Self::HarnessRunInterrupted { run_id, at },
            HarnessEvent::Idle => Self::HarnessIdle { at },
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
    /// ADR 0016 §A.1.5c: stable identifier for this coord pod —
    /// stamped on the `session_lease.locked_by` column so
    /// `SELECT * FROM session_lease` tells an operator which
    /// pod is mid-eviction on which session. Defaults to
    /// `hostname` (the k8s pod name in prod); cheap to override
    /// via the `ENGRAM_COORD_POD_ID` env var if local tests want
    /// a deterministic value.
    pub pod_id: Arc<String>,
    /// ADR 0023/0047: per-session credential-broker tokens (session →
    /// expected bearer). PURE READ-THROUGH CACHE over the KEK-sealed
    /// `session_broker_tokens` PG rows (the authority — minted
    /// first-writer-wins, so the token is stable for the session's
    /// lifetime on every replica). A miss loads + unseals from PG;
    /// cleared at terminal alongside the row.
    pub git_broker_tokens: Arc<dashmap::DashMap<SessionId, String>>,
    /// ADR 0023: the configured git forge authority (GitHub App, etc).
    /// Set on `main`'s run path via `run_with_registry_and_local`;
    /// `None` in tests and when `--git-forge` is unset (the forge
    /// endpoints then 501). Kept here rather than on `Services` so the
    /// many test `Services` literals don't need touching.
    pub forge: Option<Arc<dyn engram_core::traits::GitForge>>,
    // ADR 0039 Task 31/32 (ADR 0051 reconcile): the per-user auth runtime
    // (`auth: Option<Arc<AuthRuntime>>`) is removed. The coordinator no
    // longer resolves human principals — the orchestrator owns auth/authz
    // and calls the coordinator over the trusted app-gRPC surface. Session
    // attribution survives as the plain `sessions.user_id` column, stamped
    // by the orchestrator (the gRPC create passes it through; today `None`).
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
        let harness_hub = Arc::new(HarnessHub::new(harness_event_sink(
            events.clone(),
            services.meta.clone(),
        )));
        let reconciler =
            crate::reconcile::Reconciler::new(crate::reconcile::grace_ticks_from_env());
        Self {
            cfg,
            services,
            events,
            host_registry,
            harness_hub,
            harness_listen_addr: parking_lot::Mutex::new(None),
            reconciler,
            cow_state_cache: Arc::new(crate::cow_state::CowStateCache::new()),
            pod_id: Arc::new(resolve_pod_id()),
            git_broker_tokens: Arc::new(dashmap::DashMap::new()),
            forge: None,
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
        self.events.publish(session, IndexedEvent { idx, event });
        Ok(idx)
    }
}

pub type SharedState = Arc<AppState>;

/// ADR 0016 §A.1.5c: resolve a stable pod identifier for the
/// `session_lease.locked_by` column. Precedence:
///   1. `ENGRAM_COORD_POD_ID` env (explicit override for local tests).
///   2. `HOSTNAME` env (set by k8s on every pod).
///   3. `unknown` string (no panic; the column is diagnostic).
fn resolve_pod_id() -> String {
    if let Ok(v) = std::env::var("ENGRAM_COORD_POD_ID") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Ok(v) = std::env::var("HOSTNAME") {
        if !v.is_empty() {
            return v;
        }
    }
    "unknown".to_string()
}

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
/// sink's `SessionEvent::from_harness` stamps its own `Utc::now()`
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
) -> EventSink {
    // Per-session cache of the most-recent forwarded event kind. Used
    // to drop a `harness_idle` that would land back-to-back with
    // another `harness_idle`: the claude harness re-announces Idle on
    // every reconnect (a protocol "ready for prompts" signal), so an
    // evict/resume cycle on an already-idle session would otherwise
    // append a redundant idle to the log on every cycle.
    let last_kind: Arc<DashMap<SessionId, &'static str>> = Arc::new(DashMap::new());
    Arc::new(move |session_id, _sandbox_id, ev| {
        let events = events.clone();
        let meta = meta.clone();
        let last_kind = last_kind.clone();
        Box::new(Box::pin(async move {
            // Forward every harness event into session_events for live
            // SSE / Web UI / Slackbot timeline. ADR 0005 retired the
            // auto-checkpoint branch this used to trigger on Idle /
            // RunCompleted; durability moved to hot+cold snapshots,
            // not git checkpoints.
            let session_event = SessionEvent::from_harness(ev, Utc::now());
            let kind = session_event.kind();

            // Drop a back-to-back duplicate `harness_idle`. The
            // upstream TTL bookkeeping in HarnessHub::reader_loop
            // already saw the event, so suppressing it here only
            // affects the persisted log + SSE bus.
            if kind == "harness_idle"
                && last_kind
                    .get(&session_id)
                    .map(|v| *v == "harness_idle")
                    .unwrap_or(false)
            {
                return;
            }

            let payload = match serde_json::to_value(&session_event) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "harness event serialize failed");
                    return;
                }
            };
            match meta.append_session_event(session_id, kind, payload).await {
                Ok(idx) => {
                    last_kind.insert(session_id, kind);
                    events.publish(
                        session_id,
                        IndexedEvent {
                            idx,
                            event: session_event,
                        },
                    );
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn evicted() -> SessionEvent {
        SessionEvent::Evicted {
            at: chrono::Utc::now(),
        }
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
        IndexedEvent { idx, event }
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
        /// ADR 0016 §A.1.5c: in-memory mirror of the
        /// `session_lease` PG table for the trait's three
        /// lease methods. Tests insert directly here to simulate
        /// a peer coord pod holding a lease.
        pub(crate) session_leases: PlMutex<SessionLeaseMap>,
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
    }

    /// Alias so the `clippy::type_complexity` lint stays happy on
    /// MiniMeta's leases field. Mirrors `session_lease`'s
    /// shape: `session_id → (sandbox_id?, locked_by, locked_at)` —
    /// `sandbox_id` is `None` for a resume lease, `Some` for an
    /// eviction.
    pub(crate) type SessionLeaseMap = std::collections::HashMap<
        SessionId,
        (
            Option<engram_core::SandboxId>,
            String,
            chrono::DateTime<chrono::Utc>,
        ),
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
                local_snapshots: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
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
                session_leases: PlMutex::new(std::collections::HashMap::new()),
                live_disk_manifests: PlMutex::new(std::collections::HashMap::new()),
                chunk_generation: PlMutex::new(0),
                evac_attempts: PlMutex::new(std::collections::HashMap::new()),
                evict_attempts: PlMutex::new(std::collections::HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl MetadataStore for MiniMeta {
        async fn create_session(
            &self,
            _: SessionSpec,
        ) -> Result<engram_core::SessionId, MetaError> {
            unreachable!("create_session not used in state tests")
        }
        async fn create_session_created(
            &self,
            _: engram_core::SessionId,
            _: SessionSpec,
            _: engram_core::HostId,
            _: engram_core::SandboxId,
        ) -> Result<(), MetaError> {
            unreachable!("create_session_created not used in state tests")
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
            let mut hosts = self.hosts.lock();
            if let Some(h) = hosts.iter_mut().find(|h| h.id == id) {
                h.status = hb.status;
                h.capacity = hb.capacity;
                h.utilization = hb.utilization;
                h.ready_images = hb.ready_images;
                h.local_snapshots = hb.local_snapshots;
                h.current_bundles = hb.current_bundles;
                h.total_vcpus = hb.total_vcpus;
                h.last_heartbeat_at = chrono::Utc::now();
            }
            Ok(())
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
        async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
            let mut should_fail = self.fail_next_record_snapshot.lock();
            if *should_fail {
                *should_fail = false;
                return Err(MetaError::Conflict(
                    "MiniMeta fail_next_record_snapshot: injected failure".into(),
                ));
            }
            drop(should_fail);
            self.snapshots.lock().push(snap);
            Ok(())
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
        async fn upsert_session_secrets(
            &self,
            _: engram_core::types::SessionSecrets,
        ) -> Result<(), MetaError> {
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

        // ADR 0016 §A.1.5c: in-memory mirror of the
        // `session_lease` PG table so the trait's lease
        // semantics are exercised end-to-end by the idle_evictor
        // unit tests.
        async fn try_acquire_session_lease(
            &self,
            session_id: SessionId,
            sandbox_id: Option<engram_core::SandboxId>,
            locked_by: &str,
        ) -> Result<bool, MetaError> {
            let mut guard = self.session_leases.lock();
            if guard.contains_key(&session_id) {
                return Ok(false);
            }
            guard.insert(
                session_id,
                (sandbox_id, locked_by.to_string(), chrono::Utc::now()),
            );
            Ok(true)
        }

        async fn release_session_lease(&self, session_id: SessionId) -> Result<(), MetaError> {
            self.session_leases.lock().remove(&session_id);
            Ok(())
        }

        async fn sweep_stale_session_leases(
            &self,
            max_age: std::time::Duration,
        ) -> Result<Vec<engram_core::traits::StaleSessionLease>, MetaError> {
            let mut guard = self.session_leases.lock();
            let now = chrono::Utc::now();
            let cutoff = match chrono::Duration::from_std(max_age) {
                Ok(d) => now - d,
                Err(_) => return Ok(Vec::new()),
            };
            let mut reaped = Vec::new();
            guard.retain(|session_id, (sandbox_id, locked_by, locked_at)| {
                if *locked_at < cutoff {
                    reaped.push(engram_core::traits::StaleSessionLease {
                        session_id: *session_id,
                        sandbox_id: *sandbox_id,
                        locked_by: locked_by.clone(),
                        locked_at: *locked_at,
                    });
                    false
                } else {
                    true
                }
            });
            Ok(reaped)
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
    async fn harness_event_sink_dedupes_back_to_back_idles() {
        // The claude harness re-emits Idle on every reconnect (e.g.
        // after an evict/resume cycle on an already-idle session).
        // Persisting each one would litter the timeline with redundant
        // "awaiting prompt" markers; the sink drops the duplicates.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:idle-dedup".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };
        let mini = Arc::new(MiniMeta::new(session));
        let meta: Arc<dyn MetadataStore> = mini.clone();

        let sandbox_id = engram_core::SandboxId::new();

        let bus = Arc::new(SessionEventBus::default());
        let sink = super::harness_event_sink(bus.clone(), meta.clone());

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
    }

    // -- ADR 0016 Phase B: live_disk_manifest + chunk_generation -------

    fn build_phase_b_meta(
        sandbox_id: Option<engram_core::SandboxId>,
    ) -> (engram_core::SessionId, Arc<MiniMeta>) {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: engram_core::types::SessionState::Active,
            host_id: None,
            sandbox_id,
            image: "test/repo:phase-b".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };
        (session_id, Arc::new(MiniMeta::new(session)))
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
