//! Harness-channel hub on the host side.
//!
//! Owns one [`HarnessConnection`] per attached harness. Each connection
//! is a long-lived bidirectional stream wrapping a guest-dialed link
//! (vsock in production, UDS for tests). On each received
//! [`HarnessEvent`] the hub:
//!
//! 1. updates the per-sandbox `last_event_at` (the idle evictor reads
//!    this to decide who to suspend),
//! 2. invokes the configured [`EventSink`] callback so the host-agent
//!    forwards the event into `session_events` (where the SSE bus,
//!    Web UI, Slackbot, and resume bootstrap pick it up),
//! 3. fields any pending [`HarnessCommand`] reply (Checkpoint /
//!    Shutdown ack) on a per-`req_id` `oneshot`.
//!
//! The `accept` side is intentionally generic over `AsyncRead +
//! AsyncWrite` so unit tests can use UDS / in-memory streams; Track B
//! wires a real vsock listener through the same API.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, CheckpointReason, HarnessAttach, HarnessAttachAck, HarnessCommand,
    HarnessEvent, HarnessFrame,
};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// Default timeout for a `Checkpoint` command's ack. The harness gets
/// this long to flush its transcript and reply; on miss the caller
/// surfaces `HarnessError::CommandTimeout` and (in idle/preempt paths)
/// proceeds without the durability guarantee.
pub const CHECKPOINT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a `shell_attached` pin may go un-renewed before the host's
/// eviction tick reaps it (issue #219). The coord shell bridge renews
/// the pin every [`SHELL_PIN_RENEW_INTERVAL`] while the WS is live, so
/// a healthy session refreshes the stamp ~5× before this fires. A pin
/// older than this means the renewer is gone (coord pod died mid-
/// session, or the bridge task was torn down without a `ReleaseShell`)
/// and the sandbox must fall back under the normal idle TTLs. Chosen
/// well above the renew interval so transient renew failures or coord
/// GC pauses don't prematurely un-pin a session a human is using.
pub const SHELL_PIN_STALE_AGE: Duration = Duration::from_secs(300);

/// How often the coord shell bridge renews a live `shell_attached`
/// pin (issue #219). Piggybacks the WS keepalive cadence. Must stay
/// comfortably below [`SHELL_PIN_STALE_AGE`] so a few dropped renewals
/// don't trip the host-side stale sweep.
pub const SHELL_PIN_RENEW_INTERVAL: Duration = Duration::from_secs(60);

/// How long `send_prompt` waits for the harness connection to appear
/// before giving up with `NotAttached`. Covers the post-resume window
/// where ensure_active has returned but the in-VM bootstrap+harness
/// handshake hasn't finished — typically a few hundred ms after FC
/// restore. Generous enough to absorb cold-restore variance, tight
/// enough that a session whose harness genuinely never reattaches
/// surfaces an error in user-visible time.
const SEND_PROMPT_ATTACH_WAIT_SECS: u64 = 10;

/// Callback invoked for every `HarnessEvent` received on a connection.
/// In production this is the host-agent's bridge into the coordinator's
/// `session_events` log; tests pass a closure that just collects them.
pub type EventSink = Arc<
    dyn Fn(SessionId, SandboxId, HarnessEvent) -> Box<dyn Future<Output = ()> + Send + Unpin>
        + Send
        + Sync,
>;

/// Public-facing hub. One per host-agent process. Cheap to clone (`Arc`
/// inside).
#[derive(Clone)]
pub struct HarnessHub {
    inner: Arc<HubInner>,
}

/// CANONICAL LOCK ORDER (issue #217).
///
/// Several of `HubInner`'s fields are independent `parking_lot::Mutex`es
/// that some paths must hold simultaneously. parking_lot mutexes are
/// thread-blocking with no deadlock detection, so any two paths that
/// nest the same pair of locks in opposite orders can wedge forever
/// (ABBA) — and once wedged here, the whole harness plane stalls while
/// heartbeats keep the host looking healthy to coord.
///
/// To make nesting safe, every site that holds more than one of these
/// at once MUST acquire them in this order (acquire a prefix; a path
/// may skip locks it doesn't need but must not reorder):
///
/// 1. `last_event_at`
/// 2. `last_idle_at`
/// 3. `connections`
/// 4. `shell_attached`
/// 5. `eviction_inflight`
///
/// `idle_sandboxes` holds the whole chain and defines this order. The
/// only path that previously inverted it was `send_prompt`, which held
/// `connections` while acquiring `last_idle_at`; it now drops the
/// `connections` guard first (clones `cmd_tx` into an owned value)
/// before touching `last_idle_at`, so it no longer nests the two.
/// `session_to_sandbox` and per-connection locks (`pending_checkpoint`)
/// are leaf locks not nested with this chain.
struct HubInner {
    /// Per-sandbox connection state. Populated by `accept_connection`,
    /// removed when the harness disconnects (clean or error).
    connections: Mutex<HashMap<SandboxId, ConnectionHandle>>,
    /// Per-sandbox last-event-of-any-kind timestamp. Drives the
    /// **hard** TTL — backstop for stuck adapters that never emit
    /// `Idle`. Updated on every inbound `HarnessEvent`.
    last_event_at: Mutex<HashMap<SandboxId, DateTime<Utc>>>,
    /// Per-sandbox last-`Idle`-event timestamp. Drives the **soft**
    /// TTL — "agent is awaiting user input." Set on `HarnessEvent::Idle`,
    /// cleared (set to None / removed) on any other event so a long
    /// tool call doesn't trip the soft TTL mid-call. Also cleared on
    /// `send_prompt` so the host's "we just gave you work" closes
    /// the slow-adapter race.
    last_idle_at: Mutex<HashMap<SandboxId, DateTime<Utc>>>,
    /// Event-emit callback supplied at construction. Invoked for every
    /// inbound `HarnessEvent` after the local maps are updated.
    event_sink: EventSink,
    /// SessionId → SandboxId routing for the TCP listener path.
    /// Populated by `bind_session` when the host-agent spawns a
    /// harness; cleared by `unbind_session` (or implicitly on
    /// `destroy()`). The listener reads HarnessAttach off an incoming
    /// connection, looks up the sandbox here, and hands the stream to
    /// the existing connection logic.
    session_to_sandbox: Mutex<HashMap<SessionId, SandboxId>>,
    /// Sandboxes with at least one external long-lived client
    /// connected (today: a browser shell WebSocket). Counted so a
    /// future second client doesn't accidentally let the first one
    /// release the keep-alive. While the count is > 0 for a sandbox,
    /// `idle_sandboxes` skips it — the human is actively poking at
    /// the box and we don't snapshot-and-evict under their feet.
    ///
    /// Each entry is `(count, last_renewed)`. The pin is acquired and
    /// released by two independent coord→host RPCs from the same axum
    /// task that bridges the WS (`api/shell.rs`). If the coord pod dies
    /// mid-session (rolling deploy) or the `ReleaseShell` RPC fails,
    /// that task never sends the release and the count would otherwise
    /// stay ≥ 1 forever — permanently exempting the sandbox from idle
    /// eviction, hard-TTL backstop included (issue #219). To bound the
    /// drift, the coord bridge renews the pin on its WS keepalive
    /// interval (`renew_shell` stamps `last_renewed`), and the host's
    /// eviction tick reaps entries not renewed within
    /// [`SHELL_PIN_STALE_AGE`] (`sweep_stale_shells`) — exactly mirroring
    /// the `eviction_inflight` hardening below.
    shell_attached: Mutex<HashMap<SandboxId, (u32, Instant)>>,
    /// ADR 0016 §A.1.5a: per-sandbox "an idle-eviction POST for this
    /// sandbox is currently in flight on coord". Marked just before
    /// the host's eviction-task POSTs candidates; cleared when the
    /// fire-and-forget spawned task observes the POST's outcome
    /// (success, transport error, or timeout). `idle_sandboxes`
    /// filters out anything still in this map, so a second POST
    /// can't race ahead of the first while coord is mid-pipeline.
    /// Stored as `Instant` so the periodic stale sweep can reap
    /// entries whose spawned task wedged (>180s — see eviction
    /// task code).
    eviction_inflight: Mutex<HashMap<SandboxId, Instant>>,
    /// Monotonic generation counter (issue #218). Each `drive_attached`
    /// task takes a fresh value via `fetch_add` before it inserts its
    /// `ConnectionHandle`, stamping the handle with that generation.
    /// The teardown epilogue then removes the per-sandbox entries ONLY
    /// if the currently-registered handle still carries its own
    /// generation — so a stale connection's EOF can never delete the
    /// registration a newer reconnect installed under the same
    /// `sandbox_id`. Relaxed ordering is sufficient: correctness rests
    /// on uniqueness + the `connections` mutex serializing the compare,
    /// not on cross-thread happens-before of the counter itself.
    next_gen: AtomicU64,
}

struct ConnectionHandle {
    /// Sender into the writer task; takes encoded `HarnessFrame`s.
    /// Bounded to keep an unbacked-up writer from blowing memory.
    cmd_tx: mpsc::Sender<HarnessFrame>,
    /// Currently-pending checkpoint ack. Only one `Checkpoint` may be
    /// in flight per connection at a time — the host serializes them.
    pending_checkpoint: Mutex<Option<oneshot::Sender<()>>>,
    /// Session this harness belongs to (from `HarnessAttach`).
    /// Stored on the connection so the idle evictor can return
    /// `(SessionId, SandboxId)` pairs without a separate lookup.
    session_id: SessionId,
    /// Per-hub-unique generation stamped at insert (issue #218). The
    /// teardown epilogue compares the live entry's generation against
    /// its own before removing, so a stale connection never evicts the
    /// replacement registration that a reconnect installed.
    generation: u64,
}

impl HarnessHub {
    /// Drive the hub's `EventSink` for a harness event that didn't
    /// originate from a local adapter loop. Used by the coord-side
    /// ingest path: a remote host's hub fires its sink to ship a
    /// `NotifyKind::HarnessEvent` over the WS, and the coord's read
    /// loop replays the event through *its* hub so the in-proc
    /// session_events emit closure (built by `harness_event_sink`)
    /// gets the same view it would in mode=all.
    pub async fn emit_external(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        event: HarnessEvent,
    ) {
        let fut: Box<dyn Future<Output = ()> + Send + Unpin> =
            (self.inner.event_sink)(session_id, sandbox_id, event);
        Box::into_pin(fut).await;
    }

    pub fn new(event_sink: EventSink) -> Self {
        Self {
            inner: Arc::new(HubInner {
                connections: Mutex::new(HashMap::new()),
                last_event_at: Mutex::new(HashMap::new()),
                last_idle_at: Mutex::new(HashMap::new()),
                event_sink,
                session_to_sandbox: Mutex::new(HashMap::new()),
                shell_attached: Mutex::new(HashMap::new()),
                eviction_inflight: Mutex::new(HashMap::new()),
                next_gen: AtomicU64::new(0),
            }),
        }
    }

    /// Mark a sandbox as having an active external shell client. While
    /// the count is non-zero, the soft/hard idle TTLs are suppressed:
    /// the user opening a browser shell is unambiguous "I'm using this
    /// session, leave it alone" intent. Symmetrically released by
    /// `release_shell`. Reference-counted so a future second client
    /// (e.g. a second tab) doesn't release the keep-alive when the
    /// first disconnects.
    pub fn acquire_shell(&self, sandbox_id: SandboxId) {
        let mut map = self.inner.shell_attached.lock();
        let slot = map.entry(sandbox_id).or_insert((0, Instant::now()));
        slot.0 += 1;
        // (Re)stamp on every acquire so a fresh client always resets
        // the stale clock — a second tab opening must not inherit a
        // near-expired stamp from the first.
        slot.1 = Instant::now();
    }

    /// Refresh the stale clock on an existing shell pin (issue #219).
    /// Called by the coord shell bridge on its WS-keepalive interval so
    /// the host's `sweep_stale_shells` can distinguish a live session
    /// from one whose renewer (the coord bridge task) has died. A no-op
    /// if the sandbox has no pin — renewal must never resurrect a pin
    /// that `release_shell`/`clear_shell` already cleared.
    pub fn renew_shell(&self, sandbox_id: SandboxId) {
        if let Some(slot) = self.inner.shell_attached.lock().get_mut(&sandbox_id) {
            slot.1 = Instant::now();
        }
    }

    /// Decrement the shell-attached count for `sandbox_id`. The
    /// sandbox falls back under the normal idle eviction policy once
    /// the count hits zero.
    pub fn release_shell(&self, sandbox_id: SandboxId) {
        let mut map = self.inner.shell_attached.lock();
        if let Some(slot) = map.get_mut(&sandbox_id) {
            slot.0 = slot.0.saturating_sub(1);
            if slot.0 == 0 {
                map.remove(&sandbox_id);
            }
        }
    }

    /// Drop the shell pin for `sandbox_id` outright, ignoring the
    /// refcount (issue #219). Called from the destroy path: once the
    /// sandbox is gone the pin is meaningless, and leaving it behind
    /// leaks an entry forever (the WS bridge for a destroyed sandbox
    /// can never deliver its `ReleaseShell`). Idempotent.
    pub fn clear_shell(&self, sandbox_id: SandboxId) {
        self.inner.shell_attached.lock().remove(&sandbox_id);
    }

    /// Tell the hub that an upcoming harness connection identifying
    /// itself with `session_id` should be routed to `sandbox_id`. The
    /// host-agent calls this when it spawns a harness as part of
    /// `SandboxBackend::create()`. Idempotent; later binds replace
    /// earlier ones (so a re-spawned harness on resume routes to the
    /// new sandbox).
    pub fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        self.inner
            .session_to_sandbox
            .lock()
            .insert(session_id, sandbox_id);
    }

    /// Drop the session→sandbox binding. Called from `destroy()` paths
    /// so a stale TCP connection identifying with the old session_id
    /// can't accidentally attach to a freshly-created replacement
    /// sandbox.
    pub fn unbind_session(&self, session_id: SessionId) {
        self.inner.session_to_sandbox.lock().remove(&session_id);
    }

    /// Accept an anonymous connection from a harness — used by the
    /// TCP listener. Reads `HarnessAttach` from the stream, looks up
    /// `bind_session`'s map for the sandbox_id, then proceeds through
    /// the same handshake-and-loop path as `accept_connection`. If no
    /// binding exists for the announced session_id, sends
    /// `HarnessAttachAck { ok: false }` and closes.
    pub fn accept_via_session_lookup<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_connection_with_session_lookup(inner, stream).await;
        });
    }

    /// Inbound connection from the harness. Spawns a task that runs
    /// the [`HarnessAttach`] handshake, then drives the bidirectional
    /// reader/writer loops until the harness disconnects. Returns
    /// immediately so a caller can accept the next connection without
    /// waiting on this one's handshake.
    ///
    /// `expected_session_id` is checked against the `session_id` in
    /// the harness's HarnessAttach frame. Production wires this from
    /// the HostRegistry mapping; pass `None` in tests to accept any.
    /// On mismatch the host sends `HarnessAttachAck { ok: false }`,
    /// closes, and logs — the connection map stays clean.
    ///
    /// `sandbox_id` is the host-side sandbox identity for the VM the
    /// harness lives in. The hub keys all per-connection state on it.
    pub fn accept_connection<S>(
        &self,
        sandbox_id: SandboxId,
        expected_session_id: Option<SessionId>,
        stream: S,
    ) where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_connection(inner, sandbox_id, expected_session_id, stream).await;
        });
    }

    /// Most recent harness event for `sandbox_id`, if any. Track B
    /// reads this to decide who's idle. `None` means "no events ever
    /// — either the harness never attached, or it just dropped".
    pub fn last_event_at(&self, sandbox_id: SandboxId) -> Option<DateTime<Utc>> {
        self.inner.last_event_at.lock().get(&sandbox_id).copied()
    }

    /// Send a one-shot `Checkpoint { reason }` and wait for ack with
    /// the default timeout. Errors with `CommandTimeout` if the
    /// harness doesn't respond, `NotAttached` if there's no live
    /// connection for `sandbox_id`.
    pub async fn checkpoint(
        &self,
        sandbox_id: SandboxId,
        reason: CheckpointReason,
    ) -> Result<(), HarnessError> {
        let (tx, rx) = oneshot::channel();
        let cmd_tx = {
            let conns = self.inner.connections.lock();
            let conn = conns.get(&sandbox_id).ok_or(HarnessError::NotAttached)?;
            // Only one checkpoint may be in flight at a time.
            let mut pending = conn.pending_checkpoint.lock();
            if pending.is_some() {
                return Err(HarnessError::CheckpointAlreadyInFlight);
            }
            *pending = Some(tx);
            conn.cmd_tx.clone()
        };
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::Checkpoint { reason }))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        match tokio::time::timeout(CHECKPOINT_ACK_TIMEOUT, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(HarnessError::WriterClosed),
            Err(_) => {
                // Clear the pending slot so a follow-up checkpoint can
                // be issued without an `AlreadyInFlight` collision.
                if let Some(conn) = self.inner.connections.lock().get(&sandbox_id) {
                    *conn.pending_checkpoint.lock() = None;
                }
                Err(HarnessError::CommandTimeout)
            }
        }
    }

    /// Send a `Shutdown { grace_secs }`. Doesn't await an ack — the
    /// harness's exit (typically observed by the SandboxBackend's
    /// `destroy` path) is the signal of completion.
    pub async fn shutdown(
        &self,
        sandbox_id: SandboxId,
        grace_secs: u32,
    ) -> Result<(), HarnessError> {
        let cmd_tx = {
            let conns = self.inner.connections.lock();
            conns
                .get(&sandbox_id)
                .ok_or(HarnessError::NotAttached)?
                .cmd_tx
                .clone()
        };
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::Shutdown {
                grace_secs,
            }))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// ADR 0030: send an operator `Interrupt`. The harness SIGINTs its
    /// in-flight child and emits `RunInterrupted` + `Idle`, staying
    /// attached for the next prompt — unlike `shutdown`, the adapter
    /// does NOT exit and the sandbox stays live. `NotAttached` if no
    /// harness is bound. No ack: the `RunInterrupted` event flowing back
    /// up the harness channel is the signal of completion.
    pub async fn interrupt(&self, sandbox_id: SandboxId) -> Result<(), HarnessError> {
        let cmd_tx = {
            let conns = self.inner.connections.lock();
            conns
                .get(&sandbox_id)
                .ok_or(HarnessError::NotAttached)?
                .cmd_tx
                .clone()
        };
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::Interrupt))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// Track A: tell the attached harness to drop its connection and
    /// re-dial in-band (the SIGUSR1 nudge's twin). The re-attach re-emits
    /// `Idle` when idle, resyncing a session whose event stream desynced
    /// from the run state machine — without touching the running agent.
    /// `NotAttached` if no harness is bound. No ack: the fresh attach (and
    /// its re-emitted `Idle`) flowing back up is the signal of completion.
    pub async fn rehandshake(&self, sandbox_id: SandboxId) -> Result<(), HarnessError> {
        let cmd_tx = {
            let conns = self.inner.connections.lock();
            conns
                .get(&sandbox_id)
                .ok_or(HarnessError::NotAttached)?
                .cmd_tx
                .clone()
        };
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::Rehandshake))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// Push a prompt to the running adapter. Adapter starts a
    /// fresh run (or queues if a run is in flight). Atomically
    /// clears `last_idle_at` so the soft idle-eviction TTL doesn't
    /// fire while the adapter is starting Claude.
    ///
    /// Returns `NotAttached` if no harness is bound to `sandbox_id`
    /// (e.g., session is `Idle` and needs auto-resume first — call
    /// `ensure_active` upstream).
    pub async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), HarnessError> {
        // Look up the connection, retrying briefly if it isn't there
        // yet. The post-resume race: `ensure_active` returns once
        // `start_agent` has written the SpawnHarness frame, but
        // agentd's harness supervisor still needs to (a) read the
        // frame, (b) kill the previous adapter, (c) spawn the new
        // one, (d) let the new adapter dial back over vsock and
        // finish HarnessAttach. That's a few hundred ms in practice.
        // Without a wait here, the user's prompt that triggered the
        // resume races the handshake and bounces with NotAttached
        // even though the system is healthy.
        let cmd_tx = {
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(SEND_PROMPT_ATTACH_WAIT_SECS);
            loop {
                // Clone `cmd_tx` out into an owned Option so the
                // `connections` guard is fully released before we touch
                // `last_idle_at`. Holding `connections` across the
                // `last_idle_at.lock()` would invert the canonical lock
                // order (see HubInner docs) versus `idle_sandboxes`,
                // which takes last_idle_at → connections — a classic
                // ABBA deadlock under the prompt/eviction-tick race
                // (issue #217). Never nest these two locks.
                let cmd_tx_opt = self
                    .inner
                    .connections
                    .lock()
                    .get(&sandbox_id)
                    .map(|h| h.cmd_tx.clone());
                if let Some(cmd_tx) = cmd_tx_opt {
                    self.inner.last_idle_at.lock().remove(&sandbox_id);
                    break cmd_tx;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(HarnessError::NotAttached);
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::Prompt {
                prompt_id,
                text,
            }))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// Phase 1b: forward a queue-mutation command (Edit/Dequeue) to the
    /// attached harness. Unlike `send_prompt` it neither waits/retries for
    /// attach nor clears `last_idle_at` — the target prompt was already
    /// queued (so the harness is attached); if it isn't, the prompt is
    /// gone and `NotAttached` is the correct answer.
    async fn send_queue_command(
        &self,
        sandbox_id: SandboxId,
        cmd: HarnessCommand,
    ) -> Result<(), HarnessError> {
        let cmd_tx = self
            .inner
            .connections
            .lock()
            .get(&sandbox_id)
            .map(|h| h.cmd_tx.clone())
            .ok_or(HarnessError::NotAttached)?;
        cmd_tx
            .send(HarnessFrame::Command(cmd))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// Phase 1b: edit a still-queued type-ahead prompt by its `prompt_id`.
    pub async fn edit_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), HarnessError> {
        self.send_queue_command(sandbox_id, HarnessCommand::EditQueued { prompt_id, text })
            .await
    }

    /// Phase 1b: remove a still-queued type-ahead prompt by its `prompt_id`.
    pub async fn dequeue_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
    ) -> Result<(), HarnessError> {
        self.send_queue_command(sandbox_id, HarnessCommand::DequeueQueued { prompt_id })
            .await
    }

    /// Number of currently-attached harnesses. Diagnostic / test helper.
    pub fn attached_count(&self) -> usize {
        self.inner.connections.lock().len()
    }

    /// Sandboxes due for idle eviction under the two-tier policy.
    ///
    /// Returns `(SessionId, SandboxId)` pairs that satisfy EITHER:
    /// - **Soft TTL fired:** `last_idle_at` is older than
    ///   `soft_ttl`. The agent emitted `Idle` (= "awaiting user
    ///   input") and stayed quiet that long. Normal "user afk"
    ///   case; Idle eviction frees the host slot.
    /// - **Hard TTL fired:** `last_event_at` is older than
    ///   `hard_ttl`. The adapter went silent without ever emitting
    ///   `Idle` — stuck in a tool call, infinite loop, etc. Backstop
    ///   so a buggy adapter can't pin a sandbox forever.
    ///
    /// A long-running tool call (e.g. 5-minute pytest) doesn't
    /// trip the soft TTL — `last_idle_at` is None during the call.
    /// `hard_ttl` is the operator's safety net; default 30 min.
    ///
    /// Only returns sandboxes with an attached harness; bare
    /// sandboxes (no adapter) aren't tracked here — deliberately.
    /// A running-but-detached sandbox (harness vsock dropped, hub
    /// maps wiped at reader-loop exit) is caught by the coord's
    /// PG-derived detection backstop (ADR 0034,
    /// `engram_coordinator::idle_detect_backstop`), which reads the
    /// durable session_events record instead of this in-memory view.
    pub fn idle_sandboxes(
        &self,
        soft_ttl: std::time::Duration,
        hard_ttl: std::time::Duration,
    ) -> Vec<(SessionId, SandboxId)> {
        let now = Utc::now();
        let soft_cutoff = now
            - chrono::Duration::from_std(soft_ttl).unwrap_or_else(|_| chrono::Duration::seconds(0));
        let hard_cutoff = now
            - chrono::Duration::from_std(hard_ttl).unwrap_or_else(|_| chrono::Duration::seconds(0));
        let event_at = self.inner.last_event_at.lock();
        let idle_at = self.inner.last_idle_at.lock();
        let conns = self.inner.connections.lock();
        let shell = self.inner.shell_attached.lock();
        // ADR 0016 §A.1.5a: skip sandboxes whose prior eviction POST
        // is still in flight on coord. Without this, the 10s tick
        // re-includes them while coord is still running the prior
        // eviction's snapshot pipeline (typically 30s+), which
        // produced the prod retry storm on 2026-05-24.
        let inflight = self.inner.eviction_inflight.lock();
        let mut out = Vec::new();
        for (sandbox_id, handle) in conns.iter() {
            // Browser-shell connections express explicit "user is
            // poking at this session" intent. Suppress eviction while
            // any shell is open — the count drops to zero (and the
            // sandbox falls back under the normal TTLs) when the
            // user closes the tab or otherwise drops the WebSocket.
            if shell.contains_key(sandbox_id) {
                continue;
            }
            if inflight.contains_key(sandbox_id) {
                continue;
            }
            let soft = idle_at
                .get(sandbox_id)
                .map(|at| *at <= soft_cutoff)
                .unwrap_or(false);
            let hard = event_at
                .get(sandbox_id)
                .map(|at| *at <= hard_cutoff)
                .unwrap_or(false);
            if soft || hard {
                out.push((handle.session_id, *sandbox_id));
            }
        }
        out
    }

    /// ADR 0016 §A.1.5a: mark a sandbox as having an eviction POST
    /// currently in flight on coord. Idempotent (re-marking refreshes
    /// the Instant — used by the stale sweep to detect wedged
    /// spawned tasks).
    pub fn mark_eviction_inflight(&self, sandbox_id: SandboxId) {
        self.inner
            .eviction_inflight
            .lock()
            .insert(sandbox_id, Instant::now());
    }

    /// ADR 0016 §A.1.5a: clear the eviction-in-flight marker. Called
    /// by the fire-and-forget spawned task in its finally block
    /// regardless of POST outcome (success, transport error, timeout).
    /// Idempotent; safe to call on an already-cleared entry.
    pub fn clear_eviction_inflight(&self, sandbox_id: SandboxId) {
        self.inner.eviction_inflight.lock().remove(&sandbox_id);
    }

    /// ADR 0016 §A.1.5a: sweep entries older than `max_age` and
    /// return the sandbox_ids removed. Called at the top of each
    /// eviction tick so a spawned task that wedged (e.g. reqwest
    /// future hung forever) can't permanently block re-eviction of
    /// the sandbox.
    pub fn sweep_stale_evictions(&self, max_age: Duration) -> Vec<SandboxId> {
        let mut guard = self.inner.eviction_inflight.lock();
        let now = Instant::now();
        let mut removed = Vec::new();
        guard.retain(|sandbox_id, marked_at| {
            if now.duration_since(*marked_at) >= max_age {
                removed.push(*sandbox_id);
                false
            } else {
                true
            }
        });
        removed
    }

    /// Issue #219: reap shell pins not renewed within `max_age` and
    /// return the sandbox_ids removed. Direct analogue of
    /// `sweep_stale_evictions` for the `shell_attached` map. A live
    /// coord shell bridge renews its pin every
    /// [`SHELL_PIN_RENEW_INTERVAL`]; a pin older than `max_age`
    /// (typically [`SHELL_PIN_STALE_AGE`]) means the renewer is gone —
    /// coord pod died mid-session, or the bridge task was torn down
    /// without sending `ReleaseShell` — so the sandbox must fall back
    /// under the normal idle TTLs instead of staying pinned forever.
    /// Called from the host's eviction tick.
    pub fn sweep_stale_shells(&self, max_age: Duration) -> Vec<SandboxId> {
        let mut guard = self.inner.shell_attached.lock();
        let now = Instant::now();
        let mut removed = Vec::new();
        guard.retain(|sandbox_id, (_count, last_renewed)| {
            if now.duration_since(*last_renewed) >= max_age {
                removed.push(*sandbox_id);
                false
            } else {
                true
            }
        });
        removed
    }
}

async fn run_connection<S>(
    inner: Arc<HubInner>,
    sandbox_id: SandboxId,
    expected_session_id: Option<SessionId>,
    stream: S,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);

    let attach: HarnessAttach = match read_msg(&mut reader).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(error = %e, sandbox_id = %sandbox_id, "harness handshake read failed");
            return;
        }
    };
    if let Some(expected) = expected_session_id {
        if attach.session_id != expected {
            let ack = HarnessAttachAck {
                ok: false,
                message: Some("session_id mismatch".into()),
            };
            let _ = write_msg(&mut writer, &ack).await;
            tracing::warn!(
                expected = %expected,
                got = %attach.session_id,
                sandbox_id = %sandbox_id,
                "harness attach rejected: session_id mismatch",
            );
            return;
        }
    }
    drive_attached(inner, sandbox_id, attach, reader, writer).await;
}

/// TCP-listener path: read HarnessAttach, look up the bound
/// sandbox_id (set by `bind_session()` when the host-agent spawns
/// the harness), then run the same post-attach loop as
/// `run_connection`. Closes with `ok: false` if no binding exists.
async fn run_connection_with_session_lookup<S>(inner: Arc<HubInner>, stream: S)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let attach: HarnessAttach = match read_msg(&mut reader).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(error = %e, "harness handshake read failed (no session bound)");
            return;
        }
    };
    let bound = {
        // Scope the guard so it drops before the AsyncWrite below.
        // parking_lot guards aren't Send, and the writer may be
        // awaited across threads via tokio::spawn.
        inner
            .session_to_sandbox
            .lock()
            .get(&attach.session_id)
            .copied()
    };
    let sandbox_id = match bound {
        Some(id) => id,
        None => {
            let ack = HarnessAttachAck {
                ok: false,
                message: Some("no sandbox bound to this session_id".into()),
            };
            let _ = write_msg(&mut writer, &ack).await;
            tracing::warn!(
                session_id = %attach.session_id,
                "harness attach rejected: no sandbox bound",
            );
            return;
        }
    };
    drive_attached(inner, sandbox_id, attach, reader, writer).await;
}

/// Shared post-attach work: send `ok: true`, register the
/// connection, run reader+writer loops, clean up. Both
/// `run_connection` and `run_connection_with_session_lookup`
/// dispatch into here once they've resolved sandbox_id.
async fn drive_attached<R, W>(
    inner: Arc<HubInner>,
    sandbox_id: SandboxId,
    attach: HarnessAttach,
    reader: R,
    mut writer: W,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let ack = HarnessAttachAck {
        ok: true,
        message: None,
    };
    if let Err(e) = write_msg(&mut writer, &ack).await {
        tracing::debug!(error = %e, "harness ack write failed");
        return;
    }

    tracing::debug!(
        session_id = %attach.session_id,
        sandbox_id = %sandbox_id,
        harness_version = %attach.harness_version,
        "harness attached",
    );

    let (cmd_tx, cmd_rx) = mpsc::channel::<HarnessFrame>(32);
    // Issue #218: stamp this connection with a per-hub-unique
    // generation BEFORE the insert. On reconnect (agentd kills the old
    // adapter, the new one redials over vsock) two `drive_attached`
    // tasks race on the same `sandbox_id`: the new one's `insert`
    // replaces the old handle, and the old one's reader then hits EOF
    // and runs its teardown epilogue. Without an identity check that
    // epilogue would unconditionally remove the *new* registration,
    // leaving a live-but-unregistered adapter (prompts bounce
    // NotAttached, the writer half is torn down, the session vanishes
    // from idle detection). The generation lets the epilogue remove
    // only when it still owns the live entry.
    let my_generation = inner.next_gen.fetch_add(1, Ordering::Relaxed);
    let handle = ConnectionHandle {
        cmd_tx,
        pending_checkpoint: Mutex::new(None),
        session_id: attach.session_id,
        generation: my_generation,
    };
    inner.connections.lock().insert(sandbox_id, handle);
    // Seed only `last_event_at` at attach — the hard TTL needs an
    // origin so a never-emits-anything adapter still gets reaped
    // eventually. `last_idle_at` is **deliberately not seeded**:
    // soft TTL must mean "harness explicitly emitted Idle and has
    // been quiet since", not "harness attached and is still booting
    // its agent". Every harness in our tree emits `HarnessEvent::
    // Idle` itself when it has no work
    // (`engram-harness-claude` line ~326 when `next_prompt.is_none()`;
    // `engram-harness-noop` at the end of its scripted run), so the
    // "attached and awaiting Prompt" case is covered by the harness
    // contract, not the hub.
    //
    // Prod-found 2026-05-28 against session `43fe13b4`: the old
    // seed-at-attach made the soft TTL fire at T+30s on every fresh
    // session whose harness needed >30s to emit its first event —
    // claude's ~7-9s cold-boot plus first-prompt processing easily
    // crosses that line. The eviction pause-suspended agentd, which
    // surfaced to the user as "no harness response + shell timeout".
    let now = Utc::now();
    inner.last_event_at.lock().insert(sandbox_id, now);

    let writer_task = tokio::spawn(writer_loop(writer, cmd_rx));
    let reader_outcome = reader_loop(reader, &inner, attach.session_id, sandbox_id).await;
    // Issue #218: guarded teardown. Only remove the per-sandbox entries
    // if the live `connections` registration is STILL ours — i.e. no
    // newer connection has replaced us under the same `sandbox_id`. A
    // reconnect inserts a fresh handle with a higher generation; if we
    // see a different generation (or no entry), a newer connection now
    // owns these entries and we must leave all three untouched.
    //
    // We acquire all three locks in the canonical order (last_event_at
    // → last_idle_at → connections; see HubInner docs / issue #217) and
    // hold them across the compare-and-remove so the decision and the
    // removals are atomic versus a concurrent attach/eviction tick.
    {
        let mut event_at = inner.last_event_at.lock();
        let mut idle_at = inner.last_idle_at.lock();
        let mut conns = inner.connections.lock();
        let still_ours = conns
            .get(&sandbox_id)
            .map(|h| h.generation == my_generation)
            .unwrap_or(false);
        if still_ours {
            conns.remove(&sandbox_id);
            event_at.remove(&sandbox_id);
            idle_at.remove(&sandbox_id);
        }
    }
    let _ = writer_task.await;
    if let Err(e) = reader_outcome {
        tracing::debug!(
            session_id = %attach.session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "harness reader loop ended",
        );
    }
}

async fn reader_loop<R>(
    mut reader: R,
    hub: &HubInner,
    session_id: SessionId,
    sandbox_id: SandboxId,
) -> Result<(), HarnessError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let frame: HarnessFrame = match read_msg(&mut reader).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Clean disconnect — harness closed its end.
                return Ok(());
            }
            Err(e) => return Err(HarnessError::Io(e)),
        };
        match frame {
            HarnessFrame::Event(ev) => {
                let now = Utc::now();
                hub.last_event_at.lock().insert(sandbox_id, now);
                // Soft TTL bookkeeping: Idle SETS last_idle_at;
                // anything else CLEARS it. A long tool call has
                // ToolCallStarted recently and no Idle, so soft
                // doesn't trip; the agent finishes, emits Idle,
                // soft starts ticking from there.
                match &ev {
                    HarnessEvent::Idle => {
                        hub.last_idle_at.lock().insert(sandbox_id, now);
                    }
                    _ => {
                        hub.last_idle_at.lock().remove(&sandbox_id);
                    }
                }
                let fut = (hub.event_sink)(session_id, sandbox_id, ev);
                fut.await;
            }
            HarnessFrame::Command(_) => {
                // Commands flow host → harness. A frame coming the
                // wrong way is the harness's bug; ack any pending
                // checkpoint as if it succeeded so we don't deadlock,
                // but log loudly. (Strict alternative: hard-error and
                // close the connection. We choose lenient because the
                // harness is third-party-ish — adapter authors may
                // misuse the type.)
                tracing::warn!(
                    session_id = %session_id,
                    sandbox_id = %sandbox_id,
                    "harness sent a Command frame; ignoring",
                );
            }
        }
    }
}

async fn writer_loop<W>(mut writer: W, mut cmd_rx: mpsc::Receiver<HarnessFrame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = cmd_rx.recv().await {
        if let Err(e) = write_msg(&mut writer, &frame).await {
            tracing::debug!(error = %e, "harness writer task exiting");
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Build an [`EventSink`] that pushes events into the
/// coordinator's `session_events` log via the provided
/// `emit` closure. Production wiring; tests typically construct
/// their own sink.
///
/// `emit` should be cheap to clone (e.g. `Arc<AppState>` capturing).
pub fn event_sink_to<F, Fut>(emit: F) -> EventSink
where
    F: Fn(SessionId, SandboxId, HarnessEvent) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Arc::new(move |session_id, sandbox_id, ev| {
        let fut = emit(session_id, sandbox_id, ev);
        Box::new(Box::pin(fut))
    })
}

#[derive(Debug)]
pub enum HarnessError {
    Io(std::io::Error),
    SessionMismatch { expected: SessionId, got: SessionId },
    NotAttached,
    CheckpointAlreadyInFlight,
    CommandTimeout,
    WriterClosed,
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "harness io: {e}"),
            Self::SessionMismatch { expected, got } => {
                write!(
                    f,
                    "harness attached to session {got} but expected {expected}"
                )
            }
            Self::NotAttached => write!(f, "no harness attached for this sandbox"),
            Self::CheckpointAlreadyInFlight => write!(f, "another checkpoint is already in flight"),
            Self::CommandTimeout => write!(f, "harness did not ack command in time"),
            Self::WriterClosed => write!(f, "harness writer closed"),
        }
    }
}

impl std::error::Error for HarnessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Spawn a TCP listener that hands every accepted connection to
/// `hub.accept_via_session_lookup`. Returns the bound address (so
/// callers using port 0 can read back the OS-assigned port to
/// publish into `AgentSpec::env`) and a `JoinHandle` for the accept
/// loop.
///
/// Production deployments would put this behind a vsock listener
/// instead — TCP is fine for ProcessBackend dev mode where the
/// "guest" is just a host child process.
pub async fn spawn_tcp_listener(
    hub: HarnessHub,
    addr: std::net::SocketAddr,
) -> std::io::Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(addr = %bound, "harness TCP listener bound");
    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    tracing::debug!(peer = %peer, "harness connection accepted");
                    let _ = stream.set_nodelay(true);
                    hub.accept_via_session_lookup(stream);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "harness listener accept failed");
                    // Brief pause before retrying so a wedged listener
                    // doesn't burn CPU. Practical accept errors on a
                    // bound TCP socket are rare; mostly EMFILE under
                    // fd exhaustion.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok((bound, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_harness_proto::HarnessAttach;
    use parking_lot::Mutex as PlMutex;
    use std::time::Duration;
    use tokio::io::DuplexStream;

    fn collecting_sink() -> (EventSink, Arc<PlMutex<Vec<HarnessEvent>>>) {
        let collected: Arc<PlMutex<Vec<HarnessEvent>>> = Arc::new(PlMutex::new(Vec::new()));
        let collected_for_sink = collected.clone();
        let sink: EventSink = Arc::new(move |_session_id, _sandbox_id, ev| {
            let collected = collected_for_sink.clone();
            Box::new(Box::pin(async move {
                collected.lock().push(ev);
            }))
        });
        (sink, collected)
    }

    /// Convenience: build a paired in-memory stream for harness ↔ host.
    /// Returns (host_side, harness_side). Either end can read/write.
    fn duplex_pair() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(1 << 16)
    }

    /// Drives the harness side of a connection: sends `HarnessAttach`,
    /// reads ack, then runs `body` with read/write handles to the host.
    async fn drive_harness<F, Fut>(
        harness_side: DuplexStream,
        session_id: SessionId,
        body: F,
    ) -> HarnessAttachAck
    where
        F: FnOnce(tokio::io::ReadHalf<DuplexStream>, tokio::io::WriteHalf<DuplexStream>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let (mut harness_r, mut harness_w) = tokio::io::split(harness_side);
        write_msg(
            &mut harness_w,
            &HarnessAttach {
                session_id,
                harness_version: "test/0.1".into(),
            },
        )
        .await
        .expect("attach");
        let ack: HarnessAttachAck = read_msg(&mut harness_r).await.expect("ack");
        if ack.ok {
            body(harness_r, harness_w).await;
        }
        ack
    }

    /// Wait for a condition with a 1s deadline. Avoids racing the
    /// spawned reader/writer task against test assertions.
    async fn wait_until<F: FnMut() -> bool>(mut cond: F) -> bool {
        for _ in 0..100 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn attach_then_event_flows_into_sink() {
        let (sink, collected) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let ack = drive_harness(harness_side, session_id, |_r, mut w| async move {
            write_msg(
                &mut w,
                &HarnessFrame::Event(HarnessEvent::ToolCallCompleted {
                    run_id: "r1".into(),
                    tool_call_id: "t1".into(),
                    tool_name: "Bash".into(),
                    ok: true,
                    duration_ms: 1,
                    result_summary: Some("line".into()),
                }),
            )
            .await
            .unwrap();
            // Hold the connection open long enough for the host to
            // process the event before the test exits and tears
            // everything down.
            tokio::time::sleep(Duration::from_millis(50)).await;
        })
        .await;
        assert!(ack.ok);

        assert!(
            wait_until(|| !collected.lock().is_empty()).await,
            "expected at least one event in the sink",
        );
        let events = collected.lock();
        match &events[0] {
            HarnessEvent::ToolCallCompleted { tool_name, .. } => assert_eq!(tool_name, "Bash"),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(hub.last_event_at(sandbox_id).is_some());
    }

    #[tokio::test]
    async fn session_id_mismatch_closes_connection_cleanly() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let expected = SessionId::new();
        let attached_with = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(expected), host_side);

        let ack = drive_harness(harness_side, attached_with, |_r, _w| async {}).await;
        assert!(!ack.ok, "host must reject mismatched session_id");

        // Mismatch leaves the maps untouched.
        assert!(
            wait_until(|| hub.attached_count() == 0).await,
            "no entry should be added on a rejected attach"
        );
    }

    #[tokio::test]
    async fn shutdown_command_reaches_harness() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Drive the harness side: do the attach handshake, then
        // wait to read one Shutdown command.
        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            let frame: HarnessFrame = read_msg(&mut hr).await.unwrap();
            matches!(
                frame,
                HarnessFrame::Command(HarnessCommand::Shutdown { .. })
            )
        });

        // Wait for the host to register the connection before we
        // try to send a command (otherwise NotAttached races the
        // handshake).
        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach within the 1s deadline"
        );

        hub.shutdown(sandbox_id, 1).await.expect("shutdown");
        let received = harness_task.await.unwrap();
        assert!(received, "harness should receive a Shutdown frame");
    }

    #[tokio::test]
    async fn interrupt_command_reaches_harness() {
        // ADR 0030: hub.interrupt() must deliver a HarnessCommand::Interrupt
        // to the attached harness (which SIGINTs its child + emits
        // RunInterrupted). Mirrors shutdown_command_reaches_harness; the
        // SIGINT-the-claude-child leaf is Linux + claude-runtime specific
        // and is covered by the manual spike documented in the ADR.
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            let frame: HarnessFrame = read_msg(&mut hr).await.unwrap();
            matches!(frame, HarnessFrame::Command(HarnessCommand::Interrupt))
        });

        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach within the 1s deadline"
        );

        hub.interrupt(sandbox_id).await.expect("interrupt");
        let received = harness_task.await.unwrap();
        assert!(received, "harness should receive an Interrupt frame");
    }

    #[tokio::test]
    async fn interrupt_returns_not_attached_for_unknown_sandbox() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let err = hub.interrupt(SandboxId::new()).await.unwrap_err();
        assert!(matches!(err, HarnessError::NotAttached));
    }

    #[tokio::test]
    async fn rehandshake_command_reaches_harness() {
        // Track A: hub.rehandshake() must deliver a
        // HarnessCommand::Rehandshake to the attached harness (which drops
        // + re-dials in-band, re-emitting Idle). The connection-layer
        // drop/re-dial leaf is exercised by the engram-harness-claude
        // forward_commands path.
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            let frame: HarnessFrame = read_msg(&mut hr).await.unwrap();
            matches!(frame, HarnessFrame::Command(HarnessCommand::Rehandshake))
        });

        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach within the 1s deadline"
        );

        hub.rehandshake(sandbox_id).await.expect("rehandshake");
        let received = harness_task.await.unwrap();
        assert!(received, "harness should receive a Rehandshake frame");
    }

    #[tokio::test]
    async fn rehandshake_returns_not_attached_for_unknown_sandbox() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let err = hub.rehandshake(SandboxId::new()).await.unwrap_err();
        assert!(matches!(err, HarnessError::NotAttached));
    }

    #[tokio::test]
    async fn checkpoint_returns_not_attached_for_unknown_sandbox() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let err = hub
            .checkpoint(SandboxId::new(), CheckpointReason::Idle)
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::NotAttached));
    }

    #[tokio::test]
    async fn idle_sandboxes_returns_pairs_past_ttl() {
        // Soft TTL fires only after the harness has explicitly
        // emitted `Idle`. Prod-found 2026-05-28: previously the hub
        // seeded `last_idle_at` at attach time, which made the soft
        // TTL fire 30 s after attach on every fresh session whose
        // harness was still cold-booting. Now the soft TTL is purely
        // event-driven — the test drives a real Idle frame before
        // asserting it fires.
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Drive the harness side: attach, emit Idle, then keep the
        // connection alive without further events.
        let _harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            write_msg(&mut hw, &HarnessFrame::Event(HarnessEvent::Idle))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach"
        );

        // Fresh attach with no Idle yet: long TTLs see no idle
        // sandbox AND short soft TTL also sees none (we did not
        // seed `last_idle_at` at attach — that's the bug we fixed).
        // Hard TTL still has its origin from attach so a zero hard
        // TTL would fire; test that separately below.
        let fresh = hub.idle_sandboxes(Duration::from_secs(60), Duration::from_secs(3600));
        assert!(fresh.is_empty(), "fresh attach is not idle: {fresh:?}");

        // Wait for the harness's Idle frame to be processed.
        assert!(
            wait_until(|| hub.last_event_at(sandbox_id).is_some()).await,
            "Idle event should reach the hub",
        );

        // Now the soft TTL has its anchor. Zero soft TTL fires.
        let aged = hub.idle_sandboxes(Duration::from_millis(0), Duration::from_secs(3600));
        assert_eq!(aged.len(), 1);
        assert_eq!(aged[0].0, session_id);
        assert_eq!(aged[0].1, sandbox_id);
    }

    #[tokio::test]
    async fn soft_ttl_does_not_fire_before_explicit_idle_emit() {
        // Prod regression guard (2026-05-28 session 43fe13b4):
        // the hub previously seeded `last_idle_at` at attach time
        // so the soft TTL fired at attach + soft_ttl seconds, even
        // if the harness had never emitted `Idle`. The result in
        // prod: every fresh agent session whose harness took more
        // than the soft TTL to produce its first event got hot-
        // suspended mid-cold-boot.
        //
        // This test reproduces the regression with `soft_ttl = 0ms`,
        // `hard_ttl = 1h`: with the seed in place, the zero soft TTL
        // would immediately fire because `last_idle_at` is the
        // attach instant (`attach_instant <= now - 0ms` is true).
        // With the fix, `last_idle_at` is None for a not-yet-Idled
        // harness, so the soft check short-circuits to false and
        // the result is empty.
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let _harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            // Stay attached but never emit anything. Mirrors a
            // harness in the middle of its agent-cold-boot window
            // (claude takes ~7-9s before its first event).
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        assert!(wait_until(|| hub.attached_count() == 1).await);

        // Zero soft TTL + long hard TTL. With the seed-at-attach bug,
        // soft fires immediately (the assertion below trips with a
        // non-empty result). Without the bug, soft check returns
        // false because `last_idle_at` is None.
        let result = hub.idle_sandboxes(Duration::from_millis(0), Duration::from_secs(3600));
        assert!(
            result.is_empty(),
            "soft TTL must not fire before the harness emits Idle; \
             got {result:?} — this regression caused the prod \
             session 43fe13b4 hot-suspension on 2026-05-28",
        );
    }

    #[tokio::test]
    async fn hard_ttl_fires_on_silent_adapter_without_idle() {
        // The companion case to the soft-TTL fix above. An adapter
        // that attaches and never emits *anything* (buggy harness,
        // stuck in cold boot indefinitely) must still get reaped —
        // that's what the hard TTL exists for. We seed
        // `last_event_at` at attach so its clock is the attach
        // moment, even though `last_idle_at` is left None.
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let _harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            // Never emit any frame; just keep the connection alive.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        assert!(wait_until(|| hub.attached_count() == 1).await);

        // Long soft TTL alone sees nothing — `last_idle_at` is None.
        let none = hub.idle_sandboxes(Duration::from_secs(3600), Duration::from_secs(3600));
        assert!(none.is_empty(), "no idle emit + long TTLs: {none:?}");

        // Zero hard TTL with `last_event_at` seeded at attach fires.
        let aged = hub.idle_sandboxes(Duration::from_secs(3600), Duration::from_millis(0));
        assert_eq!(aged.len(), 1);
        assert_eq!(aged[0].0, session_id);
        assert_eq!(aged[0].1, sandbox_id);
    }

    #[tokio::test]
    async fn long_tool_call_does_not_trip_soft_ttl() {
        // The motivating case for the two-tier rewrite: an adapter
        // emits ToolCallStarted, runs a 5-min pytest, and emits
        // ToolCallCompleted. The soft TTL must NOT fire on
        // last_event_at staleness — only `last_idle_at`.
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let _harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            // Emit ToolCallStarted, then sit silent.
            write_msg(
                &mut hw,
                &HarnessFrame::Event(HarnessEvent::ToolCallStarted {
                    run_id: "r".into(),
                    tool_call_id: "t".into(),
                    tool_name: "Bash".into(),
                    args_summary: Some("sleep 300".into()),
                }),
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        assert!(wait_until(|| hub.attached_count() == 1).await);
        // Wait for ToolCallStarted to be processed.
        assert!(wait_until(|| hub.last_event_at(sandbox_id).is_some()).await);

        // Soft TTL of 0ms — should NOT fire (last_idle_at was cleared
        // by the ToolCallStarted event). Hard TTL of 1h — also not.
        let none = hub.idle_sandboxes(Duration::from_millis(0), Duration::from_secs(3600));
        assert!(
            none.is_empty(),
            "tool-call-in-flight must not be idle: {none:?}",
        );
    }

    #[tokio::test]
    async fn harness_disconnect_clears_last_event_at_and_attached_count() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Drive the harness: attach, send one event, drop.
        let h = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            write_msg(&mut hw, &HarnessFrame::Event(HarnessEvent::Idle))
                .await
                .unwrap();
            // dropping `hw`/`hr` closes the harness side
        });
        let _ = h.await;

        assert!(
            wait_until(|| hub.attached_count() == 0).await,
            "connection map should clear on EOF",
        );
        assert!(
            hub.last_event_at(sandbox_id).is_none(),
            "last_event_at cleared on disconnect",
        );
    }

    #[tokio::test]
    async fn tcp_listener_routes_attach_via_session_lookup_to_bound_sandbox() {
        // End-to-end demo wiring: bind a (session_id, sandbox_id)
        // pair, spawn a real TCP listener, connect a harness client
        // over a real TCP socket, observe an event arrive at the
        // collecting sink keyed on the *bound* sandbox_id.
        let (sink, collected) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        hub.bind_session(session_id, sandbox_id);

        let (addr, _listener_task) =
            spawn_tcp_listener(hub.clone(), "127.0.0.1:0".parse().unwrap())
                .await
                .expect("listener bound");

        // Harness side: dial the listener, send Attach, await ack,
        // emit one event, then drop.
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        write_msg(
            &mut conn,
            &HarnessAttach {
                session_id,
                harness_version: "tcp-test/0.1".into(),
            },
        )
        .await
        .unwrap();
        let ack: HarnessAttachAck = read_msg(&mut conn).await.unwrap();
        assert!(ack.ok, "attach should succeed when session is bound");
        write_msg(&mut conn, &HarnessFrame::Event(HarnessEvent::Idle))
            .await
            .unwrap();
        drop(conn);

        // The collecting sink should observe the Idle event for the
        // bound sandbox_id. Poll briefly — the event arrives async.
        assert!(
            wait_until(|| !collected.lock().is_empty()).await,
            "should have collected one event",
        );
        assert!(matches!(collected.lock()[0], HarnessEvent::Idle));
    }

    #[tokio::test]
    async fn tcp_listener_rejects_attach_for_unbound_session() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);

        let (addr, _listener_task) =
            spawn_tcp_listener(hub.clone(), "127.0.0.1:0".parse().unwrap())
                .await
                .expect("listener bound");

        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        write_msg(
            &mut conn,
            &HarnessAttach {
                session_id: SessionId::new(), // not bound
                harness_version: "tcp-test/0.1".into(),
            },
        )
        .await
        .unwrap();
        let ack: HarnessAttachAck = read_msg(&mut conn).await.unwrap();
        assert!(!ack.ok, "unbound session should be rejected");
    }

    // ADR 0016 §A.1.5a — eviction in-flight gate -----------------

    fn noop_sink() -> EventSink {
        Arc::new(|_, _, _| Box::new(Box::pin(async {})))
    }

    #[tokio::test]
    async fn idle_sandboxes_skips_evictions_in_flight() {
        // Attach a sandbox, mark its last_idle_at to a value past the
        // soft TTL, then mark it as "eviction in flight". `idle_sandboxes`
        // should NOT return it. Clearing the marker un-suppresses.
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_side);
        let _ack = drive_harness(harness_side, session_id, |_r, _w| async {}).await;

        // Force the soft-idle bookkeeping past the TTL.
        hub.inner
            .last_idle_at
            .lock()
            .insert(sandbox_id, Utc::now() - chrono::Duration::seconds(120));

        // Sanity: without the gate, this sandbox WOULD be returned.
        let before = hub.idle_sandboxes(Duration::from_secs(30), Duration::from_secs(1800));
        assert_eq!(before.len(), 1, "soft TTL should have fired");
        assert_eq!(before[0].1, sandbox_id);

        // Mark eviction in flight; same call now returns nothing.
        hub.mark_eviction_inflight(sandbox_id);
        let during = hub.idle_sandboxes(Duration::from_secs(30), Duration::from_secs(1800));
        assert!(
            during.is_empty(),
            "eviction-in-flight should suppress the candidate; got {during:?}",
        );

        // Clear; sandbox re-appears as a candidate.
        hub.clear_eviction_inflight(sandbox_id);
        let after = hub.idle_sandboxes(Duration::from_secs(30), Duration::from_secs(1800));
        assert_eq!(
            after.len(),
            1,
            "clearing the marker should re-expose the candidate"
        );
    }

    #[tokio::test]
    async fn sweep_stale_evictions_reaps_old_entries() {
        // Mark two sandboxes as in-flight, force one to be "old", run
        // the sweep with a short max_age, assert only the old one is
        // reaped and the still-fresh one is preserved.
        let hub = HarnessHub::new(noop_sink());
        let fresh = SandboxId::new();
        let stale = SandboxId::new();
        hub.mark_eviction_inflight(fresh);
        hub.mark_eviction_inflight(stale);

        // Backdate the stale entry's Instant. `Instant` doesn't have a
        // public "set to past" API, but we can reach into the inner
        // mutex to swap the value (the field is private; this is in-
        // crate code so the test can access it).
        {
            let mut guard = hub.inner.eviction_inflight.lock();
            let old = Instant::now()
                .checked_sub(Duration::from_secs(300))
                .unwrap_or_else(Instant::now);
            guard.insert(stale, old);
        }

        let reaped = hub.sweep_stale_evictions(Duration::from_secs(180));
        assert_eq!(reaped, vec![stale], "only the stale entry should be reaped");

        // Fresh entry survives.
        assert!(
            hub.inner.eviction_inflight.lock().contains_key(&fresh),
            "fresh entry must survive the sweep",
        );
        // Stale entry is gone.
        assert!(
            !hub.inner.eviction_inflight.lock().contains_key(&stale),
            "stale entry must be removed",
        );
    }

    #[test]
    fn mark_eviction_inflight_is_idempotent() {
        // Re-marking the same sandbox refreshes the Instant (used by
        // the stale sweep to defer the deadline). Subsequent clears
        // remove the single entry.
        let hub = HarnessHub::new(noop_sink());
        let sb = SandboxId::new();
        hub.mark_eviction_inflight(sb);
        let first = *hub.inner.eviction_inflight.lock().get(&sb).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        hub.mark_eviction_inflight(sb);
        let second = *hub.inner.eviction_inflight.lock().get(&sb).unwrap();
        assert!(
            second > first,
            "re-marking must refresh the Instant (second={second:?} first={first:?})",
        );
        hub.clear_eviction_inflight(sb);
        assert!(
            !hub.inner.eviction_inflight.lock().contains_key(&sb),
            "clear must remove the entry",
        );
    }

    // Issue #219 — shell-pin keep-alive leak hardening --------------

    /// Core regression for issue #219: a shell pin whose coord-side
    /// renewer has died (no `release_shell`, no `renew_shell`) must be
    /// reaped by the host's stale sweep so the sandbox falls back under
    /// idle eviction — including the hard-TTL backstop. Without the fix,
    /// `shell_attached` had no timestamp and no sweep, so the pin (and
    /// the unconditional `continue` in `idle_sandboxes`) stuck forever.
    #[tokio::test]
    async fn stale_shell_pin_is_reaped_and_sandbox_becomes_evictable() {
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_side);
        let _ack = drive_harness(harness_side, session_id, |_r, _w| async {}).await;

        // Push the sandbox past the soft TTL so it WOULD be a candidate.
        hub.inner
            .last_idle_at
            .lock()
            .insert(sandbox_id, Utc::now() - chrono::Duration::seconds(120));

        // Open a shell: the pin now suppresses eviction.
        hub.acquire_shell(sandbox_id);
        let pinned = hub.idle_sandboxes(Duration::from_secs(30), Duration::from_secs(1800));
        assert!(
            pinned.is_empty(),
            "an attached shell must suppress idle eviction; got {pinned:?}",
        );

        // Simulate the coord bridge dying: the pin stops being renewed.
        // Backdate the stamp past the stale window (the count stays ≥1 —
        // this is exactly the leaked state the bug produced).
        {
            let mut guard = hub.inner.shell_attached.lock();
            let entry = guard.get_mut(&sandbox_id).expect("pin present");
            entry.1 = Instant::now()
                .checked_sub(SHELL_PIN_STALE_AGE + Duration::from_secs(60))
                .unwrap_or_else(Instant::now);
            assert!(entry.0 >= 1, "count must stay non-zero — the leak");
        }

        // The eviction tick's sweep reaps the un-renewed pin.
        let reaped = hub.sweep_stale_shells(SHELL_PIN_STALE_AGE);
        assert_eq!(reaped, vec![sandbox_id], "stale pin must be reaped");

        // Sandbox is once again an idle-eviction candidate.
        let after = hub.idle_sandboxes(Duration::from_secs(30), Duration::from_secs(1800));
        assert_eq!(
            after.len(),
            1,
            "after the stale sweep the sandbox must be evictable again; got {after:?}",
        );
        assert_eq!(after[0].1, sandbox_id);
    }

    /// A pin renewed within the window is NOT reaped — a live shell
    /// session keeps its sandbox pinned across many sweep ticks.
    #[test]
    fn renewed_shell_pin_survives_the_sweep() {
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        hub.acquire_shell(sandbox_id);

        // Backdate near the edge, then renew — the renew must reset the
        // clock so the immediately-following sweep keeps it.
        {
            let mut guard = hub.inner.shell_attached.lock();
            let entry = guard.get_mut(&sandbox_id).unwrap();
            entry.1 = Instant::now()
                .checked_sub(SHELL_PIN_STALE_AGE + Duration::from_secs(10))
                .unwrap_or_else(Instant::now);
        }
        hub.renew_shell(sandbox_id);

        let reaped = hub.sweep_stale_shells(SHELL_PIN_STALE_AGE);
        assert!(
            reaped.is_empty(),
            "a freshly-renewed pin must survive the sweep; reaped {reaped:?}",
        );
        assert!(
            hub.inner.shell_attached.lock().contains_key(&sandbox_id),
            "renewed pin must remain",
        );
    }

    /// `renew_shell` must never resurrect a pin that was already
    /// released/cleared — otherwise a late renewal RPC from a dying
    /// bridge could re-pin a sandbox the host just freed.
    #[test]
    fn renew_shell_does_not_resurrect_a_released_pin() {
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        hub.acquire_shell(sandbox_id);
        hub.release_shell(sandbox_id);
        assert!(
            !hub.inner.shell_attached.lock().contains_key(&sandbox_id),
            "release must clear the entry at count 0",
        );
        hub.renew_shell(sandbox_id);
        assert!(
            !hub.inner.shell_attached.lock().contains_key(&sandbox_id),
            "renew on a missing pin must be a no-op, not a resurrection",
        );
    }

    /// Multi-tab refcount semantics are preserved across the new
    /// `(count, Instant)` representation: two acquires need two releases
    /// before the pin clears.
    #[test]
    fn shell_pin_refcount_survives_the_instant_pairing() {
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        hub.acquire_shell(sandbox_id);
        hub.acquire_shell(sandbox_id);
        assert_eq!(
            hub.inner.shell_attached.lock().get(&sandbox_id).unwrap().0,
            2,
            "two tabs → count 2",
        );
        hub.release_shell(sandbox_id);
        assert!(
            hub.inner.shell_attached.lock().contains_key(&sandbox_id),
            "first release must NOT clear while a second tab is open",
        );
        hub.release_shell(sandbox_id);
        assert!(
            !hub.inner.shell_attached.lock().contains_key(&sandbox_id),
            "second release clears the pin",
        );
    }

    /// `clear_shell` (the destroy-path cleanup) empties the pin
    /// regardless of refcount — once the sandbox is gone the pin is
    /// meaningless and must not leak.
    #[test]
    fn clear_shell_empties_pin_regardless_of_count() {
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        hub.acquire_shell(sandbox_id);
        hub.acquire_shell(sandbox_id);
        hub.clear_shell(sandbox_id);
        assert!(
            !hub.inner.shell_attached.lock().contains_key(&sandbox_id),
            "destroy-time clear must empty the map even with count > 1",
        );
        // Idempotent on an already-clear entry.
        hub.clear_shell(sandbox_id);
    }

    /// Regression for issue #217: ABBA lock-order inversion between
    /// `send_prompt` (held `connections` then acquired `last_idle_at`)
    /// and `idle_sandboxes` (acquires `last_idle_at` then `connections`).
    ///
    /// These two run concurrently by design — `send_prompt` on every
    /// prompt RPC, `idle_sandboxes` on the eviction tick. Under the
    /// buggy ordering one interleaving wedges both `parking_lot::Mutex`es
    /// forever (no detection, no poisoning), stalling the entire harness
    /// plane while the host still looks healthy to coord.
    ///
    /// The test hammers both paths from dedicated OS threads against an
    /// attached sandbox, under a watchdog. With the bug present it
    /// deadlocks and the watchdog trips; with the fix (`send_prompt`
    /// drops the `connections` guard before touching `last_idle_at`) the
    /// loops complete well within the budget.
    ///
    /// Run on a multi-thread runtime so the racing tasks land on
    /// distinct worker threads (a current-thread runtime would serialize
    /// them and never expose the inversion).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn send_prompt_and_idle_sandboxes_do_not_deadlock() {
        let hub = HarnessHub::new(noop_sink());
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Keep the harness side alive (drain frames) for the whole test
        // so the connection stays in the `connections` map — that's what
        // makes `send_prompt`'s lookup take the contended path. Without a
        // reader the bounded writer channel could fill and change timing.
        let harness_task = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(harness_side);
            write_msg(
                &mut w,
                &HarnessAttach {
                    session_id,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .expect("attach");
            let _ack: HarnessAttachAck = read_msg(&mut r).await.expect("ack");
            // Drain inbound prompt frames until the host closes the link.
            while read_msg::<_, HarnessFrame>(&mut r).await.is_ok() {}
        });

        // Wait for the connection to be established before racing.
        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach before the stress loop",
        );

        const ITERS: usize = 20_000;
        let hub_idle = hub.clone();
        // `idle_sandboxes` loop on its own OS thread (mirrors the
        // eviction tick). Spawn-blocking so it runs on a blocking
        // thread, not a tokio worker — a wedge here must not be able to
        // starve the watchdog.
        let idle_loop = tokio::task::spawn_blocking(move || {
            for _ in 0..ITERS {
                // Tiny TTLs so the lock body does its full work each call.
                let _ = hub_idle.idle_sandboxes(
                    std::time::Duration::from_millis(0),
                    std::time::Duration::from_millis(0),
                );
            }
        });

        // `send_prompt` loop — the connection is attached, so each call
        // hits the `connections`-then-`last_idle_at` critical section.
        let hub_prompt = hub.clone();
        let prompt_loop = tokio::spawn(async move {
            for i in 0..ITERS {
                // Ignore the result: WriterClosed at teardown is fine —
                // we only care that the call returns at all (no wedge).
                let _ = hub_prompt
                    .send_prompt(sandbox_id, format!("pid{i}"), format!("p{i}"))
                    .await;
            }
        });

        // Watchdog: on the buggy code the two loops deadlock and neither
        // join future ever resolves. A generous budget keeps a slow CI
        // runner from flaking while still catching a real wedge.
        let work = async {
            let _ = idle_loop.await;
            let _ = prompt_loop.await;
        };
        tokio::time::timeout(Duration::from_secs(30), work)
            .await
            .expect("send_prompt/idle_sandboxes deadlocked (issue #217 ABBA inversion)");

        // Tear down the harness reader.
        harness_task.abort();
        let _ = harness_task.await;
    }

    /// Regression for issue #218: a stale connection's teardown must
    /// never remove the registration a newer reconnect installed under
    /// the same `sandbox_id`.
    ///
    /// Models the documented resume/re-prompt sequence: connection A is
    /// attached, then agentd kills its adapter and the replacement
    /// adapter B redials and attaches (replacing A's handle under the
    /// same `sandbox_id`). A's reader then observes EOF and runs its
    /// teardown epilogue. Before the fix that epilogue did an
    /// unconditional keyed remove, deleting B's live registration plus
    /// its idle-tracking timestamps — leaving a healthy adapter that
    /// bounces every prompt with `NotAttached`, has its writer half torn
    /// down, and disappears from idle detection.
    ///
    /// The test forces the bad interleaving deterministically: it brings
    /// up A, then brings up B and waits for B to own the registration,
    /// and only THEN drops A's harness end so A's EOF epilogue runs
    /// strictly after B's attach. It asserts B still owns
    /// `connections[S]` and that a `send_prompt` flows through to B's
    /// live writer (i.e. B's writer half stayed open).
    #[tokio::test]
    async fn stale_teardown_does_not_evict_replacement_connection() {
        let (sink, _) = collecting_sink();
        let hub = HarnessHub::new(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();

        // --- Connection A: attach and register. ---
        let (host_a, harness_a) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_a);
        let (mut a_r, mut a_w) = tokio::io::split(harness_a);
        write_msg(
            &mut a_w,
            &HarnessAttach {
                session_id,
                harness_version: "test-A/0.1".into(),
            },
        )
        .await
        .expect("A attach");
        let ack_a: HarnessAttachAck = read_msg(&mut a_r).await.expect("A ack");
        assert!(ack_a.ok, "A should attach");
        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "A should be registered",
        );
        let gen_a = hub
            .inner
            .connections
            .lock()
            .get(&sandbox_id)
            .map(|h| h.generation)
            .expect("A registered");

        // --- Connection B: the replacement adapter redials and
        // attaches under the SAME sandbox_id (later binds replace
        // earlier ones). Its insert overwrites A's handle. ---
        let (host_b, harness_b) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_b);
        let (mut b_r, mut b_w) = tokio::io::split(harness_b);
        write_msg(
            &mut b_w,
            &HarnessAttach {
                session_id,
                harness_version: "test-B/0.1".into(),
            },
        )
        .await
        .expect("B attach");
        let ack_b: HarnessAttachAck = read_msg(&mut b_r).await.expect("B ack");
        assert!(ack_b.ok, "B should attach");

        // Wait until B owns the registration (a strictly newer
        // generation than A's is in the map). This makes the rest of
        // the test deterministic regardless of task scheduling.
        assert!(
            wait_until(|| {
                hub.inner
                    .connections
                    .lock()
                    .get(&sandbox_id)
                    .map(|h| h.generation != gen_a)
                    .unwrap_or(false)
            })
            .await,
            "B should have replaced A's registration",
        );

        // --- Now drop A's harness end so A's reader hits EOF and runs
        // its teardown epilogue STRICTLY after B's attach. This is the
        // exact stale-teardown ordering from the bug report. ---
        drop(a_r);
        drop(a_w);

        // Give A's epilogue ample opportunity to run. With the bug it
        // would remove B's registration here; with the fix the
        // generation guard leaves B's entry untouched.
        assert!(
            wait_until(|| {
                // The bug manifests as the entry disappearing; the fix
                // keeps exactly B's entry. We wait for the map to be
                // observed at least once after A's drop, then assert
                // below — but a positive condition keeps this honest:
                // B must remain registered.
                hub.attached_count() == 1
            })
            .await,
            "B's registration must survive A's stale teardown",
        );
        // A small extra settle so a late, buggy epilogue would have
        // fired before the hard assertions below.
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            hub.attached_count(),
            1,
            "exactly B must remain registered after A's teardown",
        );
        let live_gen = hub
            .inner
            .connections
            .lock()
            .get(&sandbox_id)
            .map(|h| h.generation)
            .expect("B must still be registered after A's stale teardown");
        assert_ne!(
            live_gen, gen_a,
            "the surviving registration must be B's, not A's",
        );

        // B's writer half must still be open: a prompt must flow through
        // to B's harness end. Before the fix the teardown dropped B's
        // cmd_tx, which made B's writer_loop exit and close the write
        // half — so this send would fail / never arrive.
        hub.send_prompt(sandbox_id, "pid-B".into(), "hello-B".into())
            .await
            .expect("send_prompt must reach B's live writer");
        let frame: HarnessFrame = read_msg(&mut b_r)
            .await
            .expect("B should receive the prompt");
        match frame {
            HarnessFrame::Command(HarnessCommand::Prompt { text, prompt_id }) => {
                assert_eq!(text, "hello-B");
                assert_eq!(prompt_id, "pid-B");
            }
            other => panic!("expected a Prompt frame at B, got {other:?}"),
        }
    }
}
