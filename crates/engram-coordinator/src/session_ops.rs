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
    let pod = pod_id();
    let outcome = state
        .services
        .meta
        .op_enqueue_and_claim(session_id, kind, payload, idempotency_key, &pod)
        .await?;
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
) -> Result<SessionState, engram_core::MetaError> {
    if fence.epoch == 0 {
        return state.services.meta.transition_session(session_id, to).await;
    }
    match state
        .services
        .meta
        .fenced_transition_session(session_id, fence.epoch as i64, to)
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
pub(crate) async fn drive_session(state: &SharedState, session_id: SessionId) {
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
pub(crate) async fn drive_claimed(state: &SharedState, op: SessionOp) {
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
    let claim_age_ms = (chrono::Utc::now() - due_at).num_milliseconds();
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
    let outcome = crate::session_verbs::dispatch(&ctx).await;
    drop(heartbeat);
    let meta = &state.services.meta;
    let terminal = !matches!(outcome, OpOutcome::Retry(_));
    let finished_done = matches!(outcome, OpOutcome::Done);
    let _ = match outcome {
        OpOutcome::Done => meta.op_finish(op.id, epoch, OpState::Done, None).await,
        OpOutcome::Cancelled => meta.op_finish(op.id, epoch, OpState::Cancelled, None).await,
        OpOutcome::Failed(e) => {
            tracing::warn!(op_id = op.id, session_id = %op.session_id, kind = op.kind.as_str(), error = %e, "op failed terminally");
            meta.op_finish(op.id, epoch, OpState::Failed, Some(&e))
                .await
        }
        OpOutcome::Retry(e) => {
            tracing::debug!(op_id = op.id, session_id = %op.session_id, kind = op.kind.as_str(), attempts = op.attempts, error = %e, "op deferred (retryable)");
            meta.op_requeue_with_backoff(op.id, epoch, backoff(op.attempts), &e)
                .await
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
            if let Err(e) = meta
                .op_wake_queued_kind(op.session_id, OpKind::Deliver)
                .await
            {
                tracing::debug!(session_id = %op.session_id, kind = op.kind.as_str(), error = %e, "sibling deliver wake after boot failed (5s poll backstops)");
            }
        }
    }
}

/// Linear backoff, capped — mirrors the outbox driver's posture: an op
/// that can't run shouldn't spin the executor, and there is deliberately
/// no silent give-up (terminal failure is an explicit verb decision).
fn backoff(attempts: i32) -> Duration {
    let secs = ((attempts.max(0) as u64) + 1) * 2;
    Duration::from_secs(secs.min(60))
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
