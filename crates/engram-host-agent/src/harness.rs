//! Harness-channel hub on the host side.
//!
//! Owns one [`HarnessConnection`] per attached harness. Each connection
//! is a long-lived bidirectional stream wrapping a guest-dialed link
//! (vsock in production, UDS for tests). On each received
//! [`HarnessEvent`] the hub:
//!
//! 1. forwards it into the `EventSink` (the coordinator's durable log —
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
use std::time::Duration;

use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, AttachReject, CheckpointReason, HarnessAttach, HarnessAttachAck,
    HarnessCommand, HarnessEvent, HarnessFrame,
};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// Default timeout for a `Checkpoint` command's ack. The harness gets
/// this long to flush its transcript and reply; on miss the caller
/// surfaces `HarnessError::CommandTimeout` and (in idle paths)
/// proceeds without the durability guarantee.
pub const CHECKPOINT_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// ADR 0052 Phase 2 (clean-idle-shutdown): grace handed to the in-guest
/// harness to drain `claude` before an idle-eviction snapshot. The
/// `Shutdown { grace_secs }` closes claude's held stdin; an idle session
/// has no in-flight turn, so it EOFs and exits 0 near-instantly. The
/// window only bounds the rare tail-end turn still wrapping up as
/// eviction fires (after grace the harness kills the child). Kept short:
/// idle eviction shouldn't stall on a stuck agent.
pub const IDLE_DRAIN_GRACE_SECS: u32 = 10;

/// Slack added on top of [`IDLE_DRAIN_GRACE_SECS`] when waiting for the
/// harness's vsock connection to drop after a drain — covers the harness
/// reap + process exit + the host reader-loop observing EOF and tearing
/// the connection down. If the connection is still up past grace+slack we
/// give up waiting and capture anyway (a still-live child is reattached
/// on resume, ADR 0045 C1).
const DRAIN_DETACH_SLACK: Duration = Duration::from_secs(3);

/// Poll cadence while waiting for the post-drain disconnect. Mirrors the
/// `send_prompt` attach-wait loop; off the hot path (idle eviction).
const DRAIN_DETACH_POLL: Duration = Duration::from_millis(50);

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

/// Locking (post-ADR 0073): ONE leaf mutex. The issue #217 "CANONICAL
/// LOCK ORDER" era — eight mutexed maps, a documented acquisition
/// order, and an ABBA wedge that stalled the whole harness plane while
/// heartbeats looked healthy — ended when the 0070 phases deleted the
/// other seven maps (replay buffers → the durable outbox; routing →
/// the on-disk binding records; TTL / shell-pin / eviction bookkeeping
/// → the coordinator's PG detector + shell-pin column). `connections`
/// is never held across an await and never nested with another lock;
/// keep it that way — the ADR records full actor-ization as
/// moot-by-deletion, so a second long-lived mutex here needs to make
/// that case again.
struct HubInner {
    /// Per-sandbox connection state. Populated by `accept_connection`,
    /// removed when the harness disconnects (clean or error).
    connections: Mutex<HashMap<SandboxId, ConnectionHandle>>,
    /// Event-emit callback supplied at construction. Invoked for every
    /// inbound `HarnessEvent`.
    event_sink: EventSink,
    /// Host-durable session→sandbox binding records (ADR 0073).
    /// `bind_session` writes a record; every attach — vsock and TCP
    /// alike — validates the presented token against the record ON
    /// DISK, never an in-memory map, so a freshly restarted host-agent
    /// accepts survivor re-dials with zero rebuild pass and a stale
    /// generation is rejected `Superseded` deterministically.
    bindings: crate::bindings::BindingStore,
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

    pub fn new(event_sink: EventSink, bindings: crate::bindings::BindingStore) -> Self {
        Self {
            inner: Arc::new(HubInner {
                connections: Mutex::new(HashMap::new()),
                event_sink,
                bindings,
                next_gen: AtomicU64::new(0),
            }),
        }
    }

    /// Record the durable binding for `session_id` (ADR 0073): an
    /// upcoming harness connection presenting this exact token routes
    /// to `sandbox_id`. Called when the host-agent spawns/starts a
    /// harness. Monotonic in `binding_epoch` — a stale caller's write
    /// is refused, so resume races converge to the newest generation
    /// regardless of RPC arrival order.
    pub fn bind_session(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        binding_epoch: u64,
    ) -> Result<(), crate::bindings::BindError> {
        self.inner
            .bindings
            .bind(session_id, sandbox_id, binding_epoch)
            .map(|_| ())
    }

    /// Drop the durable binding. Called from `destroy()` paths so a
    /// late dial from the torn-down generation gets `UnknownBinding`
    /// (transient) instead of routing anywhere.
    pub fn unbind_session(&self, session_id: SessionId) {
        if let Err(e) = self.inner.bindings.unbind(session_id) {
            tracing::warn!(session_id = %session_id, error = %e, "binding unbind failed");
        }
    }

    /// The sandbox currently bound to `session_id`, if any — read from
    /// the durable record (the same source every attach validates
    /// against), so this answers "would an in-guest harness re-dial
    /// for this session route?" even on a freshly restarted host-agent.
    pub fn bound_sandbox(&self, session_id: SessionId) -> Option<SandboxId> {
        self.inner
            .bindings
            .read(session_id)
            .ok()
            .flatten()
            .map(|r| r.sandbox_id)
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

    /// Acquire the connection's command sender. ADR 0073: NO
    /// attach-wait — a missing connection is an immediate
    /// `NotAttached`; the durable retry lives in the coordinator's
    /// outbox, and idle detection reads the durable event log (the
    /// prompt's own run_started resets the soft TTL there).
    fn acquire_cmd_tx(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<mpsc::Sender<HarnessFrame>, HarnessError> {
        self.inner
            .connections
            .lock()
            .get(&sandbox_id)
            .map(|h| h.cmd_tx.clone())
            .ok_or(HarnessError::NotAttached)
    }

    /// Push a prompt to the running adapter. Adapter starts a
    /// fresh run (or queues if a run is in flight).
    ///
    /// Returns `NotAttached` if no harness is bound to `sandbox_id`
    /// (e.g., session is `Idle` and needs auto-resume first — call
    /// `ensure_active` upstream).
    pub async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
        mode: Option<String>,
    ) -> Result<(), HarnessError> {
        // ADR 0073: fail fast when unattached. The coordinator's outbox
        // driver owns retries (and the reattach), and the mpsc handoff
        // below is fire-and-forget by design — durability is the PG row
        // + the ack loop, not host memory.
        let cmd_tx = self.acquire_cmd_tx(sandbox_id)?;
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::Prompt {
                prompt_id,
                text,
                mode,
            }))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// Phase 1b: forward a queue-mutation command (Edit/Dequeue) to the
    /// attached harness. The target prompt was already queued (so the
    /// harness is attached); if it isn't, the prompt is gone and
    /// `NotAttached` is the correct answer.
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

    /// ADR 0089: deliver an opaque result for an orchestrator-registered
    /// tool. The coordinator outbox owns retry and confirmation semantics.
    pub async fn tool_result(
        &self,
        sandbox_id: SandboxId,
        tool_call_id: String,
        result_json: String,
    ) -> Result<(), HarnessError> {
        let cmd_tx = self.acquire_cmd_tx(sandbox_id)?;
        cmd_tx
            .send(HarnessFrame::Command(HarnessCommand::ToolResult {
                call_id: tool_call_id,
                result_json,
            }))
            .await
            .map_err(|_| HarnessError::WriterClosed)?;
        Ok(())
    }

    /// ADR 0073 phase 4: sandboxes with a live harness connection —
    /// the heartbeat's `harness_attached` liveness set (the coordinator
    /// compares it against its own view as a disagreement alarm).
    pub fn attached_sandboxes(&self) -> Vec<SandboxId> {
        self.inner.connections.lock().keys().copied().collect()
    }

    /// Number of currently-attached harnesses. Diagnostic / test helper.
    pub fn attached_count(&self) -> usize {
        self.inner.connections.lock().len()
    }

    /// Is a harness currently attached for `sandbox_id`? The entry is
    /// removed when the harness's vsock connection drops (reader-loop EOF
    /// teardown), so this flips false the moment the in-guest harness
    /// exits — the signal [`drain`](Self::drain) waits on.
    pub fn is_attached(&self, sandbox_id: SandboxId) -> bool {
        self.inner.connections.lock().contains_key(&sandbox_id)
    }

    /// ADR 0052 Phase 2 (clean-idle-shutdown): gracefully stop the
    /// in-guest `claude` before an idle-eviction snapshot, so the captured
    /// memory image contains NO live agent process.
    ///
    /// Sends `Shutdown { grace_secs }` — the harness closes claude's held
    /// stdin (the in-flight turn, if any, drains to a final `result` and
    /// the process exits 0), emits nothing spurious when idle, then exits
    /// itself. We then wait (bounded by `grace_secs` + [`DRAIN_DETACH_SLACK`])
    /// for the harness's vsock connection to drop: its exit is the signal
    /// that both claude and the harness are gone. On resume agentd respawns
    /// a fresh harness which `--resume`s into the same on-disk session
    /// (decision 1: respawn-with-resume), so nothing is lost.
    ///
    /// Best-effort — returns `true` if the snapshot will be agent-free:
    /// either the harness detached in time, or there was nothing attached
    /// to drain (`NotAttached`). Returns `false` if a harness stayed
    /// attached past the deadline (a stuck/uncooperative agent); the caller
    /// captures anyway and the still-live child is reattached on resume.
    pub async fn drain(&self, sandbox_id: SandboxId, grace_secs: u32) -> bool {
        match self.shutdown(sandbox_id, grace_secs).await {
            Ok(()) => {}
            // Nothing bound — already agent-free, no drain needed.
            Err(HarnessError::NotAttached) => return true,
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "idle drain: Shutdown send failed; capturing without a clean drain",
                );
                return false;
            }
        }
        let deadline = crate::time_source::metrics_now()
            + Duration::from_secs(grace_secs as u64)
            + DRAIN_DETACH_SLACK;
        while self.is_attached(sandbox_id) {
            if crate::time_source::metrics_now() >= deadline {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    grace_secs,
                    "idle drain: harness still attached after grace; capturing with a live \
                     claude (reattached on resume)",
                );
                return false;
            }
            tokio::time::sleep(DRAIN_DETACH_POLL).await;
        }
        tracing::debug!(
            sandbox_id = %sandbox_id,
            "idle drain: harness detached; snapshot will be claude-free",
        );
        true
    }
}

/// ADR 0108 A1: bound on the handshake read. A connection whose
/// `HarnessAttach` frame was swallowed (the post-checkpoint vsock RX-gate
/// window, PR #596's named-latent arm) would otherwise park the accept
/// task forever — an unlogged fd + task leak per occurrence. The SDK
/// gives up and redials at 5 s; 10 s here keeps the host strictly more
/// patient, so a slow-but-live guest is never cut off by the host first.
const ATTACH_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// ADR 0108 A2: generation-ordered registration. Insert only when the
/// slot is empty or held by an OLDER generation. Returns false when a
/// newer connection already owns the slot — the caller must close
/// without acking.
fn register_generation_ordered(
    inner: &HubInner,
    sandbox_id: SandboxId,
    handle: ConnectionHandle,
) -> bool {
    let mut conns = inner.connections.lock();
    match conns.get(&sandbox_id) {
        Some(existing) if existing.generation > handle.generation => false,
        _ => {
            conns.insert(sandbox_id, handle);
            true
        }
    }
}

async fn read_attach_bounded<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<HarnessAttach> {
    match tokio::time::timeout(ATTACH_READ_TIMEOUT, read_msg::<_, HarnessAttach>(reader)).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "attach frame never arrived within the handshake bound",
        )),
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

    let attach: HarnessAttach = match read_attach_bounded(&mut reader).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(error = %e, sandbox_id = %sandbox_id, "harness handshake read failed");
            return;
        }
    };
    if let Some(expected) = expected_session_id {
        if attach.session_id != expected {
            reject_attach(
                &mut writer,
                &attach,
                AttachReject::SessionMismatch,
                "session_id mismatch",
            )
            .await;
            return;
        }
    }
    // ADR 0073: even with a transport-derived sandbox_id, the token is
    // validated against the durable record — the transport tells us
    // where the bytes came from, the record tells us which GENERATION
    // currently owns the session. The epoch is the fence; the token's
    // sandbox_id is informational only (a live-moved harness carries
    // its old sandbox in a frozen env — ADR 0045 C1 — and is still
    // the current generation). Connection state is keyed on the
    // TRANSPORT sandbox: on a live move the record may briefly lag
    // the VM the bytes actually arrived from.
    match validate_attach(&inner, &attach) {
        Ok(record) => {
            if attach.sandbox_id != sandbox_id || record.sandbox_id != sandbox_id {
                tracing::debug!(
                    session_id = %attach.session_id,
                    transport_sandbox = %sandbox_id,
                    token_sandbox = %attach.sandbox_id,
                    record_sandbox = %record.sandbox_id,
                    "attach token/record sandbox differs from transport (live-move shape)",
                );
            }
            drive_attached(inner, sandbox_id, attach, reader, writer).await;
        }
        Err(reject) => {
            reject_attach(&mut writer, &attach, reject, reject_reason(reject)).await;
        }
    }
}

/// TCP-listener path (Process backend, tests): read HarnessAttach,
/// resolve the sandbox from the DURABLE binding record (ADR 0073 —
/// never an in-memory map), validate the token, then run the same
/// post-attach loop as `run_connection`.
async fn run_connection_with_session_lookup<S>(inner: Arc<HubInner>, stream: S)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let attach: HarnessAttach = match read_attach_bounded(&mut reader).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(error = %e, "harness handshake read failed (no session bound)");
            return;
        }
    };
    match validate_attach(&inner, &attach) {
        Ok(record) => {
            let sandbox_id = record.sandbox_id;
            drive_attached(inner, sandbox_id, attach, reader, writer).await;
        }
        Err(reject) => {
            reject_attach(&mut writer, &attach, reject, reject_reason(reject)).await;
        }
    }
}

/// ADR 0073 attach-token validation, shared by both connection paths.
/// The EPOCH is the entire fence:
/// - no record / unreadable record → `UnknownBinding` (transient);
/// - presented epoch < record        → `Superseded` (fatal);
/// - presented epoch > record        → `UnknownBinding` (the bind for
///   the harness's own generation hasn't landed yet — retry);
/// - equal epoch → accept. The token's sandbox_id is deliberately NOT
///   compared: a live-moved harness (ADR 0045 C1) presents its frozen
///   spawn-time sandbox while remaining the current generation.
fn validate_attach(
    inner: &HubInner,
    attach: &HarnessAttach,
) -> Result<crate::bindings::BindingRecord, AttachReject> {
    let record = match inner.bindings.read(attach.session_id) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(AttachReject::UnknownBinding),
        Err(e) => {
            tracing::warn!(
                session_id = %attach.session_id,
                error = %e,
                "binding record unreadable at attach",
            );
            return Err(AttachReject::UnknownBinding);
        }
    };
    if attach.binding_epoch < record.binding_epoch {
        return Err(AttachReject::Superseded);
    }
    if attach.binding_epoch > record.binding_epoch {
        return Err(AttachReject::UnknownBinding);
    }
    Ok(record)
}

fn reject_reason(reject: AttachReject) -> &'static str {
    match reject {
        AttachReject::UnknownBinding => "no current binding record for this session",
        AttachReject::Superseded => "binding superseded by a newer generation",
        AttachReject::SessionMismatch => "attach token does not match the binding record",
    }
}

async fn reject_attach<W: AsyncWrite + Unpin>(
    writer: &mut W,
    attach: &HarnessAttach,
    reject: AttachReject,
    message: &str,
) {
    let ack = HarnessAttachAck {
        ok: false,
        reject: Some(reject),
        message: Some(message.into()),
    };
    let _ = write_msg(writer, &ack).await;
    tracing::warn!(
        session_id = %attach.session_id,
        sandbox_id = %attach.sandbox_id,
        binding_epoch = attach.binding_epoch,
        reject = ?reject,
        "harness attach rejected",
    );
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
        cmd_tx: cmd_tx.clone(),
        pending_checkpoint: Mutex::new(None),
        generation: my_generation,
    };
    // ADR 0108 A2, invariant 2 — "ack implies registered": the insert
    // happens BEFORE the ack write, so when the SDK reads `ok: true`
    // the hub can already route to this connection. And the insert is
    // generation-ORDERED: the mint and the insert are separated by a
    // lock acquisition, so a slower task holding an older generation
    // can lose the lock race to a newer one — an unconditional insert
    // would let the loser clobber the winner (the #218 guard covers
    // only removal). An older generation backs off and closes; its
    // guest side sees EOF and redials against the surviving winner.
    if !register_generation_ordered(&inner, sandbox_id, handle) {
        tracing::debug!(
            session_id = %attach.session_id,
            sandbox_id = %sandbox_id,
            generation = my_generation,
            "stale connection lost the registration race; closing unacked",
        );
        return;
    }

    let ack = HarnessAttachAck {
        ok: true,
        reject: None,
        message: None,
    };
    if let Err(e) = write_msg(&mut writer, &ack).await {
        tracing::debug!(error = %e, "harness ack write failed");
        // We registered but the peer can never learn it: undo, guarded
        // by our own generation (a newer connection may already own the
        // entry).
        let mut conns = inner.connections.lock();
        let still_ours = conns
            .get(&sandbox_id)
            .map(|h| h.generation == my_generation)
            .unwrap_or(false);
        if still_ours {
            conns.remove(&sandbox_id);
        }
        return;
    }

    tracing::debug!(
        session_id = %attach.session_id,
        sandbox_id = %sandbox_id,
        harness_version = %attach.harness_version,
        "harness attached",
    );
    // ADR 0073 phase 3: no TTL bookkeeping here. Idle detection reads
    // the durable event log coordinator-side (idle_detector.rs), which
    // inherits the seed-at-attach lesson structurally: a session's
    // clock starts at its created_at/last event, never at attach
    // (prod 43fe13b4: seed-at-attach soft-TTL'd fresh sessions whose
    // harness needed >30s to emit its first event).

    let writer_task = tokio::spawn(writer_loop(writer, cmd_rx));

    // ADR 0073: no replay pass. Command-side at-least-once now lives in
    // the coordinator's session_outbox (redelivered until the confirming
    // event acks the row) — host memory holds nothing durable.
    drop(cmd_tx);

    let reader_outcome = reader_loop(reader, &inner, attach.session_id, sandbox_id).await;
    // Issue #218: guarded teardown. Only remove the registration if it
    // is STILL ours — a reconnect inserts a fresh handle with a higher
    // generation; seeing a different generation (or no entry) means a
    // newer connection owns it and we must leave it untouched.
    {
        let mut conns = inner.connections.lock();
        let still_ours = conns
            .get(&sandbox_id)
            .map(|h| h.generation == my_generation)
            .unwrap_or(false);
        if still_ours {
            conns.remove(&sandbox_id);
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
                // ADR 0073: no host-side TTL bookkeeping — the sink
                // lands the event in the coordinator's durable log,
                // which IS the idle detector's input (an `Idle` event
                // becoming newest starts the soft clock; any other
                // event resets it — the same set/clear rule the old
                // in-memory maps implemented). Confirming events
                // (RunStarted{prompt_id} / PromptQueued /
                // ToolCallCompleted) also ack the outbox row at the
                // coordinator's emit choke point.
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
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
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

    /// ADR 0073: hub over an ephemeral binding store. Tests exercise
    /// the same disk-validated attach path as production.
    fn test_hub(sink: EventSink) -> HarnessHub {
        HarnessHub::new(
            sink,
            crate::bindings::BindingStore::open(
                std::env::temp_dir().join(format!("engram-hub-test-{}", uuid::Uuid::new_v4())),
            )
            .expect("binding store"),
        )
    }

    /// Convenience: build a paired in-memory stream for harness ↔ host.
    /// Returns (host_side, harness_side). Either end can read/write.
    fn duplex_pair() -> (DuplexStream, DuplexStream) {
        tokio::io::duplex(1 << 16)
    }

    /// Drives the harness side of a connection: sends `HarnessAttach`,
    /// reads ack, then runs `body` with read/write handles to the host.
    async fn drive_harness<F, Fut>(
        hub: &HarnessHub,
        harness_side: DuplexStream,
        session_id: SessionId,
        sandbox_id: SandboxId,
        body: F,
    ) -> HarnessAttachAck
    where
        F: FnOnce(tokio::io::ReadHalf<DuplexStream>, tokio::io::WriteHalf<DuplexStream>) -> Fut,
        Fut: Future<Output = ()>,
    {
        // ADR 0073: bind (epoch 1) so the attach token validates —
        // the same choreography the host-agent runs before a spawn.
        // Idempotent for tests that already bound.
        let _ = hub.bind_session(session_id, sandbox_id, 1);
        let (mut harness_r, mut harness_w) = tokio::io::split(harness_side);
        write_msg(
            &mut harness_w,
            &HarnessAttach {
                session_id,
                sandbox_id,
                binding_epoch: 1,
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
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let ack = drive_harness(
            &hub,
            harness_side,
            session_id,
            sandbox_id,
            |_r, mut w| async move {
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
            },
        )
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
    }

    #[tokio::test]
    async fn session_id_mismatch_closes_connection_cleanly() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let expected = SessionId::new();
        let attached_with = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(expected, sandbox_id, 1).expect("bind");

        hub.accept_connection(sandbox_id, Some(expected), host_side);

        let ack = drive_harness(
            &hub,
            harness_side,
            attached_with,
            sandbox_id,
            |_r, _w| async {},
        )
        .await;
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
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Drive the harness side: do the attach handshake, then
        // wait to read one Shutdown command.
        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    sandbox_id,
                    binding_epoch: 1,
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
    async fn drain_sends_shutdown_then_waits_for_disconnect() {
        // ADR 0052 Phase 2: `drain` must (a) deliver a Shutdown frame and
        // (b) not return until the harness's connection drops — the signal
        // that claude + the harness have exited and the snapshot is safe.
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Harness side: handshake, read ONE Shutdown frame, then drop its
        // end of the pipe (= a clean disconnect, the post-drain exit).
        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    sandbox_id,
                    binding_epoch: 1,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            let frame: HarnessFrame = read_msg(&mut hr).await.unwrap();
            assert!(matches!(
                frame,
                HarnessFrame::Command(HarnessCommand::Shutdown { .. })
            ));
            // Returning drops `harness_side`, closing the connection.
        });

        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach within the 1s deadline"
        );

        // drain() sends the Shutdown, the task reads it and disconnects,
        // and drain() resolves true once the connection is gone.
        let drained = hub.drain(sandbox_id, 2).await;
        harness_task.await.unwrap();
        assert!(drained, "drain should report a clean disconnect");
        assert!(
            !hub.is_attached(sandbox_id),
            "connection must be torn down after the harness exits"
        );
    }

    #[tokio::test]
    async fn drain_unattached_is_a_noop() {
        // No harness bound → nothing to drain → the snapshot is already
        // claude-free, so drain succeeds immediately (no waiting).
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        assert!(hub.drain(SandboxId::new(), 2).await);
    }

    #[tokio::test]
    async fn interrupt_command_reaches_harness() {
        // ADR 0030: hub.interrupt() must deliver a HarnessCommand::Interrupt
        // to the attached harness (which SIGINTs its child + emits
        // RunInterrupted). Mirrors shutdown_command_reaches_harness; the
        // SIGINT-the-claude-child leaf is Linux + claude-runtime specific
        // and is covered by the manual spike documented in the ADR.
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    sandbox_id,
                    binding_epoch: 1,
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
        let hub = test_hub(sink);
        let err = hub.interrupt(SandboxId::new()).await.unwrap_err();
        assert!(matches!(err, HarnessError::NotAttached));
    }

    #[tokio::test]
    async fn tool_result_reaches_harness_and_retires_on_confirmation() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");
        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        let harness_task = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    sandbox_id,
                    binding_epoch: 1,
                    harness_version: "test/0.1".into(),
                },
            )
            .await
            .unwrap();
            let _: HarnessAttachAck = read_msg(&mut hr).await.unwrap();
            let frame: HarnessFrame = read_msg(&mut hr).await.unwrap();
            let ok = matches!(
                &frame,
                HarnessFrame::Command(HarnessCommand::ToolResult {
                    call_id,
                    result_json,
                }) if call_id == "call_1" && result_json == r#"{"saved":true}"#
            );
            write_msg(
                &mut hw,
                &HarnessFrame::Event(HarnessEvent::ToolCallCompleted {
                    run_id: "run-1".into(),
                    tool_call_id: "call_1".into(),
                    tool_name: "save_memory".into(),
                    ok: true,
                    duration_ms: 1,
                    result_summary: None,
                }),
            )
            .await
            .unwrap();
            ok
        });

        assert!(
            wait_until(|| hub.attached_count() == 1).await,
            "harness should attach within the 1s deadline"
        );

        hub.tool_result(sandbox_id, "call_1".into(), r#"{"saved":true}"#.into())
            .await
            .expect("tool_result");

        assert!(
            harness_task.await.unwrap(),
            "harness should receive a ToolResult frame and confirm the same call id"
        );
    }

    #[tokio::test]
    async fn checkpoint_returns_not_attached_for_unknown_sandbox() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let err = hub
            .checkpoint(SandboxId::new(), CheckpointReason::Idle)
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::NotAttached));
    }

    #[tokio::test]
    async fn harness_disconnect_clears_last_event_at_and_attached_count() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");

        hub.accept_connection(sandbox_id, Some(session_id), host_side);

        // Drive the harness: attach, send one event, drop.
        let h = tokio::spawn(async move {
            let (mut hr, mut hw) = tokio::io::split(harness_side);
            write_msg(
                &mut hw,
                &HarnessAttach {
                    session_id,
                    sandbox_id,
                    binding_epoch: 1,
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
    }

    #[tokio::test]
    async fn snapshot_epoch_bump_detaches_idle_harness() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        let (host_side, harness_side) = duplex_pair();
        let (epoch_tx, epoch_rx) = tokio::sync::watch::channel(0u64);

        hub.bind_session(session_id, sandbox_id, 1).expect("bind");
        hub.accept_connection(
            sandbox_id,
            Some(session_id),
            engram_sandbox_firecracker::EpochSeveredStream::new(host_side, epoch_rx),
        );

        let hub_for_harness = hub.clone();
        let (prompt_read_tx, prompt_read_rx) = oneshot::channel();
        let harness_task = tokio::spawn(async move {
            let ack = drive_harness(
                &hub_for_harness,
                harness_side,
                session_id,
                sandbox_id,
                |mut reader, writer| async move {
                    let frame: HarnessFrame = read_msg(&mut reader)
                        .await
                        .expect("harness should read the prompt");
                    assert!(
                        matches!(frame, HarnessFrame::Command(HarnessCommand::Prompt { .. })),
                        "harness should receive a prompt",
                    );
                    prompt_read_tx
                        .send(())
                        .expect("test should wait for the prompt read");
                    std::future::pending::<()>().await;
                    drop((reader, writer));
                },
            )
            .await;
            assert!(ack.ok, "harness should attach");
        });

        assert!(
            wait_until(|| hub.is_attached(sandbox_id)).await,
            "harness should attach within the 1s deadline",
        );
        hub.send_prompt(sandbox_id, "prompt-1".into(), "hello".into(), None)
            .await
            .expect("send_prompt should reach the harness");
        prompt_read_rx
            .await
            .expect("harness should confirm that it read the prompt");

        epoch_tx.send_modify(|epoch| *epoch += 1);

        assert!(
            wait_until(|| !hub.is_attached(sandbox_id)).await,
            "snapshot severance should detach the idle harness within the 1s deadline",
        );
        harness_task.abort();
    }

    #[tokio::test]
    async fn tcp_listener_routes_attach_via_session_lookup_to_bound_sandbox() {
        // End-to-end demo wiring: bind a (session_id, sandbox_id)
        // pair, spawn a real TCP listener, connect a harness client
        // over a real TCP socket, observe an event arrive at the
        // collecting sink keyed on the *bound* sandbox_id.
        let (sink, collected) = collecting_sink();
        let hub = test_hub(sink);
        let session_id = SessionId::new();
        let sandbox_id = SandboxId::new();
        hub.bind_session(session_id, sandbox_id, 1).expect("bind");

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
                sandbox_id,
                binding_epoch: 1,
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
        let hub = test_hub(sink);

        let (addr, _listener_task) =
            spawn_tcp_listener(hub.clone(), "127.0.0.1:0".parse().unwrap())
                .await
                .expect("listener bound");

        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        write_msg(
            &mut conn,
            &HarnessAttach {
                session_id: SessionId::new(), // not bound
                sandbox_id: SandboxId::new(),
                binding_epoch: 1,
                harness_version: "tcp-test/0.1".into(),
            },
        )
        .await
        .unwrap();
        let ack: HarnessAttachAck = read_msg(&mut conn).await.unwrap();
        assert!(!ack.ok, "unbound session should be rejected");
    }

    // ADR 0016 §A.1.5a — eviction in-flight gate -----------------

    /// ADR 0108 A2, invariant 2 — "ack implies registered": the hub
    /// inserts the connection BEFORE it writes `ok: true`. The instant
    /// the SDK reads the ack, `send_prompt` can route. Before the
    /// reorder there was a window (ack on the wire, insert not yet
    /// run) where a fast prompt bounced `NotAttached` off an acked
    /// harness.
    #[tokio::test]
    async fn ack_implies_registered() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();
        hub.bind_session(session_id, sandbox_id, 1).expect("bind");
        let (host_side, harness_side) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_side);
        let (mut r, mut w) = tokio::io::split(harness_side);
        write_msg(
            &mut w,
            &HarnessAttach {
                session_id,
                sandbox_id,
                binding_epoch: 1,
                harness_version: "test/0.1".into(),
            },
        )
        .await
        .expect("attach");
        let ack: HarnessAttachAck = read_msg(&mut r).await.expect("ack");
        assert!(ack.ok, "attach should succeed");
        // Deliberately NO wait_until here: the ack on the wire IS the
        // proof the insert already ran (same task poll, insert strictly
        // precedes the ack write).
        assert!(
            hub.is_attached(sandbox_id),
            "an acked connection must already be registered",
        );
    }

    /// ADR 0108 A2: generation-ordered insert. The generation mint and
    /// the map insert are separated by a lock acquisition, so a task
    /// holding an OLDER generation can run its insert after a newer
    /// task's. The ordered insert refuses the stale write; the newer
    /// connection keeps the slot. (The #218 guard covers only removal —
    /// this closes the symmetric insert hole.)
    #[tokio::test]
    async fn stale_generation_cannot_clobber_newer_registration() {
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let (tx_new, _rx_new) = mpsc::channel::<HarnessFrame>(1);
        let (tx_old, _rx_old) = mpsc::channel::<HarnessFrame>(1);
        let old_gen = hub.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        let new_gen = hub.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        // The newer generation wins the lock race and registers first.
        assert!(register_generation_ordered(
            &hub.inner,
            sandbox_id,
            ConnectionHandle {
                cmd_tx: tx_new,
                pending_checkpoint: Mutex::new(None),
                generation: new_gen,
            },
        ));
        // The stale task's late insert is refused...
        assert!(!register_generation_ordered(
            &hub.inner,
            sandbox_id,
            ConnectionHandle {
                cmd_tx: tx_old,
                pending_checkpoint: Mutex::new(None),
                generation: old_gen,
            },
        ));
        // ...and the newer connection keeps the slot.
        assert_eq!(
            hub.inner
                .connections
                .lock()
                .get(&sandbox_id)
                .map(|h| h.generation),
            Some(new_gen),
        );
    }

    /// ADR 0108 A1: a connection that never sends its attach frame (the
    /// swallowed-frame shape from the vsock RX-gate window) must not
    /// park the accept task forever. The host cuts it at the handshake
    /// bound; the guest side observes EOF — its cue to redial.
    #[tokio::test(start_paused = true)]
    async fn silent_connection_is_cut_at_the_handshake_bound() {
        use tokio::io::AsyncReadExt;
        let (sink, _) = collecting_sink();
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let (host_side, harness_side) = duplex_pair();
        hub.accept_connection(sandbox_id, None, host_side);
        // Send nothing. Keep our write half open so the only EOF source
        // is the host cutting the connection. Paused time auto-advances
        // past the bound.
        let (mut r, _w) = tokio::io::split(harness_side);
        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(60), r.read(&mut buf))
            .await
            .expect("host must cut the silent connection at the handshake bound")
            .expect("EOF should be clean");
        assert_eq!(n, 0, "expected EOF, not data");
        assert!(!hub.is_attached(sandbox_id));
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
        let hub = test_hub(sink);
        let sandbox_id = SandboxId::new();
        let session_id = SessionId::new();

        // --- Connection A: attach and register. ---
        hub.bind_session(session_id, sandbox_id, 1).expect("bind");
        let (host_a, harness_a) = duplex_pair();
        hub.accept_connection(sandbox_id, Some(session_id), host_a);
        let (mut a_r, mut a_w) = tokio::io::split(harness_a);
        write_msg(
            &mut a_w,
            &HarnessAttach {
                session_id,
                sandbox_id,
                binding_epoch: 1,
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
                sandbox_id,
                binding_epoch: 1,
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
        hub.send_prompt(sandbox_id, "pid-B".into(), "hello-B".into(), None)
            .await
            .expect("send_prompt must reach B's live writer");
        let frame: HarnessFrame = read_msg(&mut b_r)
            .await
            .expect("B should receive the prompt");
        match frame {
            HarnessFrame::Command(HarnessCommand::Prompt {
                text,
                prompt_id,
                mode,
            }) => {
                assert_eq!(text, "hello-B");
                assert_eq!(prompt_id, "pid-B");
                assert_eq!(mode, None);
            }
            other => panic!("expected a Prompt frame at B, got {other:?}"),
        }
    }
}
