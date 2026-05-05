use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_core::types::{ExecRusage, SessionStatus};
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
        from: SessionStatus,
        to: SessionStatus,
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
    /// Phase 4 (Track C.6): a checkpoint successfully pushed the
    /// session's workspace to its `engram/sessions/<id>` branch.
    /// `commit_sha` is the new HEAD on that branch; `harness_acked`
    /// reflects whether the in-guest harness flushed its transcript
    /// before the push (false on best-effort/timeout paths — the
    /// workspace is durable but the transcript may lag one tool call).
    CheckpointPushed {
        commit_sha: String,
        harness_acked: bool,
        at: DateTime<Utc>,
    },
    /// Phase 4 (Track C.6): a checkpoint attempt failed. Surfaces
    /// the underlying reason (workspace size cap, broken remote,
    /// missing harness, etc.) for operator forensics.
    CheckpointFailed {
        reason: String,
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
    HarnessIdle {
        at: DateTime<Utc>,
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
            Self::ExecStarted { .. } => "exec_started",
            Self::ExecCompleted { .. } => "exec_completed",
            Self::Stdout { .. } => "stdout",
            Self::Stderr { .. } => "stderr",
            Self::SnapshotTaken { .. } => "snapshot_taken",
            Self::Evicted { .. } => "evicted",
            Self::Resumed { .. } => "resumed",
            Self::CheckpointPushed { .. } => "checkpoint_pushed",
            Self::CheckpointFailed { .. } => "checkpoint_failed",
            Self::HarnessRunStarted { .. } => "run_started",
            Self::HarnessAgentMessage { .. } => "agent_message",
            Self::HarnessToolCallStarted { .. } => "tool_call_started",
            Self::HarnessToolCallCompleted { .. } => "tool_call_completed",
            Self::HarnessRunCompleted { .. } => "run_completed",
            Self::HarnessIdle { .. } => "harness_idle",
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

/// In-process session → live-sandbox-id map.
///
/// Sandboxes are ephemeral host-local resources, so this lives in
/// memory rather than in Postgres. After a coordinator restart the
/// map is empty; sessions whose sandboxes were lost will need to be
/// re-created or restored from snapshot before exec can succeed.
/// That maps cleanly to the "snapshots are a cache, not source of
/// truth" rule in DESIGN.md.
#[derive(Default)]
pub struct SandboxRegistry {
    inner: DashMap<SessionId, SandboxId>,
}

impl SandboxRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, session: SessionId, sandbox: SandboxId) {
        self.inner.insert(session, sandbox);
    }

    pub fn get(&self, session: SessionId) -> Option<SandboxId> {
        self.inner.get(&session).map(|r| *r)
    }

    pub fn unbind(&self, session: SessionId) -> Option<SandboxId> {
        self.inner.remove(&session).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

pub struct AppState {
    pub cfg: CoordinatorConfig,
    pub services: Services,
    pub registry: SandboxRegistry,
    /// Shared with the `pg_listener` task so cross-replica events
    /// land on the same broadcast bus as locally-emitted ones. Cheap
    /// to clone (`Arc` clone), so handlers freely take a reference and
    /// the listener task takes its own.
    pub events: Arc<SessionEventBus>,
    /// Multi-host routing layer. In `--mode=all` this has exactly one
    /// entry registered at startup (the local backend, wrapped in
    /// `engram_host_agent::pooled_backend::PooledBackend` so warm-pool
    /// semantics still apply); `--mode=coordinator` fills it in as
    /// hosts dial `/api/hosts/connect`. The warm pool is host-side
    /// now — the coordinator just routes via the scheduler and lets
    /// each host's `PooledBackend` handle checkout/replenish.
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
    /// Phase 4 Track F: coord-local bare clones of every writable
    /// repo Engram has touched, used for `engram session log/diff/
    /// fork` queries that need git history without involving a
    /// session sandbox. Lazily clones per repo URL on first query.
    pub git_workdir: Arc<crate::git_workdir::GitWorkdir>,
}

impl AppState {
    /// Convenience constructor for tests and `--mode=all`-flavoured
    /// embeddings: builds a fresh `HostRegistry` and pre-registers
    /// `services.sandbox` as the sole host. Callers that want warm-
    /// pool semantics in this single-host setup should pass a
    /// `PooledBackend`-wrapped `services.sandbox`. Production
    /// `--mode=coordinator` should use [`AppState::new_with_registry`]
    /// to thread a registry that hosts dial into via WS.
    pub fn new(cfg: CoordinatorConfig, services: Services) -> Self {
        let registry = Arc::new(HostRegistry::new());
        registry.register(HostId::new(), services.sandbox.clone());
        Self::new_with_registry(cfg, services, registry)
    }

    /// Construct an AppState whose `services.sandbox` already routes
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
            services.sandbox.clone(),
        )));
        let git_workdir = Arc::new(crate::git_workdir::GitWorkdir::new(
            cfg.local_path.join("git-workdirs"),
        ));
        Self {
            cfg,
            services,
            registry: SandboxRegistry::new(),
            events,
            host_registry,
            harness_hub,
            harness_listen_addr: parking_lot::Mutex::new(None),
            git_workdir,
        }
    }

    /// Where to write per-session snapshot directories on local disk.
    /// Per-host scratch under `cfg.local_path`; not durable across host
    /// loss. Cross-host durability for sessions is git, not snapshots.
    pub fn snapshot_dir(&self) -> std::path::PathBuf {
        self.cfg.local_path.join("snapshots")
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

/// Build the [`EventSink`] that forwards harness events into
/// `session_events` and triggers Track C.9 auto-checkpoints on
/// `Idle` / `RunCompleted` for Git sessions. Captures clones of
/// the bus + meta service + sandbox backend so the closure has no
/// cycles back into AppState.
fn harness_event_sink(
    events: Arc<SessionEventBus>,
    meta: Arc<dyn engram_core::traits::MetadataStore>,
    sandbox: Arc<dyn engram_core::traits::SandboxBackend>,
) -> EventSink {
    Arc::new(move |session_id, sandbox_id, ev| {
        let events = events.clone();
        let meta = meta.clone();
        let sandbox = sandbox.clone();
        Box::new(Box::pin(async move {
            // 1. Always forward the event into session_events (live
            //    SSE / Web UI / Slackbot timeline).
            let triggers_checkpoint =
                matches!(ev, HarnessEvent::Idle | HarnessEvent::RunCompleted { .. });
            let session_event = SessionEvent::from_harness(ev, Utc::now());
            let kind = session_event.kind();
            let payload = match serde_json::to_value(&session_event) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "harness event serialize failed");
                    return;
                }
            };
            match meta.append_session_event(session_id, kind, payload).await {
                Ok(idx) => {
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

            // 2. Auto-checkpoint on Idle / RunCompleted for Git
            //    sessions. Skip the harness round-trip — the agent
            //    just told us it's idle.
            if !triggers_checkpoint {
                return;
            }
            auto_checkpoint(session_id, sandbox_id, &meta, &sandbox, &events).await;
        }))
    })
}

/// Look up the session, decide whether to checkpoint, run the git
/// push, emit `CheckpointPushed` / `CheckpointFailed`. All best-
/// effort: failures log + emit a CheckpointFailed event but never
/// propagate (the event sink can't return an error to the harness).
pub(crate) async fn auto_checkpoint(
    session_id: SessionId,
    sandbox_id: SandboxId,
    meta: &Arc<dyn engram_core::traits::MetadataStore>,
    sandbox: &Arc<dyn engram_core::traits::SandboxBackend>,
    events: &Arc<SessionEventBus>,
) {
    let session = match meta.get_session(session_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "auto_checkpoint: get_session failed; skipping",
            );
            return;
        }
    };
    if !matches!(
        session.session_kind,
        engram_core::types::session::SessionKind::Git
    ) {
        return;
    }
    let branch = match session.checkpoint_branch.as_deref() {
        Some(b) => b,
        None => {
            tracing::warn!(
                session_id = %session_id,
                "auto_checkpoint: git session is missing its checkpoint_branch",
            );
            return;
        }
    };

    use engram_harness_proto::CheckpointReason;
    use engram_host_agent::checkpoint::{checkpoint_workspace_only, CheckpointConfig};

    let cfg = CheckpointConfig::default();
    let outcome = checkpoint_workspace_only(
        sandbox,
        sandbox_id,
        ".",
        branch,
        CheckpointReason::RunCompleted,
        &cfg,
    )
    .await;

    let now = Utc::now();
    let event = match outcome {
        Ok(o) => match o.commit_sha {
            Some(sha) => SessionEvent::CheckpointPushed {
                commit_sha: sha,
                harness_acked: o.harness_acked,
                at: now,
            },
            None => {
                // Empty diff — no commit, no push, no event. We
                // intentionally don't emit a CheckpointPushed for
                // this case so SSE subscribers only see "real"
                // checkpoints.
                return;
            }
        },
        Err(e) => SessionEvent::CheckpointFailed {
            reason: e.to_string(),
            at: now,
        },
    };
    let kind = event.kind();
    let payload = match serde_json::to_value(&event) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "auto_checkpoint event serialize failed");
            return;
        }
    };
    match meta.append_session_event(session_id, kind, payload).await {
        Ok(idx) => {
            events.publish(session_id, IndexedEvent { idx, event });
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "append_session_event for checkpoint result failed",
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn registry_bind_get_unbind() {
        let r = SandboxRegistry::new();
        let sid = SessionId::new();
        let sbx = SandboxId::new();
        assert!(r.is_empty());
        assert_eq!(r.get(sid), None);

        r.bind(sid, sbx);
        assert_eq!(r.len(), 1);
        assert_eq!(r.get(sid), Some(sbx));

        let removed = r.unbind(sid);
        assert_eq!(removed, Some(sbx));
        assert!(r.is_empty());
        assert_eq!(r.get(sid), None);
        assert_eq!(r.unbind(sid), None, "second unbind is a no-op");
    }

    fn evicted() -> SessionEvent {
        SessionEvent::Evicted {
            at: chrono::Utc::now(),
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

    #[test]
    fn registry_isolates_sessions() {
        let r = SandboxRegistry::new();
        let s1 = SessionId::new();
        let s2 = SessionId::new();
        let b1 = SandboxId::new();
        let b2 = SandboxId::new();
        r.bind(s1, b1);
        r.bind(s2, b2);
        assert_eq!(r.get(s1), Some(b1));
        assert_eq!(r.get(s2), Some(b2));
        r.unbind(s1);
        assert_eq!(
            r.get(s2),
            Some(b2),
            "unbinding one session must not affect another"
        );
    }

    // ---------------------------------------------------------------
    // auto_checkpoint integration: real ProcessBackend + real git
    // remote + minimal in-line MetadataStore mock. Verifies that on
    // a Git session with workspace changes, the auto path commits +
    // pushes to the checkpoint branch and emits CheckpointPushed.
    // ---------------------------------------------------------------
    use async_trait::async_trait;
    use engram_core::traits::{MetadataStore, SandboxBackend};
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
    use engram_core::types::session::{
        checkpoint_branch_for, HarnessSpec, SessionKind, WorkspaceSpec,
    };
    use engram_core::types::{
        HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SnapshotRecord,
    };
    use engram_core::{HostId, MetaError};
    use engram_sandbox_process::ProcessBackend;
    use parking_lot::Mutex as PlMutex;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use std::time::Duration;
    use tempfile::TempDir;

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
    }

    impl MiniMeta {
        pub(crate) fn new(session: Session) -> Self {
            Self {
                session: PlMutex::new(session),
                events: PlMutex::new(Vec::new()),
                next_idx: PlMutex::new(0),
                snapshots: PlMutex::new(Vec::new()),
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
        async fn set_session_status(
            &self,
            id: engram_core::SessionId,
            status: engram_core::types::SessionStatus,
        ) -> Result<(), MetaError> {
            let mut s = self.session.lock();
            if id != s.id {
                return Err(MetaError::NotFound);
            }
            s.status = status;
            Ok(())
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
            Ok(())
        }
        async fn upsert_host(&self, _: HostRecord) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn set_host_status(&self, _: HostId, _: HostStatus) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_reassign_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<engram_core::SessionId>, MetaError> {
            Ok(Vec::new())
        }
        async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
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
                .filter(|s| s.session_id == sid)
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
                .rfind(|s| s.session_id == sid)
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
        async fn upsert_harness_pack(
            &self,
            _: engram_core::types::HarnessPack,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_harness_packs(
            &self,
        ) -> Result<Vec<engram_core::types::HarnessPack>, MetaError> {
            Ok(Vec::new())
        }
        async fn get_harness_pack(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::HarnessPack>, MetaError> {
            Ok(None)
        }
        async fn delete_harness_pack(&self, _: &str) -> Result<(), MetaError> {
            Ok(())
        }
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
        async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
            Ok(())
        }
    }

    fn run_git(args: &[&str], cwd: &Path) {
        let out = StdCommand::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "host {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seed_remote_and_workspace() -> (TempDir, TempDir) {
        let remote = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        run_git(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );
        run_git(&["git", "init", "--initial-branch=main"], workspace.path());
        run_git(
            &[
                "git",
                "remote",
                "add",
                "origin",
                &format!("{}", remote.path().display()),
            ],
            workspace.path(),
        );
        run_git(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
            workspace.path(),
        );
        run_git(&["git", "push", "-u", "origin", "main"], workspace.path());
        (remote, workspace)
    }

    fn process_spec(rootfs: &Path) -> SandboxSpec {
        SandboxSpec {
            image: "auto-checkpoint-test".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
        }
    }

    #[tokio::test]
    async fn auto_checkpoint_pushes_completed_run_to_checkpoint_branch() {
        let (remote, workspace) = seed_remote_and_workspace();

        // Build a Git session whose checkpoint_branch is set to the
        // standard `engram/sessions/<id>` namespace.
        let session_id = engram_core::SessionId::new();
        let branch = checkpoint_branch_for(session_id);
        let session = Session {
            id: session_id,
            user_id: None,
            status: engram_core::types::SessionStatus::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:auto-checkpoint-test".into(),
            workspace: WorkspaceSpec::Git {
                url: format!("file://{}", remote.path().display()),
                branch: "main".into(),
                read_only: false,
            },
            harness: HarnessSpec::None,
            session_kind: SessionKind::Git,
            checkpoint_branch: Some(branch.clone()),
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };
        let meta: Arc<dyn MetadataStore> = Arc::new(MiniMeta::new(session));

        // ProcessBackend whose sandbox cwd starts as a clone of the
        // seed workspace (origin → bare remote).
        let sandbox_root = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let sandbox_id = backend
            .create(process_spec(workspace.path()))
            .await
            .unwrap();

        // Simulate the agent making a workspace edit.
        let req = ExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "echo agent-output > result.txt".into(),
            ],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        backend.exec(sandbox_id, req).await.unwrap();

        let bus = Arc::new(SessionEventBus::default());
        let mut sub = bus.subscribe(session_id);

        super::auto_checkpoint(session_id, sandbox_id, &meta, &backend, &bus).await;

        // The bus published exactly one CheckpointPushed event with
        // a non-empty commit_sha matching the remote branch HEAD.
        let event = tokio::time::timeout(Duration::from_millis(500), sub.recv())
            .await
            .expect("event arrived")
            .expect("subscriber alive");
        let commit_sha = match event.event {
            SessionEvent::CheckpointPushed { commit_sha, .. } => commit_sha,
            other => panic!("expected CheckpointPushed, got {other:?}"),
        };
        assert!(!commit_sha.is_empty());

        let out = StdCommand::new("git")
            .args(["-C"])
            .arg(remote.path())
            .args(["rev-parse", &branch])
            .output()
            .expect("rev-parse");
        let remote_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(remote_sha, commit_sha);
    }

    #[tokio::test]
    async fn auto_checkpoint_skips_ephemeral_session_silently() {
        // Ephemeral sessions have no checkpoint branch — auto_checkpoint
        // returns without emitting anything, and no error is logged
        // upward. The bus should see zero events.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: engram_core::types::SessionStatus::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:test".into(),
            workspace: WorkspaceSpec::Empty,
            harness: HarnessSpec::None,
            session_kind: SessionKind::Ephemeral,
            checkpoint_branch: None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };
        let meta: Arc<dyn MetadataStore> = Arc::new(MiniMeta::new(session));

        // Bare-bones sandbox; we never reach exec_stream because the
        // session_kind early-returns.
        let sandbox_root = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let spec = SandboxSpec {
            image: "local-test".into(),
            rootfs_source: None,
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
        };
        let sandbox_id = backend.create(spec).await.unwrap();

        let bus = Arc::new(SessionEventBus::default());
        let mut sub = bus.subscribe(session_id);

        super::auto_checkpoint(session_id, sandbox_id, &meta, &backend, &bus).await;

        // Nothing emitted within a generous deadline.
        let recv = tokio::time::timeout(Duration::from_millis(100), sub.recv()).await;
        assert!(recv.is_err(), "Local session must not produce events");
    }
}
