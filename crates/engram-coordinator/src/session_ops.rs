//! ADR 0079 (issue #543): the session-op executor — the lifecycle kernel.
//!
//! Every lifecycle verb is a durable `session_ops` row. This module owns
//! the single-writer-per-session executor: claim (CAS-bumps the fencing
//! epoch), drive the verb's step sequence (each step durably recorded,
//! idempotent-from-step), finish/requeue/cancel, and the fence-then-resume
//! reclaim sweep that replaces the lease reaper. Wire handlers only
//! enqueue and observe.
//!
//! Hot path: enqueue fires `pg_notify('session_ops', session_id)`;
//! `pg_listener` re-broadcasts into the `wake` Notify consumed here. A
//! fallback poll (5 s) covers missed notifies only — enqueue→claim is
//! milliseconds by construction, and `op_enqueue_and_claim` makes the
//! idle-session happy path ONE PG round trip (the enqueuer drives the op
//! inline on its own executor entry below).

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::SessionFence;
use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState, SessionOp};
use engram_core::types::BindingDisposition;
use engram_core::types::SessionState;
use engram_core::SessionId;
use tokio::sync::Notify;

use crate::state::SharedState;

/// Fallback rescan cadence — crash-recovery/missed-notify bound only.
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);

/// A running op whose executor hasn't stamped `heartbeat_at` for this
/// long is reclaimable (fence-then-resume).
///
/// ADR 0079 (review finding #1): raised 60s → 180s. Heartbeat is stamped
/// not only at step boundaries but by a WITHIN-STEP background beat
/// ([`OpCtx::spawn_heartbeat`] / [`spawn_op_heartbeat`]) every
/// [`OP_HEARTBEAT_INTERVAL`] while a step's body is in flight — so a step
/// bracketing a multi-second-to-minute host RPC (the ~92s prod
/// GCS-page-in restore, composed capture/upload, boot) keeps proving
/// liveness INDEPENDENTLY of step progress. 180s comfortably exceeds the
/// longest real leg (92s restore tail + margin) with ~12 missed beats of
/// slack, so the sweep only ever fires on a GENUINELY dead executor; a
/// live-but-slow one is never reclaimed, which is what makes the "fence
/// the stale writer's in-flight RPCs" window a non-issue (a dead executor
/// issues no more RPCs — the successor's first fenced host RPC advances
/// the host high-water regardless).
const RECLAIM_STALE: Duration = Duration::from_secs(180);
const RECLAIM_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Cadence of the within-step liveness heartbeat — well under
/// [`RECLAIM_STALE`] so a healthy in-flight step is never mistaken for a
/// dead executor even across a long host RPC.
const OP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// ADR 0079 (review finding #5): how long a session may sit `pending`
/// with no create_boot op before the reclaim sweep re-enqueues one. Long
/// enough that a just-placed session (whose op enqueue is landing) is
/// never swept, short enough to recover well within the 10-minute pending
/// reservation window `place_queued_session` accounts against.
const PENDING_ORPHAN_GRACE: Duration = Duration::from_secs(120);

/// Verb-body outcome: what the executor does with the row.
pub enum OpOutcome {
    /// Success — `done`.
    Done,
    /// Retryable failure: requeue with backoff.
    Retry(String),
    /// ADR 0108 A5: retryable, with a caller-chosen delay instead of the
    /// attempts-scaled backoff. For arms that KNOW their cadence — a
    /// deliver waiting out a boot or an attach grace — the growing
    /// backoff is wrong twice: the wait is not a failure, and the
    /// inflated attempts counter then slows the retries that matter
    /// (the 2026-07-31 incident recovered at a 40 s cadence for this
    /// reason). The attempts counter still increments in the store;
    /// only the pacing is fixed.
    RetryAfter(Duration, String),
    /// Terminal failure.
    Failed(String),
    /// The op observed its cancel flag between steps and stopped at a
    /// safe boundary.
    Cancelled,
}

/// Per-op execution context handed to verb bodies. Wraps the fenced
/// step/heartbeat primitives; a `false` from [`OpCtx::step`] means a
/// successor re-claimed (this writer is FENCED) and the body must return
/// immediately without side effects.
pub struct OpCtx<'a> {
    pub state: &'a SharedState,
    pub op: &'a SessionOp,
    pub epoch: i64,
}

impl OpCtx<'_> {
    /// The op's host-RPC / PG-write fence: `sessions.current_epoch` as
    /// CAS-bumped at this op's claim.
    pub fn fence(&self) -> SessionFence {
        SessionFence {
            session_id: self.op.session_id,
            epoch: self.epoch as u64,
        }
    }

    /// Durably record the step marker (+ progress heartbeat). `false` =
    /// fenced — STOP.
    pub async fn step(&self, step: &str) -> bool {
        match self
            .state
            .services
            .meta
            .op_record_step(self.op.id, self.epoch, step)
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                crate::metrics::note_fenced_write();
                tracing::info!(
                    op_id = self.op.id,
                    session_id = %self.op.session_id,
                    epoch = self.epoch,
                    step,
                    "op fenced at step boundary (successor re-claimed); stopping silently",
                );
                false
            }
            Err(e) => {
                // PG unreachable: indistinguishable from fenced for
                // safety purposes — stop; the reclaim sweep re-drives.
                tracing::warn!(op_id = self.op.id, error = %e, "op step record failed; stopping");
                false
            }
        }
    }

    /// The step already durably recorded (used when resuming from a
    /// reclaim: skip work up to and including `self.op.step`).
    pub fn resume_point(&self) -> Option<&str> {
        self.op.step.as_deref()
    }

    /// Cooperative cancellation check between steps.
    pub async fn cancel_requested(&self) -> bool {
        self.state
            .services
            .meta
            .op_cancel_requested(self.op.id)
            .await
            .unwrap_or(false)
    }
}

/// Callers that receive `Claimed(op)` own driving it. Production uses
/// [`enqueue`] (which spawns detached); the simulator drives synchronously
/// inside its step so no mutating work outlives a scheduler step (ADR 0098
/// determinism discipline).
pub async fn enqueue_claim(
    state: &SharedState,
    session_id: SessionId,
    kind: OpKind,
    payload: serde_json::Value,
    idempotency_key: Option<&str>,
) -> Result<EnqueueOutcome, engram_core::MetaError> {
    let pod = pod_id();
    state
        .services
        .meta
        .op_enqueue_and_claim(session_id, kind, payload, idempotency_key, &pod)
        .await
}

/// Enqueue a verb and, when the session is idle (no running/queued op),
/// drive it INLINE on this task — the one-round-trip happy path. When
/// something is already in flight, the row waits its turn and the
/// executor loop picks it up (NOTIFY-hot).
///
/// Returns the enqueue outcome so wire handlers can bounded-observe the
/// row (`op_get` on the returned id) for API compatibility.
pub async fn enqueue(
    state: &SharedState,
    session_id: SessionId,
    kind: OpKind,
    payload: serde_json::Value,
    idempotency_key: Option<&str>,
) -> Result<EnqueueOutcome, engram_core::MetaError> {
    let outcome = enqueue_claim(state, session_id, kind, payload, idempotency_key).await?;
    if let EnqueueOutcome::Claimed(op) = &outcome {
        // Drive detached from the caller's (possibly wire-lifetime)
        // future: the op row is the durable owner, this spawn is just
        // compute. A cancelled request cancels nothing but its own
        // observation; a dead pod's op is reclaimed by the sweep.
        let state = state.clone();
        let op = op.clone();
        tokio::spawn(async move {
            drive_claimed(&state, op).await;
        });
    }
    Ok(outcome)
}

/// Apply a session state transition under `fence`. Epoch 0 (a caller
/// outside any op — the ADR 0079 interim `SessionFence::unfenced()`
/// paths) takes the plain legality CAS; a real epoch takes the fenced
/// variant, where a 0-row write (`Ok(None)`: a successor re-claimed the
/// session) surfaces as a `Conflict` carrying the `fenced:` marker — the
/// caller stops, never retries, never compensates (its own op-row
/// finish/requeue writes are epoch-fenced no-ops anyway).
pub(crate) async fn transition_with_fence(
    state: &SharedState,
    session_id: SessionId,
    fence: SessionFence,
    to: SessionState,
    disposition: BindingDisposition,
) -> Result<SessionState, engram_core::MetaError> {
    if fence.epoch == 0 {
        return state
            .services
            .meta
            .transition_session(session_id, to, disposition)
            .await;
    }
    match state
        .services
        .meta
        .fenced_transition_session(session_id, fence.epoch as i64, to, disposition)
        .await?
    {
        Some(prev) => Ok(prev),
        None => {
            crate::metrics::note_fenced_write();
            Err(engram_core::MetaError::Conflict(format!(
                "fenced: session {session_id} was re-claimed by a successor op (epoch moved past {})",
                fence.epoch,
            )))
        }
    }
}

/// [`transition_with_fence`] that lands `events` ATOMICALLY with the
/// transition (one store transaction). For transitions that make the
/// session immediately claimable (eviction's Idle flip: "the instant the
/// session is Idle it is resumable"), post-commit `emit_fenced` calls
/// race the successor's claim and the transition's own facts
/// (`evicted`, the final `status_changed`) get fenced out of the record
/// — the e2e_resume event-loss flake. Appending them inside the
/// transition makes a successor order strictly after.
///
/// The events must not be outbox-acking kinds (`outbox_ack_id` = None
/// for lifecycle events); their only post-commit side effect is the
/// in-process publish to live streams, done here with the committed
/// indices.
pub(crate) async fn transition_with_fence_emitting(
    state: &SharedState,
    session_id: SessionId,
    fence: SessionFence,
    to: SessionState,
    disposition: BindingDisposition,
    events: Vec<crate::state::SessionEvent>,
) -> Result<SessionState, engram_core::MetaError> {
    debug_assert!(
        events
            .iter()
            .all(|e| crate::state::outbox_ack_id(session_id, e).is_none()),
        "transition_with_fence_emitting only handles the publish side effect; \
         outbox-acking events must go through emit_fenced"
    );
    if fence.epoch == 0 {
        // Unfenced interim path: plain transition, then plain appends —
        // no fence exists to race, so the atomicity doesn't apply. The
        // plain transition applies the same disposition contract (#896).
        let prev = state
            .services
            .meta
            .transition_session(session_id, to, disposition)
            .await?;
        for event in events {
            let _ = state.emit(session_id, event).await;
        }
        return Ok(prev);
    }
    let wire = crate::state::wire_events(&events)?;
    match state
        .services
        .meta
        .fenced_transition_session_with_events(
            session_id,
            fence.epoch as i64,
            to,
            disposition,
            &wire,
        )
        .await?
    {
        Some((prev, indices)) => {
            for (idx, event) in indices.into_iter().zip(events) {
                state.events.publish(
                    session_id,
                    crate::state::IndexedEvent {
                        idx,
                        event,
                        ephemeral: false,
                    },
                );
            }
            Ok(prev)
        }
        None => {
            crate::metrics::note_fenced_write();
            Err(engram_core::MetaError::Conflict(format!(
                "fenced: session {session_id} was re-claimed by a successor op (epoch moved past {})",
                fence.epoch,
            )))
        }
    }
}

/// ADR 0079 interim: an inline op-log claim for lifecycle pipelines that
/// have NOT yet migrated into verb bodies (the manual snapshot, the evac
/// resume, the live teleport). Rides the same `session_ops` row + the
/// `session_ops_one_running` index, so exclusion is uniform with the
/// migrated verbs — one primitive, no parallel lease.
///
/// `try_acquire` claims immediately or gives up (withdrawing its own
/// queued row) — it never waits; a `None` means another op owns the
/// session right now. The holder drives its pipeline inline, stamping
/// progress via [`OpClaim::touch`] / [`OpClaim::spawn_heartbeat`], and
/// calls [`OpClaim::finish`]. A crashed holder leaves the running row
/// for the executor's reclaim sweep, whose verb arm terminally fails it
/// (fence-then-free — the successor to the lease reaper's
/// free-the-lock-and-hope).
pub(crate) struct OpClaim {
    state: SharedState,
    op: SessionOp,
    epoch: i64,
    finished: std::sync::atomic::AtomicBool,
}

impl OpClaim {
    pub(crate) async fn try_acquire(
        state: &SharedState,
        session_id: SessionId,
        kind: OpKind,
        payload: serde_json::Value,
    ) -> Result<Option<Self>, engram_core::MetaError> {
        // ADR 0079 (review finding #8): claim-or-fail ATOMICALLY. The old
        // enqueue-then-`op_cancel_by_id` shape left a `queued` row between
        // the INSERT commit and the cancel that the executor could claim
        // and run the FULL verb (an unrequested relocation) after the
        // caller had already been told "busy". The exclusive claim inserts
        // + claims in one transaction and ROLLS BACK if the lane is busy,
        // so no grabbable row is ever left behind.
        match state
            .services
            .meta
            .op_enqueue_and_claim_exclusive(session_id, kind, payload, &pod_id())
            .await?
        {
            Some(op) => {
                let epoch = op.epoch.expect("claimed op carries its epoch");
                Ok(Some(Self {
                    state: state.clone(),
                    op,
                    epoch,
                    finished: std::sync::atomic::AtomicBool::new(false),
                }))
            }
            None => Ok(None),
        }
    }

    /// The claim's host-RPC / PG-write fence.
    pub(crate) fn fence(&self) -> SessionFence {
        SessionFence {
            session_id: self.op.session_id,
            epoch: self.epoch as u64,
        }
    }

    /// View the claim as a verb-execution context, so an inline holder
    /// can drive a REAL step-recorded pipeline (`run_evict_pipeline`)
    /// synchronously under its claim — the admin evacuate/drain shape.
    pub(crate) fn as_ctx(&self) -> OpCtx<'_> {
        OpCtx {
            state: &self.state,
            op: &self.op,
            epoch: self.epoch,
        }
    }

    /// Stamp progress (heartbeat) on the running row. The tri-state
    /// mirrors the retired lease's `touch_checked`: `Held` = still ours,
    /// `Lost` = a successor re-claimed (authoritative — stop),
    /// `TransientError` = a PG blip the caller may retry through.
    pub(crate) async fn touch(&self, step: &str) -> OpTouch {
        match self
            .state
            .services
            .meta
            .op_record_step(self.op.id, self.epoch, step)
            .await
        {
            Ok(true) => OpTouch::Held,
            Ok(false) => OpTouch::Lost,
            Err(e) => OpTouch::TransientError(e),
        }
    }

    /// Background heartbeat for straight-line pipelines with no touch
    /// loop of their own (the manual snapshot's capture body). Keeps the
    /// running row's `heartbeat_at` fresh so the reclaim sweep (180s
    /// staleness) never fences a healthy holder. Dropping the returned
    /// handle aborts the loop (RAII, tied to the claim's scope).
    pub(crate) fn spawn_heartbeat(&self, step: &'static str) -> OpClaimHeartbeat {
        let state = self.state.clone();
        let op_id = self.op.id;
        let epoch = self.epoch;
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                match state.services.meta.op_record_step(op_id, epoch, step).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(
                            op_id,
                            "inline op-claim heartbeat fenced (successor re-claimed); \
                             the fenced writes are the authoritative safety net",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(op_id, error = %e, "inline op-claim heartbeat transport error; will retry");
                    }
                }
            }
        });
        OpClaimHeartbeat { handle }
    }

    /// Finish the claim's row. Fenced — finishing a row a successor
    /// re-claimed is a no-op.
    pub(crate) async fn finish(&self, outcome: OpState, error: Option<&str>) {
        self.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self
            .state
            .services
            .meta
            .op_finish(self.op.id, self.epoch, outcome, error)
            .await;
        // Completion re-drive: a queued op behind this inline claim must
        // not wait for the fallback poll.
        let state = self.state.clone();
        let session_id = self.op.session_id;
        tokio::spawn(async move {
            drive_session(&state, session_id).await;
        });
    }
}

impl Drop for OpClaim {
    fn drop(&mut self) {
        if self.finished.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Backstop for early-return/panic paths that skipped the explicit
        // `finish`: fail the row so the session's op lane frees now
        // rather than waiting out the reclaim sweep. Best-effort +
        // epoch-fenced, mirroring the retired lease guard's Drop.
        let state = self.state.clone();
        let op_id = self.op.id;
        let epoch = self.epoch;
        let session_id = self.op.session_id;
        tokio::spawn(async move {
            let _ = state
                .services
                .meta
                .op_finish(
                    op_id,
                    epoch,
                    OpState::Failed,
                    Some("inline claim dropped without an explicit finish"),
                )
                .await;
            drive_session(&state, session_id).await;
        });
    }
}

/// RAII handle for an inline claim's heartbeat task.
pub(crate) struct OpClaimHeartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for OpClaimHeartbeat {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// ADR 0079 (review finding #1): the within-step liveness heartbeat for
/// an EXECUTOR-driven verb (the counterpart to [`OpClaim::spawn_heartbeat`]
/// for inline claims). Bumps `heartbeat_at` — and ONLY `heartbeat_at`, via
/// `op_heartbeat`, so the step marker is untouched — every
/// [`OP_HEARTBEAT_INTERVAL`] while the verb body runs. Dropping the guard
/// aborts the loop (RAII, tied to the `drive_claimed` scope). A `false`
/// (successor re-claimed) stops the loop early; the fenced writes are the
/// authoritative safety net either way.
fn spawn_op_heartbeat(state: &SharedState, op_id: i64, epoch: i64) -> OpHeartbeat {
    let state = state.clone();
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(OP_HEARTBEAT_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // consume the immediate first tick
        loop {
            tick.tick().await;
            match state.services.meta.op_heartbeat(op_id, epoch).await {
                Ok(true) => {}
                Ok(false) => {
                    // Fenced: a successor re-claimed this op. Stop beating;
                    // the in-flight body's next fenced write stops it too.
                    break;
                }
                Err(e) => {
                    tracing::warn!(op_id, error = %e,
                        "within-step op heartbeat transport error; will retry");
                }
            }
        }
    });
    OpHeartbeat { handle }
}

/// RAII handle for an executor-driven op's within-step heartbeat.
struct OpHeartbeat {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for OpHeartbeat {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Outcome of an [`OpClaim::touch`]. `Lost` is authoritative (a
/// successor re-claimed the op — give up ownership); `TransientError` is
/// a PG-transport blip the caller may retry through.
pub(crate) enum OpTouch {
    Held,
    Lost,
    TransientError(engram_core::MetaError),
}

/// The executor loop: one per coordinator pod. Wakes on
/// `pg_notify('session_ops', …)` (via `wake`) or the fallback tick,
/// claims head ops per due session, and drives them. Also owns the
/// reclaim sweep.
pub fn spawn(state: SharedState, wake: Arc<Notify>) -> tokio::task::JoinHandle<()> {
    // Reclaim sweep: fence-then-resume for running ops whose executor
    // died. Part of the executor, not a new scanner.
    {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RECLAIM_SWEEP_INTERVAL).await;
                match state
                    .services
                    .meta
                    .op_reclaim_stale(RECLAIM_STALE, &pod_id())
                    .await
                {
                    Ok(ops) => {
                        for op in ops {
                            ::metrics::counter!(crate::metrics::SESSION_OP_RECLAIMS_TOTAL)
                                .increment(1);
                            tracing::warn!(
                                op_id = op.id,
                                session_id = %op.session_id,
                                kind = op.kind.as_str(),
                                step = ?op.step,
                                "reclaimed stale op (executor died); resuming at recorded step",
                            );
                            let state = state.clone();
                            tokio::spawn(async move {
                                drive_claimed(&state, op).await;
                            });
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "op reclaim sweep failed"),
                }

                // ADR 0079 (review finding #5): pending-orphan backstop —
                // re-enqueue create_boot for a session that was placed
                // (`queued → pending`) but lost its create_boot op (a
                // crash between the flip and the enqueue, or a
                // terminal-Failed create_boot whose fenced Failed flip
                // also errored). The op verb re-reads host_id from the
                // row. `PENDING_ORPHAN_GRACE` keeps a just-placed session
                // (op enqueue still in flight) out of the sweep.
                match state
                    .services
                    .meta
                    .orphaned_pending_sessions(PENDING_ORPHAN_GRACE)
                    .await
                {
                    Ok(ids) => {
                        for session_id in ids {
                            // R3 (#722): REVIVE, always. The D7 stack failed an
                            // aged orphan here instead of reviving it, because
                            // placement's crash-orphan exclusion had already
                            // WRITTEN OFF its reservation — so a revived boot
                            // would have over-packed the host (Σ reserved >
                            // allocatable). That exclusion is GONE: a `pending`
                            // now reserves its slot UNCONDITIONALLY for as long
                            // as it is `pending` (ONE reservation authority), so
                            // reviving it is always safe — the boot lands on the
                            // host placement never re-sold. Failing a
                            // crash-orphaned session that could still boot was
                            // user-hostile; this restores the ADR 0079 #5
                            // intent: an orphan (crash between the flip and the
                            // enqueue, or a terminal-Failed create_boot whose
                            // fenced flip also errored) is re-driven to boot. A
                            // genuinely-doomed boot exhausts the op's 30-attempt
                            // budget → terminal Failed → the verb's fenced flip
                            // clears it; the reservation releases with that real
                            // transition (the sole reclaimer), never a
                            // placement-side write-off.
                            ::metrics::counter!(
                                crate::metrics::SESSION_OP_PENDING_ORPHANS_RECOVERED_TOTAL
                            )
                            .increment(1);
                            tracing::warn!(
                                %session_id,
                                "reclaim sweep: orphaned Pending session (no create_boot op); \
                                 re-enqueueing create_boot",
                            );
                            // Fresh key so the re-enqueue always lands (a
                            // stale terminal keyed row no longer blocks it,
                            // review finding #4); the verb reads host_id
                            // from the session row.
                            let key = format!("boot-recover:{session_id}");
                            let _ = enqueue(
                                &state,
                                session_id,
                                OpKind::CreateBoot,
                                serde_json::json!({ "recovered": true }),
                                Some(&key),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "pending-orphan backstop scan failed")
                    }
                }
            }
        });
    }

    tokio::spawn(async move {
        let inflight: Arc<dashmap::DashSet<SessionId>> = Arc::new(dashmap::DashSet::new());
        loop {
            let due = match state.services.meta.op_due_sessions().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "op executor: due-session scan failed");
                    Vec::new()
                }
            };
            for session_id in due {
                if !inflight.insert(session_id) {
                    continue; // this pod is already driving the session
                }
                let state = state.clone();
                let inflight = inflight.clone();
                tokio::spawn(async move {
                    drive_session(&state, session_id).await;
                    inflight.remove(&session_id);
                });
            }
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(RESCAN_INTERVAL) => {}
            }
        }
    })
}

/// Claim-and-drive this session's queue until empty or not-claimable
/// (another pod won, or head not due). `pub(crate)` so tests can drive
/// enqueued ops deterministically without the executor loop.
///
/// ADR 0079 (review finding #13): this is the single ITERATIVE drain
/// loop. It calls [`drive_one`] (which does NOT re-drive), so a burst of
/// N queued ops for a session drains at O(1) stack depth — the old
/// `drive_claimed → Box::pin(drive_session) → drive_claimed …` shape
/// nested one future per op.
/// Drive one session's op pipeline to its next yield point. `pub` so the
/// DST harness (engram-dst, ADR 0098 D5) steps it directly.
pub async fn drive_session(state: &SharedState, session_id: SessionId) {
    loop {
        let op = match state
            .services
            .meta
            .op_claim_head(session_id, &pod_id())
            .await
        {
            Ok(Some(op)) => op,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(session_id = %session_id, error = %e, "op claim failed");
                return;
            }
        };
        drive_one(state, op).await;
    }
}

/// Drive one CLAIMED op to a terminal row state, then continue the
/// session's queue via the iterative [`drive_session`] loop (completion
/// re-drive: a busy session never waits for a notify). `pub(crate)` for
/// deterministic test driving. The continuation is a LOOP, not a
/// recursive self-call — [`drive_session`] invokes [`drive_one`] directly.
/// Drive an ALREADY-CLAIMED op (the reclaim sweep's continuation).
/// `pub` so the DST harness mirrors the sweep (engram-dst, ADR 0098 D5).
pub async fn drive_claimed(state: &SharedState, op: SessionOp) {
    let session_id = op.session_id;
    drive_one(state, op).await;
    // Completion re-drive — iterative (review finding #13).
    drive_session(state, session_id).await;
}

/// Drive exactly one CLAIMED op to its terminal row state. No re-drive —
/// the caller ([`drive_session`]'s loop, or [`drive_claimed`]) owns
/// continuation.
async fn drive_one(state: &SharedState, op: SessionOp) {
    let epoch = op.epoch.expect("claimed op carries its epoch");
    // Enqueue→claim latency, from when the row became CLAIMABLE — for a
    // backed-off retry that's `not_before`, not `created_at` (re-review:
    // measuring retries from creation records the whole prior attempt's
    // duration and permanently pollutes the p99 that proves the executor
    // is NOTIFY-hot; a real poll-hop regression would be invisible).
    let due_at = op
        .not_before
        .map_or(op.created_at, |nb| nb.max(op.created_at));
    let claim_age_ms = (state.services.clock.now_utc() - due_at).num_milliseconds();
    ::metrics::histogram!(crate::metrics::SESSION_OP_CLAIM_LATENCY_SECONDS)
        .record((claim_age_ms.max(0) as f64) / 1000.0);
    let ctx = OpCtx {
        state,
        op: &op,
        epoch,
    };
    // ADR 0079 (review finding #1): a within-step liveness heartbeat runs
    // for the whole verb body, so a step bracketing a long host RPC keeps
    // the reclaim sweep away from a healthy-but-slow executor. Dropped
    // (RAII abort) before `op_finish` so the beat never re-stamps a
    // just-finished row.
    let heartbeat = spawn_op_heartbeat(state, op.id, epoch);
    // ADR 0079 backstop / the 2026-07-17 resume-stall incident (session
    // 03e6535e): the within-step liveness heartbeat above PROVES the
    // executor is alive independently of step progress — by design, so a
    // long-but-healthy host RPC is never reclaimed. Its failure mode is an
    // executor that is alive but WEDGED: a verb `await` that never returns
    // (there, `host.start_agent` on a resume `finish` step hung on a dead
    // rootfs device) keeps the row heartbeating forever, so the 180s
    // stale-op reclaim can never fire and the op is pinned until a deploy
    // rolls the pod (34 minutes, in the incident). The per-RPC
    // `grpc-timeout`s (restore/start_agent, 240s) are the first line; this
    // is the backstop for a hang that is NOT a single bounded RPC (a lock,
    // a channel wait, a future whose timeout isn't honoured). On expiry the
    // dispatch future is DROPPED (cancelled) and the op requeues with
    // backoff. Uses the tokio timer (not the injected clock) so it bounds
    // REAL wall-clock hangs — and so the simulator's paused-clock advance
    // fires it deterministically.
    //
    // CANCELLATION SAFETY (adversarial-review finding): dropping the
    // dispatch future is only equivalent to an executor crash for verbs
    // whose mid-flight cancellation leaves NO unfenced host-side cleanup
    // racing a successor. On a real crash the whole process dies, taking
    // any detached cleanup task with it, and nothing re-claims the lane for
    // 180s; a timeout, by contrast, keeps the process alive and frees the
    // lane immediately. For a capture/migration verb that is exactly the
    // hazard: cancelling `snapshot`/`snapshot_begin` (Evict,
    // CheckpointFinalize) triggers the host's CaptureUnwind — guest-resume
    // + disk-drain requeue on the SHARED sandbox — which would then run
    // concurrently with the successor op (a retry's fresh capture, or a
    // Deliver/Resume against the same guest), corrupting snapshot
    // quiescence. So `op_deadline` returns `None` for those verbs (they keep
    // their own bounds — the ADR-0090 quarantine capture timeout, and the
    // 180s reclaim on genuine executor death); Resume/CreateBoot leave only
    // an orphan-reaped half-restore or a reattach-idempotent half-spawn, and
    // Deliver/Destroy have no shared-sandbox cleanup, so those are bounded.
    // Stamp the attempt start on the injected clock: the requeue arms
    // below pace an op's next claim by its OWN attempt's elapsed time
    // in addition to the backoff — see the pacing note at the arms.
    let attempt_started = state.services.clock.now_utc();
    let outcome = match op_deadline(op.kind) {
        Some(deadline) => {
            match tokio::time::timeout(deadline, crate::session_verbs::dispatch(&ctx)).await {
                Ok(outcome) => outcome,
                Err(_) => OpOutcome::Retry(format!(
                    "op exceeded wall-clock deadline {deadline:?} at step {:?} (executor alive \
                     but wedged); requeued with backoff",
                    op.step.as_deref().unwrap_or("<start>"),
                )),
            }
        }
        None => crate::session_verbs::dispatch(&ctx).await,
    };
    drop(heartbeat);
    let meta = &state.services.meta;
    let terminal = !matches!(outcome, OpOutcome::Retry(_) | OpOutcome::RetryAfter(_, _));
    let finished_done = matches!(outcome, OpOutcome::Done);
    let _ = match outcome {
        OpOutcome::Done => meta.op_finish(op.id, epoch, OpState::Done, None).await,
        OpOutcome::Cancelled => meta.op_finish(op.id, epoch, OpState::Cancelled, None).await,
        OpOutcome::Failed(e) => {
            tracing::warn!(op_id = op.id, session_id = %op.session_id, kind = op.kind.as_str(), error = %e, "op failed terminally");
            meta.op_finish(op.id, epoch, OpState::Failed, Some(&e))
                .await
        }
        // Both retry arms requeue with `attempt_elapsed + delay`, never
        // the bare delay. DST finding (ADR 0108 swarm, seed 33043259):
        // an attempt whose dispatch burns LONGER than its requeue delay
        // — a hung host RPC resolved only by `op_deadline` (Deliver
        // 120s) or by an in-verb bound (evict's capture timeout) —
        // re-arms every SIBLING op on the session whose ≤60s-capped
        // backoff it just outlasted. Two such ops mutually re-arm:
        // each one's burn makes the other due again, `drive_session`'s
        // claim loop never runs dry, and the lane churns hung RPCs
        // back-to-back at 100% duty forever (measured: 854k claims /
        // 426k attempts on one Deliver op, three virtual years inside
        // ONE simulator step; Deliver has no attempt budget, so the
        // pair is immortal). Adding the attempt's own elapsed time
        // makes a PAIR provably terminate: a 2-cycle needs each burn to
        // reach the other op's burn+delay, and summing both gives
        // B_a + B_b ≥ B_a + B_b + d_a + d_b — impossible for delays
        // > 0. Cycles of N≥3 ops can still self-sustain under any
        // per-op pacing ((N-2)·Σburns ≥ Σdelays is satisfiable), which
        // is why the op POPULATION is bounded too: Resume and Evict
        // carry attempt budgets, and the deliver verb sweeps duplicate
        // Deliver ops on claim (see `deliver`'s duplicate-sweep note).
        // A `max(delay, elapsed)` clamp is NOT enough: it recreates the
        // exact boundary (`not_before <= now`) every time the sibling's
        // burn equals the pace, and the loop churns on. Fast failures
        // (elapsed ≈ 0) keep their exact cadence, so the hot path and
        // the ADR 0108 A5 fixed-cadence intent are unchanged — pacing
        // is simply never finer than the work it paces, strictly.
        // Event wakes (`op_wake_queued_kind`) still cut every pace
        // short, so recovery latency stays wake-driven, not poll-bound.
        OpOutcome::Retry(e) => {
            let paced = attempt_elapsed(state, attempt_started) + backoff(op.attempts);
            tracing::debug!(op_id = op.id, session_id = %op.session_id, kind = op.kind.as_str(), attempts = op.attempts, delay_ms = paced.as_millis() as u64, error = %e, "op deferred (retryable)");
            meta.op_requeue_with_backoff(op.id, epoch, paced, &e).await
        }
        OpOutcome::RetryAfter(delay, e) => {
            let paced = attempt_elapsed(state, attempt_started) + delay;
            tracing::debug!(op_id = op.id, session_id = %op.session_id, kind = op.kind.as_str(), attempts = op.attempts, delay_ms = paced.as_millis() as u64, error = %e, "op deferred (fixed cadence)");
            meta.op_requeue_with_backoff(op.id, epoch, paced, &e).await
        }
    };
    // ADR 0079 + ADR 0094: the initial-prompt DELIVER op is deferred
    // while the session is still booting ("session is pending — not
    // deliverable") and requeued on a growing backoff. Nothing else makes
    // it ready when the boot completes, so the prompt would wait out the
    // accumulated backoff (fresh create: ~36 s — the dominant TTFM cost,
    // measured on the dev VM; claude answers in ~2 s once it has the
    // prompt). The op that drives the row to Active wakes the sibling
    // DELIVER on success, so the loop's next claim forwards the prompt in
    // <100 ms. Two boot ops enqueue a sibling deliver:
    //   - `Resume{flavor=for_delivery}` — prompt-after-idle (ADR 0079).
    //   - `CreateBoot` — the fresh-create boot (ADR 0094; the original
    //     "40 s = guest stampede" reading was wrong — it was this backoff).
    //
    // Gated on Done-or-session-terminal (re-review): a boot/resume that
    // fails terminally while the session stays RESUMABLE (a deterministic
    // non-`gone:` failure, e.g. a corrupt disk-only manifest) must NOT
    // wake the deliver — the woken deliver would instantly enqueue a fresh
    // boot/resume, whose failure wakes it again, resetting the deliver's
    // growing backoff every cycle into an unpaced failure loop. The
    // session-terminal arm keeps the `gone:`/failed path fast (session
    // flipped terminal → the woken deliver drops its rows and completes).
    // Never fired while the boot merely retries — the deliver stays
    // backed off.
    let wakes_sibling_deliver = match op.kind {
        OpKind::CreateBoot => true,
        OpKind::Resume => op.payload.get("flavor").and_then(|f| f.as_str()) == Some("for_delivery"),
        _ => false,
    };
    if terminal && wakes_sibling_deliver {
        let wake = if finished_done {
            true
        } else {
            matches!(
                meta.get_session(op.session_id).await,
                Ok(s) if s.status.is_terminal()
            )
        };
        if wake {
            // ADR 0108 A4: the boot/resume just completed, so the
            // harness attach is expected within the grace window. Stamp
            // it BEFORE the wake — the woken deliver consults the stamp
            // and must never observe the pre-stamp state.
            state
                .attach_grace
                .insert(op.session_id, state.services.clock.now_utc());
            if let Err(e) = meta
                .op_wake_queued_kind(op.session_id, OpKind::Deliver)
                .await
            {
                tracing::debug!(session_id = %op.session_id, kind = op.kind.as_str(), error = %e, "sibling deliver wake after boot failed (5s poll backstops)");
            }
        }
    }
}

/// The attempt's own duration on the injected clock — the pacing floor
/// the retry arms in [`drive_one`] add to their delay (see the note
/// there). Saturates to zero if the clock reads backwards.
fn attempt_elapsed(
    state: &SharedState,
    attempt_started: chrono::DateTime<chrono::Utc>,
) -> Duration {
    state
        .services
        .clock
        .now_utc()
        .signed_duration_since(attempt_started)
        .to_std()
        .unwrap_or_default()
}

/// Linear backoff, capped — mirrors the outbox driver's posture: an op
/// that can't run shouldn't spin the executor, and there is deliberately
/// no silent give-up (terminal failure is an explicit verb decision).
fn backoff(attempts: i32) -> Duration {
    let secs = ((attempts.max(0) as u64) + 1) * 2;
    Duration::from_secs(secs.min(60))
}

/// Wall-clock backstop for a single op-dispatch attempt (see the deadline
/// wrap in [`drive_one`]). NOT a pacing knob — sized generously ABOVE the
/// sum of the per-RPC `grpc-timeout`s a healthy attempt can legitimately
/// spend, so it only ever fires on a genuinely wedged executor. On expiry
/// the attempt requeues with backoff; a fresh attempt gets a fresh
/// deadline (this is per-attempt, never cumulative across retries).
///
/// Returns `None` for verbs whose mid-flight cancellation is NOT
/// crash-equivalent — capture (Evict/CheckpointFinalize) and migration
/// (Teleport), where dropping the host RPC spawns unfenced CaptureUnwind /
/// abort cleanup on the shared sandbox that would race a successor op (see
/// the cancellation-safety note in [`drive_one`]). Those verbs keep their
/// existing bounds: the ADR-0090 quarantine capture timeout and the 180s
/// reclaim on genuine executor death.
fn op_deadline(kind: OpKind) -> Option<Duration> {
    let deadline = match kind {
        // The worst legitimate case: restore (≤240s grpc) in the `restore`
        // step THEN start_agent (≤240s grpc) in the `finish` step, plus
        // secret/token minting and PG writes — up to ~500s. 600s clears it.
        // A cancelled attempt leaves only an orphan-reaped half-restore or a
        // reattach-idempotent half-spawn — no shared-sandbox cleanup race.
        OpKind::Resume | OpKind::CreateBoot => Duration::from_secs(600),
        // Forward one outbox row (30s ACK budget) / tear down — fast, and
        // no shared-sandbox unwind on cancel (retry re-forwards/re-destroys).
        OpKind::Deliver | OpKind::Destroy => Duration::from_secs(120),
        // Capture + migration: cancellation is unsafe (see doc comment).
        OpKind::Evict | OpKind::CheckpointFinalize | OpKind::Teleport => return None,
    };
    // Operators can pin a single global backstop (and tests set a short one)
    // via `ENGRAM_OP_DEADLINE_SECS` — applied only to deadline-eligible
    // verbs, never to force one onto a cancel-unsafe verb. Mirrors the
    // `ENGRAM_QUARANTINE_CAPTURE_TIMEOUT_SECS` knob.
    let deadline = std::env::var("ENGRAM_OP_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(deadline, Duration::from_secs);
    Some(deadline)
}

/// This pod's identity — observability only, never authority (the epoch
/// is the authority).
pub fn pod_id() -> String {
    static POD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    POD.get_or_init(|| {
        std::env::var("HOSTNAME").unwrap_or_else(|_| format!("coord-{}", std::process::id()))
    })
    .clone()
}

#[cfg(test)]
mod deadline_tests {
    use super::op_deadline;
    use engram_core::types::session_op::OpKind;

    /// Cancellation safety (adversarial-review finding): the wall-clock
    /// deadline hard-cancels the dispatch future, so it may ONLY apply to
    /// verbs whose mid-flight cancellation leaves no unfenced host-side
    /// cleanup racing a successor. Capture (Evict/CheckpointFinalize) and
    /// migration (Teleport) spawn CaptureUnwind/abort on the shared sandbox,
    /// so they must have NO deadline; the rest are bounded.
    #[test]
    fn only_cancel_safe_verbs_carry_a_deadline() {
        // Cancel-unsafe: no deadline (keep their own bounds).
        for k in [OpKind::Evict, OpKind::CheckpointFinalize, OpKind::Teleport] {
            assert!(
                op_deadline(k).is_none(),
                "{k:?} must not be deadline-cancelled"
            );
        }
        // Cancel-safe: bounded.
        for k in [
            OpKind::Resume,
            OpKind::CreateBoot,
            OpKind::Deliver,
            OpKind::Destroy,
        ] {
            assert!(
                op_deadline(k).is_some(),
                "{k:?} must carry a wall-clock deadline"
            );
        }
    }
}
