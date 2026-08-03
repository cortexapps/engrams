//! ADR 0079: the verb registry — each lifecycle verb's step sequence,
//! driven by the executor in `session_ops.rs`. Bodies are
//! idempotent-from-step: given (kind, payload, step) they perform only
//! the remaining work; every session write rides the fenced `MetadataStore`
//! family and every host RPC carries the op's epoch.

use engram_core::types::session_op::OpKind;
use engram_core::types::BindingDisposition;
use engram_core::types::SessionState;

use crate::error::ApiError;
use crate::session_ops::{OpCtx, OpOutcome};
use crate::state::{SessionEvent, SharedState};

pub async fn dispatch(ctx: &OpCtx<'_>) -> OpOutcome {
    match ctx.op.kind {
        OpKind::Resume => resume(ctx).await,
        OpKind::Evict => evict(ctx).await,
        OpKind::Deliver => deliver(ctx).await,
        OpKind::CreateBoot => create_boot(ctx).await,
        OpKind::Destroy => destroy(ctx).await,
        // Inline-claim kinds (session_ops::OpClaim): these rows are
        // normally driven to a terminal state by their inline holder and
        // only reach the executor when that holder died mid-pipeline and
        // the reclaim sweep re-claimed the row. Neither pipeline is
        // re-drivable from a step marker yet (they migrate to real verbs
        // in follow-up phases), so terminally fail the row — this FREES
        // the session's op lane (fence-then-free, the successor to the
        // retired lease reaper) and the existing recovery machinery owns
        // the rest (periodic checkpoints for a torn manual snapshot; the
        // ADR 0018 parachute / evac scanner for a torn teleport).
        OpKind::CheckpointFinalize => OpOutcome::Failed(
            "manual snapshot claim abandoned (holder died); safe to retry the snapshot".into(),
        ),
        OpKind::Teleport => OpOutcome::Failed(
            "teleport claim abandoned (holder died); the parachute/evac machinery owns \
             recovery — safe to re-issue the move"
                .into(),
        ),
    }
}

/// Map a pipeline `ApiError` into the verb outcome. `Conflict` /
/// `Unavailable` are retryable (requeue with backoff); `Gone` is
/// terminal and tagged `gone:` so the bounded-observe entry points can
/// surface an honest 410 to the caller; everything else is terminal.
fn outcome_from_api_error(e: ApiError) -> OpOutcome {
    match e {
        ApiError::Conflict(m) => OpOutcome::Retry(m),
        ApiError::Unavailable(m) => OpOutcome::Retry(m),
        ApiError::Gone(m) => OpOutcome::Failed(format!("gone: {m}")),
        other => OpOutcome::Failed(other.to_string()),
    }
}

/// Resume-verb retry budget (ADR 0079 review finding #1, livelock
/// terminator). The within-step heartbeat already means a slow restore is
/// never reclaimed mid-flight, so a resume completes normally; this budget
/// is the defense-in-depth bound that terminates a resume op that keeps
/// failing (a genuinely unresumable snapshot, or an evict ahead of it that
/// never settles) instead of requeueing forever. Generous: at the capped
/// ≤60s backoff, ~60 attempts is ~an hour of continuous failure — well
/// past any legitimate evict-ahead wait — before the terminal `gone:`.
const RESUME_MAX_ATTEMPTS: i32 = 60;

/// After this many attempts the resume crash-shortcut (finish-only on a
/// prior binding) is presumed to be latching a STALE binding and falls
/// through to a full re-restore (review finding #10). Small — a live
/// binding's finish succeeds on the first try; only a dead one keeps
/// failing.
const SHORTCUT_MAX_ATTEMPTS: i32 = 3;

/// The resume verb: bring an Idle / Created session back to Active.
/// Steps: `dispatch → restore → bind → finish` (the restore/bind/finish
/// markers are recorded inside `api::snapshot`'s pipeline functions,
/// which this verb owns exclusively now). Wraps [`resume_inner`] with the
/// retry-budget livelock terminator.
async fn resume(ctx: &OpCtx<'_>) -> OpOutcome {
    match resume_inner(ctx).await {
        OpOutcome::Retry(e) if ctx.op.attempts >= RESUME_MAX_ATTEMPTS => {
            ::metrics::counter!(crate::metrics::SESSION_OP_RESUME_BUDGET_EXHAUSTED_TOTAL)
                .increment(1);
            tracing::warn!(
                session_id = %ctx.op.session_id,
                attempts = ctx.op.attempts,
                error = %e,
                "resume op retry budget exhausted; failing terminally (gone)",
            );
            // ADR 0090: a session still parked at `Created` when the budget
            // exhausts is the harness-start wedge class — failing only the
            // OP row left the SESSION at Created forever with nothing but a
            // 409 for the user (campaign B1: 16+ min of "agentd is not yet
            // ready" and no terminal state). Flip it Failed with a
            // user-visible event. An `Idle` session is left alone: its
            // durable state is intact and a later resume can succeed.
            fail_wedged_created_session(ctx, &e).await;
            OpOutcome::Failed(format!(
                "gone: resume did not complete after {} attempts: {e}",
                ctx.op.attempts
            ))
        }
        other => other,
    }
}

/// Budget-exhaustion terminal flip for a resume that left the session at
/// `Created` (the ADR 0090 harness-start wedge). Best-effort: a fenced
/// transition failure (successor claimed / state moved on) logs and leaves
/// the op's terminal `Failed` as the only record.
async fn fail_wedged_created_session(ctx: &OpCtx<'_>, reason: &str) {
    let state = ctx.state;
    let id = ctx.op.session_id;
    match state.services.meta.get_session(id).await {
        Ok(s) if s.status == SessionState::Created => {}
        _ => return,
    }
    if let Err(e) = state
        .services
        .meta
        .append_session_event(
            id,
            "harness_start_failed",
            serde_json::json!({
                "reason": "resume retry budget exhausted; the harness never started",
                "detail": reason,
                "attempts": ctx.op.attempts,
            }),
        )
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "harness_start_failed event failed");
    }
    match crate::session_ops::transition_with_fence(
        state,
        id,
        ctx.fence(),
        SessionState::Failed,
        BindingDisposition::Detach,
    )
    .await
    {
        Ok(prev) => {
            let _ = state
                .emit_fenced(
                    id,
                    ctx.fence(),
                    SessionEvent::StatusChanged {
                        from: prev,
                        to: SessionState::Failed,
                        at: state.services.clock.now_utc(),
                    },
                )
                .await;
        }
        Err(e) => tracing::warn!(session_id = %id, error = %e,
            "resume budget exhausted but Created→Failed flip failed (state moved on?)"),
    }
}

async fn resume_inner(ctx: &OpCtx<'_>) -> OpOutcome {
    if !ctx.step("dispatch").await {
        return OpOutcome::Failed("fenced at dispatch".into());
    }
    // Cooperative cancel before any side effect: the session is exactly
    // as we found it (Idle stays Idle, etc.).
    if ctx.cancel_requested().await {
        return OpOutcome::Cancelled;
    }
    let state = ctx.state;
    let id = ctx.op.session_id;
    let session = match state.services.meta.get_session(id).await {
        Ok(s) => s,
        Err(engram_core::MetaError::NotFound) => {
            return OpOutcome::Failed("gone: session row no longer exists".into())
        }
        Err(e) => return OpOutcome::Retry(format!("get_session: {e}")),
    };

    // Crash-resume shortcut (idempotent-from-step): a prior attempt of
    // THIS op already restored a VM and bound it (step >= "bind" and the
    // row carries a live binding) — re-running the full dispatch would
    // double-restore from the same snapshot. Skip straight to the finish
    // leg (harness rebuild + the Active flip), which is idempotent.
    //
    // A recorded "restore" step WITHOUT a binding means the restore
    // never committed its bind — re-run from dispatch; if the previous
    // attempt's VM actually came up, it is unreferenced by any session
    // row and the host's ownership-oracle orphan reap GCs it (the
    // replacement for the deleted resume_from_idle residual-destroy
    // compensation).
    //
    // Review finding #10: the shortcut is BOUNDED. A STALE binding (an
    // evict's `fenced_assign_sandbox(None)` errored, leaving Idle+bound
    // over a destroyed VM) makes `finish_resume_to_active` fail Unavailable
    // every retry, re-entering the shortcut forever. After
    // `SHORTCUT_MAX_ATTEMPTS` failed finishes we fall through to full
    // dispatch (`resume_from_idle` restores a FRESH VM and rebinds,
    // overwriting the stale binding).
    if let (Some("bind" | "finish"), Some(sandbox_id), SessionState::Idle | SessionState::Created) =
        (ctx.resume_point(), session.sandbox_id, session.status)
    {
        if ctx.op.attempts <= SHORTCUT_MAX_ATTEMPTS {
            if !ctx.step("finish").await {
                return OpOutcome::Failed("fenced at finish".into());
            }
            return match crate::api::snapshot::finish_resume_to_active(
                state,
                &session,
                sandbox_id,
                true,
                ctx.fence(),
            )
            .await
            {
                Ok(crate::api::snapshot::FinishResumeOutcome::Active) => OpOutcome::Done,
                // 2026-07-21 livelock incident (prod 8174b7aa): a failed
                // harness start parked the session at Created and this arm
                // read `Ok(_) => Done` — the op "succeeded" while the
                // session sat wedged with nothing owning it. Mirror
                // `resume_from_created` (its ADR 0090 comment: "retryable,
                // not a 200 — the resume op's backoff + budget own the
                // retry"): Retry, so a transient start_agent flake heals
                // in-op, the stale-binding case falls through to full
                // dispatch after SHORTCUT_MAX_ATTEMPTS, and a truly dead
                // harness terminates VISIBLY via the budget's
                // Created → Failed flip instead of a silent Done.
                Ok(crate::api::snapshot::FinishResumeOutcome::CreatedHarnessFailed(e)) => {
                    OpOutcome::Retry(format!(
                        "harness start failed after resume (session parked at \
                         Created; will retry): {e}"
                    ))
                }
                Err(e) => outcome_from_api_error(e),
            };
        }
        tracing::warn!(
            session_id = %id,
            %sandbox_id,
            attempts = ctx.op.attempts,
            "resume crash-shortcut kept failing; binding likely stale — \
             falling through to full dispatch (re-restore)",
        );
        // fall through to the match below (full re-restore)
    }

    match session.status {
        // Already there — the op's goal state. Also load-bearing for
        // ordering: a resume op queued behind an in-flight rung-2 ascent
        // (or another resume) must complete idempotently, not spin.
        SessionState::Active => OpOutcome::Done,
        // ADR 0091: the guest is dead/wedged on a live host. Recovery =
        // destroy the dead sandbox (best-effort — it may already be
        // gone), release the binding, flip to Idle, and Retry: the next
        // attempt re-dispatches into the normal Idle resume below, which
        // restores from the latest checkpoint (and arbitrates
        // recoverability, demoting to Dead when nothing usable exists).
        SessionState::Unreachable => {
            if let Some(sandbox_id) = session.sandbox_id {
                if let Err(e) = state.services.host.destroy(sandbox_id, ctx.fence()).await {
                    tracing::warn!(session_id = %id, %sandbox_id, error = %e,
                        "unreachable recovery: destroy of the dead sandbox failed \
                         (continuing — orphan_reap owns stragglers)");
                }
            }
            match state
                .services
                .meta
                .fenced_assign_sandbox(id, ctx.epoch, None, session.host_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    // A successor re-claimed; stop silently.
                    return OpOutcome::Done;
                }
                Err(e) => return OpOutcome::Retry(format!("unreachable recovery: unbind: {e}")),
            }
            match crate::session_ops::transition_with_fence(
                state,
                id,
                ctx.fence(),
                SessionState::Idle,
                BindingDisposition::RequireUnbound,
            )
            .await
            {
                Ok(prev) => {
                    let _ = state
                        .emit_fenced(
                            id,
                            ctx.fence(),
                            SessionEvent::StatusChanged {
                                from: prev,
                                to: SessionState::Idle,
                                at: state.services.clock.now_utc(),
                            },
                        )
                        .await;
                    OpOutcome::Retry("unreachable guest cleared; resuming from checkpoint".into())
                }
                Err(e) => OpOutcome::Retry(format!("unreachable recovery: idle flip: {e}")),
            }
        }
        SessionState::Idle => match crate::api::snapshot::resume_from_idle(ctx, session).await {
            Ok(_) => OpOutcome::Done,
            Err(e) => outcome_from_api_error(e),
        },
        // ADR 0018: an auto-evac'd / harness-failed session parked at
        // Created finishes the harness rebuild.
        SessionState::Created => {
            match crate::api::snapshot::resume_from_created(ctx, session).await {
                Ok(_) => OpOutcome::Done,
                Err(e) => outcome_from_api_error(e),
            }
        }
        // Review finding #9: a direct /resume on a PARKED-PAUSED session
        // (Evicting, park_rung=2, evict op already Done) must ascend — un-
        // pause in ms — not Retry-forever behind a nonexistent evict.
        // Because THIS resume op holds the one-running slot, no evict
        // pipeline is mid-capture: Evicting is the nomination window, a
        // parked-paused VM, or (ADR 0101 C) the post-capture SETTLE
        // window — the ascent itself distinguishes them (it probes VM
        // liveness and refuses the settle window, where the VM is
        // already destroyed). Ascend under our fence; on `false` (mid-
        // eviction we couldn't cancel, or awaiting the settle) fall to
        // the Retry — the settle lands Idle within a heartbeat and the
        // next attempt resumes from the snapshot.
        SessionState::Evicting => {
            match crate::api::snapshot::ascend_evicting_to_active(state, id, ctx.fence()).await {
                Ok(true) => OpOutcome::Done,
                Ok(false) => OpOutcome::Retry(
                    "session is mid-eviction; the resume runs after the evict op".into(),
                ),
                Err(e) => OpOutcome::Retry(format!("rung ascent: {e}")),
            }
        }
        // ADR 0101 C: parked is a real state now — the VM is alive and
        // paused in place; the same ascent machinery un-pauses it (~1s)
        // and flips `Parked → Active`.
        SessionState::Parked => {
            match crate::api::snapshot::ascend_evicting_to_active(state, id, ctx.fence()).await {
                Ok(true) => OpOutcome::Done,
                Ok(false) => OpOutcome::Retry(
                    "session is parked but the un-park did not land; retrying".into(),
                ),
                Err(e) => OpOutcome::Retry(format!("un-park ascent: {e}")),
            }
        }
        // ADR 0079 note: terminal-for-this-op rather than Retry — the
        // evac scanner relocates Evacuating sessions via its own inline
        // claim, and a retrying resume op would sit AHEAD of that claim
        // in the queue and starve it. The wire caller sees the same
        // retryable "relocating" message as before.
        SessionState::Evacuating => OpOutcome::Failed(
            "session is relocating (operator drain / teleport); it will resume \
             automatically — retry shortly"
                .into(),
        ),
        // The queue scanner owns placed-queued sessions; a resume op has
        // nothing to do until it dequeues (which enqueues a fresh op).
        SessionState::Queued | SessionState::Pending => OpOutcome::Failed(format!(
            "session is {} — its scanner owns the next transition; not directly resumable",
            session.status.as_str()
        )),
        SessionState::HostLost => OpOutcome::Failed(
            "session's host went away; resume from a snapshot once the dead-host straggler sweep settles it".into(),
        ),
        SessionState::Dead => OpOutcome::Failed(
            "gone: snapshot_invalidated: session is terminal; chunked manifests are gone \
             or never existed"
                .into(),
        ),
        SessionState::Failed | SessionState::Completed => OpOutcome::Failed(format!(
            "gone: session is {} (terminal); no work to dispatch",
            session.status.as_str()
        )),
    }
}

/// Evict-verb retry budget. Mirrors the retired eviction scanner's
/// `max_attempts = 20` (~3 minutes of continuous pipeline failure at the
/// executor's capped backoff) before the HostLost fallback breaks the
/// loop by construction.
const EVICT_MAX_ATTEMPTS: i32 = 20;

/// Fast-retry budget for the ADR 0090 quarantined-survivor flavor
/// (`payload.quarantine`). The survivor's disk is unserved and its user
/// already degraded — a capture that keeps failing (or timing out; the
/// pipeline bounds each quarantine capture attempt) must leave the fast
/// lane in minutes, not spin the 20-attempt budget while the session
/// lane stays locked (2026-07-13 incident: one wedged evict held the
/// lane for ~50 minutes with the user's resume queued behind it). Past
/// this budget the op PARKS on the slow retry cadence below — it does
/// NOT destroy the VM (2026-08-02 durability-rollback RCA: the old
/// destroy-on-exhaustion arm rewound 11 sessions past acked writes).
const QUARANTINE_EVICT_MAX_ATTEMPTS: i32 = 3;

/// Slow-lane retry cadence for a stuck quarantined survivor. The op
/// stays QUEUED with a future `not_before`, which (a) keeps the
/// `adr0090-quarantine:*` idempotency key live so the host's 5s
/// advertise dedupes to `Duplicate` (no 8174b7aa-style op flood), and
/// (b) never head-of-line blocks the session lane (`op_claim_head`
/// only sees due ops). Each wake-up re-runs the capture, which
/// converges losslessly once the host's rehydrate retry pass re-serves
/// the survivor's disk; until then each attempt fails fast against the
/// host's snapshot refusal.
const QUARANTINE_STUCK_RETRY: std::time::Duration = std::time::Duration::from_secs(120);

/// #810 finding 2 (the Evicting-convergence hole): on evict-budget
/// exhaustion, which flavors MUST settle the session into `HostLost`?
///
/// `HostLost` is the only non-terminal lane the dead-host straggler sweep
/// re-drives (`list_host_lost_sessions` → destroy/settle → Idle-recoverable),
/// so a settled-but-failed eviction that does not land there strands with no
/// re-driver.
///
/// - `nominated`: the eviction was the coordinator's own densification pick;
///   it cannot stay Active (it would just re-nominate forever) and there is no
///   durable snapshot to call it Idle — HostLost is the honest limbo.
/// - `quarantine` (ADR 0090) no longer reaches this flip at all
///   (2026-08-02 durability-rollback RCA): exhaustion parks the op on the
///   slow retry lane instead of destroying the VM, so there is nothing to
///   settle — the session keeps its (crippled, recoverable) sandbox. The
///   pre-#810 stranding this flip fixed came from the destroy; no destroy,
///   no strand.
///
/// Any other flavor (a plain idle-evict that keeps failing) leaves the op
/// terminally Failed and the session where it was — an operator-visible
/// coord-side fault, not a settle that fabricates a lost host.
const fn exhaustion_settles_host_lost(nominated: bool) -> bool {
    nominated
}

/// ADR 0090 / 2026-08-02 durability-rollback RCA: a quarantined
/// survivor's eviction exhausted its fast-retry budget. The old arm
/// DESTROYED the VM and settled `HostLost` — which IS the rollback: the
/// next resume unconditionally rewinds to the last published disk
/// manifest, dropping every acked-but-unuploaded guest write past it
/// (11 sessions between 07-22 and 08-02). The refusal that fails these
/// attempts exists precisely to protect those writes; destroying on its
/// third firing guaranteed the loss it prevented.
///
/// New posture: **park, never destroy.** The VM (and the only copy of
/// its un-uploaded acked writes) stays alive; the op returns
/// [`OpOutcome::RetryAfter`] on the [`QUARANTINE_STUCK_RETRY`] cadence,
/// staying QUEUED so the idempotency key keeps the host's 5s advertise
/// deduped (no 8174b7aa-style livelock) without head-of-line blocking
/// the lane (a not-due op is invisible to `op_claim_head`). Recovery is
/// the host's rehydrate retry pass re-serving the disk, after which the
/// next slow-lane attempt captures + relocates with ZERO loss.
///
/// The stuck crossing is made loud exactly once (at `attempts ==
/// budget`): the alertable `QUARANTINE_STUCK_TOTAL` counter + a WARN.
/// If the disk never recovers (e.g. a de-configured kernel device), the
/// alert is the operator's cue — an explicit `session destroy` /
/// host reboot is an OPERATOR decision accepting the loss, not an
/// automatic ladder outcome.
fn park_quarantined_survivor_stuck(
    ctx: &OpCtx<'_>,
    evict_err: &crate::idle_evictor::EvictError,
) -> OpOutcome {
    let session_id = ctx.op.session_id;
    if ctx.op.attempts == QUARANTINE_EVICT_MAX_ATTEMPTS {
        ::metrics::counter!(crate::metrics::QUARANTINE_STUCK_TOTAL).increment(1);
        tracing::warn!(
            %session_id,
            attempts = ctx.op.attempts,
            error = %evict_err,
            "quarantined-survivor evict exhausted its fast-retry budget; PARKING the op \
             on the slow retry lane (VM preserved, acked writes preserved) — recovery is \
             the host's rehydrate retry re-serving the disk; if this session stays stuck, \
             an operator must intervene (the old arm destroyed the VM here and rewound \
             past acked writes; 2026-08-02 RCA)",
        );
    } else {
        tracing::info!(
            %session_id,
            attempts = ctx.op.attempts,
            error = %evict_err,
            "stuck quarantined-survivor evict retried and still failing; staying on the \
             slow retry lane",
        );
    }
    OpOutcome::RetryAfter(
        QUARANTINE_STUCK_RETRY,
        format!(
            "quarantined survivor unevictable after {} attempts; parked on the \
             {}s retry lane awaiting disk re-serve: {evict_err}",
            ctx.op.attempts,
            QUARANTINE_STUCK_RETRY.as_secs(),
        ),
    )
}

/// The evict verb: the idle-eviction / drain pipeline
/// (`idle_evictor::run_evict_pipeline` — park or capture + destroy +
/// mark-idle). Payload: `{"target": "idle"|"evacuating", "allow_park":
/// bool, "nominated": bool}`; steps `park_or_capture → mark_idle` are
/// recorded inside the pipeline.
async fn evict(ctx: &OpCtx<'_>) -> OpOutcome {
    let payload = &ctx.op.payload;
    let target = match payload.get("target").and_then(|v| v.as_str()) {
        Some("evacuating") => SessionState::Evacuating,
        // Default Idle: covers the nominated / admin / drop_local
        // flavors and a payload-less row.
        _ => SessionState::Idle,
    };
    let allow_park = payload
        .get("allow_park")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let nominated = payload
        .get("nominated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let quarantine = payload
        .get("quarantine")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let started = ctx.state.services.clock.now_mono();
    match crate::idle_evictor::run_evict_pipeline(ctx, target, allow_park, nominated).await {
        Ok(crate::idle_evictor::EvictOutcome::Evacuated) => {
            // Only completed captures are recorded — the pre-0034 bug
            // shape would reappear as nominations without completions,
            // not as a latency shift.
            ::metrics::histogram!(crate::metrics::EVICTION_PIPELINE_SECONDS).record(
                ctx.state
                    .services
                    .clock
                    .now_mono()
                    .saturating_sub(started)
                    .as_secs_f64(),
            );
            OpOutcome::Done
        }
        // Park: the op completes early at parked (session Evicting,
        // park_rung = 2, VM paused in place). The rung reaper's descent
        // is a FRESH evict op.
        Ok(crate::idle_evictor::EvictOutcome::ParkedPaused) => OpOutcome::Done,
        // A re-entry guard fired (the session moved to a non-evictable
        // state before we ran): complete the op as a no-op.
        Ok(crate::idle_evictor::EvictOutcome::Skipped { reason }) => {
            tracing::info!(
                session_id = %ctx.op.session_id,
                reason,
                "evict op skipped (session no longer evictable)",
            );
            OpOutcome::Done
        }
        Ok(crate::idle_evictor::EvictOutcome::CancelRequested) => OpOutcome::Cancelled,
        // ADR 0090 (2026-07-21 livelock): the guard destroyed a
        // quarantined survivor whose session couldn't be evicted (e.g.
        // harness-failed park at Created) and settled it for its owning
        // re-driver. The destroy cleared the host's quarantine entry, so
        // the 5s advertise → enqueue loop ends with this op.
        Ok(crate::idle_evictor::EvictOutcome::QuarantineReaped) => OpOutcome::Done,
        Ok(crate::idle_evictor::EvictOutcome::Fenced) => {
            OpOutcome::Failed("fenced mid-pipeline (successor re-claimed)".into())
        }
        Err(e) => {
            // Retry budget: the attempt count lives on the op row (bumped
            // at every claim). Exhaustion falls back to HostLost for
            // nominated evictions — NOT Active (would re-nominate
            // forever), NOT Idle (lies: no durable snapshot), NOT Dead
            // (destroys a healthy runtime over a coord-side failure) —
            // exactly the retired scanner's classification (ADR 0034).
            // The one lane the dead-host straggler sweep re-drives is
            // HostLost, so a settled-but-failed nominated eviction MUST
            // land there or it strands with no re-driver.
            //
            // Quarantine flavor (ADR 0090, 2026-08-02 durability-rollback
            // RCA): a smaller fast budget, then the op PARKS on the slow
            // retry lane — no destroy, no HostLost. The old arm destroyed
            // the crippled VM here "so the straggler sweep drives
            // HostLost → Idle", which rewound the session past its acked
            // writes; the survivor's disk is recoverable (the host's
            // rehydrate retry pass), so the honest posture is to wait
            // loudly, preserving the VM. RetryAfter keeps the op QUEUED:
            // the idempotency key stays live, so the host's 5s advertise
            // dedupes (the "key isn't burned" livelock cannot restart)
            // and the not-due op never blocks the session lane.
            let budget = if quarantine {
                QUARANTINE_EVICT_MAX_ATTEMPTS
            } else {
                EVICT_MAX_ATTEMPTS
            };
            if ctx.op.attempts >= budget {
                if quarantine {
                    if ctx.op.attempts == QUARANTINE_EVICT_MAX_ATTEMPTS {
                        ::metrics::counter!(crate::metrics::EVICTION_BUDGET_EXHAUSTED_TOTAL)
                            .increment(1);
                    }
                    return park_quarantined_survivor_stuck(ctx, &e);
                }
                ::metrics::counter!(crate::metrics::EVICTION_BUDGET_EXHAUSTED_TOTAL).increment(1);
                // #810 finding 2 (the Evicting-convergence hole): a
                // settled-but-failed NOMINATED eviction must land in
                // HostLost — the one lane the dead-host straggler sweep
                // re-drives (`list_host_lost_sessions`) — or it strands in
                // `Evicting` with no re-driver (prod aac4efab, 15+ min).
                // The flip fires WITHOUT a destroy (the runtime may be
                // healthy — the straggler sweep's ask-the-host reconcile
                // settles it). A fenced-out failure here (a successor
                // re-claimed the lane) is safe: the successor now owns the
                // session's convergence. (See `exhaustion_settles_host_lost`;
                // the quarantine flavor returned above and never reaches
                // this flip — 2026-08-02 RCA.)
                if exhaustion_settles_host_lost(nominated) {
                    match crate::session_ops::transition_with_fence(
                        ctx.state,
                        ctx.op.session_id,
                        ctx.fence(),
                        SessionState::HostLost,
                        BindingDisposition::Retain,
                    )
                    .await
                    {
                        Ok(prev) => {
                            tracing::warn!(
                                session_id = %ctx.op.session_id,
                                attempts = ctx.op.attempts,
                                nominated,
                                quarantine,
                                error = %e,
                                "evict op budget exhausted; session falls back to HostLost",
                            );
                            let _ = ctx
                                .state
                                .emit_fenced(
                                    ctx.op.session_id,
                                    ctx.fence(),
                                    crate::state::SessionEvent::StatusChanged {
                                        from: prev,
                                        to: SessionState::HostLost,
                                        at: ctx.state.services.clock.now_utc(),
                                    },
                                )
                                .await;
                        }
                        Err(te) => {
                            tracing::warn!(
                                session_id = %ctx.op.session_id,
                                error = %te,
                                "evict budget-exhaustion HostLost fallback transition failed",
                            );
                        }
                    }
                }
                return OpOutcome::Failed(format!(
                    "retry budget exhausted after {} attempts: {e}",
                    ctx.op.attempts
                ));
            }
            OpOutcome::Retry(e.to_string())
        }
    }
}

// ---------------------------------------------------------------------
// deliver — the outbox forward, as an op (ADR 0073's driver reshaped).
// ---------------------------------------------------------------------

/// How long a delivered row waits for its confirming event before it
/// re-becomes due. Generous: covers a slow first token from the agent
/// after a cold resume; the cost of redelivering early is nil (dedup).
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Backoff for rows whose delivery attempt FAILED (host error, harness
/// not yet re-bound). Grows linearly with attempts, capped — an
/// unresumable session shouldn't spin, and there is deliberately no
/// terminal give-up: the row stays until acked or the session dies
/// (FK CASCADE). Un-deliverable ≠ droppable — the user asked.
fn failure_backoff(attempts: i32) -> std::time::Duration {
    let secs = ((attempts.max(0) as u64) + 1) * 2;
    std::time::Duration::from_secs(secs.min(60))
}

/// The deliver verb: drain the session's due outbox rows oldest-first
/// (per-session order is row order; only the head is ever forwarded).
/// The ADR 0073 outbox row stays the durable redelivery state — this op
/// is the transient execution vehicle: `outbox_delivery`'s shim (and the
/// prompt enqueue path) enqueue one Deliver op per due session, and a
/// row deferred by a failed attempt re-becomes due on its own
/// `not_before`, re-enqueued by the shim's next wake.
async fn deliver(ctx: &OpCtx<'_>) -> OpOutcome {
    use engram_core::types::outbox::OutboxRow;
    let state = ctx.state;
    let id = ctx.op.session_id;
    if !ctx.step("drain").await {
        return OpOutcome::Failed("fenced at drain".into());
    }
    // One Deliver op owns the whole drain: this loop re-fetches
    // `outbox_next_due` each pass, so any row a sibling was enqueued for
    // is already ours. The shim's `op_pending_exists` guard is only
    // ADVISORY (a racing wake slips duplicates through), and its "a
    // duplicate finds no due rows and no-ops" assumption inverts when
    // the head row is STUCK: every duplicate then churns the same
    // failing forward. DST finding (ADR 0108 swarm, seed 33043259):
    // duplicates accumulated against a hung host, and N≥3 Deliver ops —
    // the one budget-less kind — mutually re-armed each other's capped
    // backoff with their own deadline burns, an immortal claim cycle.
    // Cancelling queued siblings on claim makes the invariant real:
    // at most one Deliver op survives per session. Never lossy — the
    // outbox rows are the durable state, and the shim's rescan
    // re-enqueues if this op dies fenced mid-drain.
    match state
        .services
        .meta
        .op_cancel_queued(id, OpKind::Deliver)
        .await
    {
        Ok(true) => {
            tracing::debug!(session_id = %id, "deliver op: cancelled queued duplicate deliver ops");
        }
        Ok(false) => {}
        Err(e) => {
            tracing::debug!(session_id = %id, error = %e,
                "deliver op: duplicate-sweep failed; continuing (duplicates only cost churn)");
        }
    }
    loop {
        let row: OutboxRow = match state.services.meta.outbox_next_due(id).await {
            Ok(Some(r)) => r,
            Ok(None) => return OpOutcome::Done,
            Err(e) => return OpOutcome::Retry(format!("outbox next-due fetch: {e}")),
        };
        match retire_legacy_answer_row(state, &row).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(reason) => return OpOutcome::Retry(reason),
        }
        let session = match state.services.meta.get_session(id).await {
            Ok(s) => s,
            // Row gone (FK CASCADE already reaped the outbox too).
            Err(engram_core::MetaError::NotFound) => return OpOutcome::Done,
            Err(e) => return OpOutcome::Retry(format!("get_session: {e}")),
        };

        // -- dispatch on the session's state ---------------------------
        if session.status.is_terminal() {
            // The session can never receive this (Dead/Completed/Failed).
            // Ack the row as consumed-by-termination so it stops waking
            // the driver; the transcript already shows the user's ask,
            // and the session's terminal state is the visible outcome.
            tracing::info!(
                session_id = %id,
                prompt_id = %row.prompt_id,
                status = session.status.as_str(),
                "deliver op: dropping row for terminal session",
            );
            let _ = state.services.meta.outbox_ack(&row.prompt_id).await;
            ::metrics::counter!(crate::metrics::OUTBOX_DROPPED_TERMINAL_TOTAL).increment(1);
            continue;
        }
        let deliverable = match session.status {
            // ADR 0074 rung-2 backstop (`ensure_active`'s Active arm):
            // Active + park_rung == 2 is the pause-landed-but-park-
            // bookkeeping-crashed window — un-pause under OUR fence
            // before forwarding to a frozen harness. The ascent's
            // Active→Active transition conflicts harmlessly; the
            // un-pause + park-clear are what matter.
            SessionState::Active if session.park_rung == 2 => {
                let _ =
                    crate::api::snapshot::ascend_evicting_to_active(state, id, ctx.fence()).await;
                true
            }
            SessionState::Active => true,
            // ADR 0074 rungs 1+2, under the op: we HOLD the session's
            // one-running slot, so no evict pipeline is mid-capture —
            // Evicting here is the nomination window, a parked-paused
            // VM, or (ADR 0101 C) the post-capture settle window; the
            // ascent probes VM liveness and refuses the last (false →
            // the resume-enqueue arm below orders a resume behind the
            // settle). Ascend inline (cancel queued evicts, un-pause a
            // parked VM, flip Active) instead of burning a full
            // evict+resume cycle on a session whose VM is alive.
            SessionState::Evicting => {
                match crate::api::snapshot::ascend_evicting_to_active(state, id, ctx.fence()).await
                {
                    Ok(ascended) => ascended,
                    Err(e) => return OpOutcome::Retry(format!("rung ascent: {e}")),
                }
            }
            // ADR 0101 C (engrams review, #836): `Parked` is the rung-2
            // paused VM as a real state — the exact case the Evicting
            // arm's inline ascent was built for, and the prompt path's
            // ONLY wake-up (send_prompt_core enqueues Deliver directly,
            // never ensure_active). We hold the one-running slot, so no
            // descent is mid-capture; un-pause + flip Active inline —
            // this is the "sending wakes it in about a second" the UI
            // advertises.
            SessionState::Parked => {
                match crate::api::snapshot::ascend_evicting_to_active(state, id, ctx.fence()).await
                {
                    Ok(ascended) => ascended,
                    Err(e) => return OpOutcome::Retry(format!("un-park ascent: {e}")),
                }
            }
            _ => false,
        };
        if !deliverable {
            // Not Active and not inline-ascendable. For the resumable
            // states, enqueue a Resume op and retry: the resume row
            // queues BEHIND this running op, and when this op requeues
            // with backoff its `not_before` gates it out of
            // `op_claim_head` (`WHERE not_before <= now() ORDER BY id`),
            // so the RESUME — higher id but due — is the claimable head
            // and runs first; this op's retry then finds the session
            // Active and forwards. Ordering by log, not poll — and the
            // outbox row is deliberately NOT deferred on this arm, so
            // the retry forwards the moment it runs.
            match session.status {
                // `Unreachable` belongs here, not with the scanner-owned
                // states: NO scanner drives `Unreachable` anywhere — the
                // Resume verb is its only recovery (destroy the dead
                // sandbox → unbind → Idle → resume from checkpoint, ADR
                // 0091), and this arm is what mints that Resume. Before
                // 2026-07-21 it fell to the wildcard below and a prompt to
                // an unreachable session retried forever without ever
                // starting recovery (status-set audit finding 1).
                SessionState::Idle
                | SessionState::Created
                | SessionState::Evicting
                | SessionState::Parked
                | SessionState::Unreachable => {
                    // Review finding #12: probe first — a resume op may
                    // already be queued behind us from a prior deliver
                    // retry. Without the guard, every backed-off deliver
                    // retry appends ANOTHER Resume row (resume-row churn on
                    // a stuck session). `op_pending_exists` is advisory but
                    // collapses the common case to one resume row.
                    match state
                        .services
                        .meta
                        .op_pending_exists(id, OpKind::Resume)
                        .await
                    {
                        Ok(true) => {
                            return OpOutcome::Retry(
                                "resume already queued; delivery retries after it".into(),
                            );
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::debug!(session_id = %id, error = %e,
                                "deliver: resume-pending probe failed; enqueueing anyway");
                        }
                    }
                    // Raw store enqueue, not `session_ops::enqueue`: we
                    // HOLD the session's running slot, so an inline claim
                    // is impossible by construction (the row queues), and
                    // the executor's completion re-drive picks it up the
                    // moment this op requeues. (Also breaks the
                    // enqueue→drive→verb→enqueue async recursion.)
                    if let Err(e) = state
                        .services
                        .meta
                        .op_enqueue_and_claim(
                            id,
                            OpKind::Resume,
                            serde_json::json!({ "flavor": "for_delivery" }),
                            None,
                            &crate::session_ops::pod_id(),
                        )
                        .await
                    {
                        return OpOutcome::Retry(format!("delivery resume enqueue: {e}"));
                    }
                    return OpOutcome::Retry("resume enqueued; delivery retries after it".into());
                }
                // Their scanners own the transition (evac resumer, queue
                // scanner); the op retry keeps knocking.
                other => {
                    let reason = format!(
                        "session is {} — not deliverable yet; its scanner owns recovery",
                        other.as_str()
                    );
                    // ADR 0108 A5: a boot in progress is a KNOWN, short
                    // wait — fixed cadence, so the pre-Active deferrals
                    // never inflate the backoff that paces post-Active
                    // delivery (the 2026-07-31 40 s recovery cadence).
                    // Every other state keeps the attempts-scaled
                    // backoff: those waits are open-ended.
                    return match other {
                        SessionState::Pending | SessionState::Queued => {
                            OpOutcome::RetryAfter(KNOWN_WAIT_RETRY, reason)
                        }
                        _ => OpOutcome::Retry(reason),
                    };
                }
            }
        }

        // -- forward the row (the ADR 0073 `deliver_one` body) ---------
        match forward_outbox_row(ctx, &row).await {
            Ok(()) => {
                if let Err(e) = state
                    .services
                    .meta
                    .outbox_mark_delivered(&row.prompt_id, ACK_TIMEOUT)
                    .await
                {
                    return OpOutcome::Retry(format!("outbox mark_delivered: {e}"));
                }
                ::metrics::counter!(crate::metrics::OUTBOX_DELIVERED_TOTAL).increment(1);
                // Loop to the next due row — one resume covers the batch.
            }
            Err(deferral) => {
                // Defer the ROW and retry the OP: preserve order — never
                // skip ahead of a stuck head. A known-wait deferral (ADR
                // 0108 A4/A5) uses its fixed cadence for both; an
                // unknown failure keeps the attempts-scaled backoff (the
                // durable redelivery state, unchanged from ADR 0073).
                tracing::debug!(
                    session_id = %id,
                    prompt_id = %row.prompt_id,
                    attempts = row.attempts,
                    reason = %deferral.reason,
                    "deliver op: delivery deferred",
                );
                let row_delay = deferral
                    .retry_after
                    .unwrap_or_else(|| failure_backoff(row.attempts));
                let _ = state
                    .services
                    .meta
                    .outbox_defer(&row.prompt_id, row_delay)
                    .await;
                ::metrics::counter!(crate::metrics::OUTBOX_DEFERRED_TOTAL).increment(1);
                return match deferral.retry_after {
                    Some(delay) => OpOutcome::RetryAfter(delay, deferral.reason),
                    None => OpOutcome::Retry(deferral.reason),
                };
            }
        }
    }
}

/// ADR 0108 A4: window after a boot/resume completes in which a
/// `send_prompt` `NotFound` means "the harness has not attached YET",
/// not "the harness is gone". Sized above the observed attach lag
/// (50–200 ms) with a wide margin; past the window, the destructive
/// reattach is the correct remedy again.
const ATTACH_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// ADR 0108 A5: fixed cadence for deferrals whose cause is KNOWN and
/// short-lived (a boot in progress, the attach grace). Never
/// attempts-scaled: the wait is not a failure, and an inflated attempts
/// counter must not slow the retries that follow it.
const KNOWN_WAIT_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// A retryable delivery failure. `retry_after: Some(_)` = the cause is
/// known and short-lived; the caller uses the fixed cadence for both the
/// row and the op. `None` = unknown cause; attempts-scaled backoff.
struct ForwardDeferral {
    reason: String,
    retry_after: Option<std::time::Duration>,
}

impl ForwardDeferral {
    fn backoff(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            retry_after: None,
        }
    }
}

impl std::fmt::Display for ForwardDeferral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Forward one outbox row to the session's live sandbox. `Err` =
/// retryable (the caller defers the row + requeues the op); terminal
/// sessions never reach here (the verb's dispatch drops their rows).
async fn forward_outbox_row(
    ctx: &OpCtx<'_>,
    row: &engram_core::types::outbox::OutboxRow,
) -> Result<(), ForwardDeferral> {
    use engram_core::types::outbox::OutboxKind;
    use engram_core::SandboxError;
    let state = ctx.state;
    if retire_legacy_answer_row(state, row)
        .await
        .map_err(ForwardDeferral::backoff)?
    {
        return Ok(());
    }
    let Some(sandbox_id) = state.resolve_sandbox(row.session_id).await else {
        return Err(ForwardDeferral::backoff(
            "no live sandbox on an Active session",
        ));
    };

    let forward = || async {
        match row.kind {
            OutboxKind::Prompt => {
                let text = row
                    .payload
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                // ADR 0107: the optional mode directive riding this prompt.
                let mode = row
                    .payload
                    .get("mode")
                    .and_then(|m| m.as_str())
                    .map(|m| m.to_string());
                state
                    .services
                    .host
                    .send_prompt(sandbox_id, row.prompt_id.clone(), text, mode)
                    .await
            }
            OutboxKind::Answer => unreachable!("legacy answer rows retire before forwarding"),
            OutboxKind::ToolResult => {
                let tool_call_id = row
                    .payload
                    .get("tool_call_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                let result_json = row
                    .payload
                    .get("result_json")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                state
                    .services
                    .host
                    .tool_result(sandbox_id, tool_call_id, result_json)
                    .await
            }
        }
    };

    match forward().await {
        Ok(()) => {
            // Delivered — the attach question is settled for this
            // binding; drop the grace stamp.
            state.attach_grace.remove(&row.session_id);
            Ok(())
        }
        Err(SandboxError::NotFound) => {
            // ADR 0108 A4: inside the attach grace, `NotFound` means the
            // harness has not dialed YET (boot/resume completed moments
            // ago; the attach follows 50–200 ms later). Firing the
            // reattach here SIGUSR1s the very harness that is mid-attach
            // — the 2026-07-31 boot race. Defer on the fixed cadence;
            // the attach-signal wake (A3) re-runs this op the moment the
            // harness announces itself.
            if let Some(stamp) = state.attach_grace.get(&row.session_id).map(|e| *e.value()) {
                let age = state.services.clock.now_utc().signed_duration_since(stamp);
                if age
                    < chrono::Duration::from_std(ATTACH_GRACE)
                        .expect("ATTACH_GRACE fits chrono::Duration")
                {
                    return Err(ForwardDeferral {
                        reason: "harness not attached yet (attach grace); \
                                 deferring without a reattach nudge"
                            .into(),
                        retry_after: Some(KNOWN_WAIT_RETRY),
                    });
                }
                // Grace expired without an attach: fall through to the
                // destructive remedy and drop the stale stamp.
                state.attach_grace.remove(&row.session_id);
            }
            // The VM is alive but no harness is attached past the grace:
            // the genuine harness-unbound desync (harness process gone,
            // VM alive — the e35ed1fa class). The harness will NOT come
            // back on its own — its connection loop only re-dials when
            // its established link drops or agentd SIGUSR1s it — so
            // `start_agent` is the remedy. (A #594-era "wait for
            // in-guest self-reattach" window here was wrong for a real
            // desync; ADR 0108's grace is different — it keys on a
            // boot/resume that JUST completed, where the attach is
            // provably in flight, and A1's handshake bounds guarantee
            // the wait converges.)
            match crate::api::snapshot::reattach_harness_in_place(
                state,
                row.session_id,
                sandbox_id,
                ctx.fence(),
            )
            .await
            {
                Ok(true) => Err(ForwardDeferral::backoff(
                    "harness reattach issued (start_agent fallback)",
                )),
                Ok(false) => Err(ForwardDeferral::backoff(
                    "session moved off the sandbox mid-delivery",
                )),
                Err(e) => Err(ForwardDeferral::backoff(format!("reattach failed: {e}"))),
            }
        }
        Err(e) => Err(ForwardDeferral::backoff(format!("forward: {e}"))),
    }
}

/// ADR 0089 P5d parse tombstone: real databases can contain an unacked
/// pre-flag-day `answer` row, but the guest wire no longer has an answer
/// command. Terminally acknowledge it before any resume or host lookup so it
/// cannot wedge the outbox head in a permanent retry loop.
async fn retire_legacy_answer_row(
    state: &SharedState,
    row: &engram_core::types::outbox::OutboxRow,
) -> Result<bool, String> {
    if row.kind != engram_core::types::outbox::OutboxKind::Answer {
        return Ok(false);
    }
    tracing::warn!(
        session_id = %row.session_id,
        prompt_id = %row.prompt_id,
        "dropping pre-flag-day answer outbox row after the ADR 0089 wire break"
    );
    state
        .services
        .meta
        .outbox_ack(&row.prompt_id)
        .await
        .map_err(|error| format!("retire legacy answer row: {error}"))?;
    Ok(true)
}

// ---------------------------------------------------------------------
// create_boot — the queue scanner's boot drive, as an op.
// ---------------------------------------------------------------------

/// Boot-retry budget for a placed (`pending`) queued session. The
/// retired requeue-by-poll flow bounced `pending → queued` and leaned on
/// the 30-minute queue timeout; the op row's capped backoff (≤60s)
/// spans a comparable wall-clock window before the terminal Failed.
const CREATE_BOOT_MAX_ATTEMPTS: i32 = 30;

/// The create_boot verb: prepare + boot a session the queue scanner
/// placed (`queued → pending`, host bound). Steps `prepare → launch`.
/// Retry lives on the op row (`not_before`/`attempts` — the retired
/// `requeue_session`/`requeue_stale_pending` poll paths); a coord death
/// mid-boot is the reclaim sweep's re-drive, not a stale-pending scan.
async fn create_boot(ctx: &OpCtx<'_>) -> OpOutcome {
    let state = ctx.state;
    let id = ctx.op.session_id;
    let session = match state.services.meta.get_session(id).await {
        Ok(s) => s,
        // `delete_pending_session` (a NotStarted boot failure on the
        // direct-create path) hard-deletes rows; nothing left to boot.
        Err(engram_core::MetaError::NotFound) => return OpOutcome::Done,
        Err(e) => return OpOutcome::Retry(format!("get_session: {e}")),
    };
    match session.status {
        SessionState::Pending => {}
        // A prior attempt of this op already finished the boot.
        SessionState::Active => return OpOutcome::Done,
        // A destroy op / timeout raced us to terminal — nothing to boot.
        s if s.is_terminal() => {
            tracing::info!(session_id = %id, status = s.as_str(),
                "create_boot op skipped (session no longer pending)");
            return OpOutcome::Done;
        }
        // A prior attempt crashed past `created` (post-restore, pre-
        // Active). Mirrors `BootError::Started`: past `created` is past
        // retry — the sandbox was already unbound by the boot pipeline
        // (or is unreferenced and orphan-reaped); fail the session.
        SessionState::Created => {
            let _ = crate::session_ops::transition_with_fence(
                state,
                id,
                ctx.fence(),
                SessionState::Failed,
                BindingDisposition::Detach,
            )
            .await;
            return OpOutcome::Failed(
                "boot crashed past created; session failed (terminal)".into(),
            );
        }
        other => {
            return OpOutcome::Failed(format!(
                "session is {} — not a placed queued create",
                other.as_str()
            ))
        }
    }
    let Some(host_id) = session.host_id else {
        return OpOutcome::Failed("placed queued session has no bound host".into());
    };

    if !ctx.step("prepare").await {
        return OpOutcome::Failed("fenced at prepare".into());
    }
    let prepared = match crate::api::sessions::prepare_from_row(state, &session).await {
        Ok(p) => p,
        Err(e) => return create_boot_retry_or_fail(ctx, format!("prepare_from_row: {e}")).await,
    };
    if !ctx.step("launch").await {
        return OpOutcome::Failed("fenced at launch".into());
    }
    match crate::session_boot::boot_on_reserved_host(state, prepared.inputs, host_id, ctx.fence())
        .await
    {
        Ok(()) => {
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "placed")
                .increment(1);
            tracing::info!(session_id = %id, %host_id, "create_boot op: placed + booted");
            OpOutcome::Done
        }
        // Row still `pending` (reservation intact) — retry the boot on
        // the op row's backoff.
        Err(crate::session_boot::BootError::NotStarted(e)) => {
            create_boot_retry_or_fail(ctx, format!("boot not-started: {e}")).await
        }
        // Reached `created` then failed — terminal (retry illegal).
        Err(crate::session_boot::BootError::Started(e)) => {
            let _ = crate::session_ops::transition_with_fence(
                state,
                id,
                ctx.fence(),
                SessionState::Failed,
                BindingDisposition::Detach,
            )
            .await;
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "failed")
                .increment(1);
            OpOutcome::Failed(format!("boot failed past created: {e}"))
        }
    }
}

/// Retry a NotStarted-shaped create_boot failure on the op row, or —
/// budget exhausted — fail the session terminally (user-visible event +
/// the Failed flip frees the `pending` reservation).
async fn create_boot_retry_or_fail(ctx: &OpCtx<'_>, reason: String) -> OpOutcome {
    if ctx.op.attempts < CREATE_BOOT_MAX_ATTEMPTS {
        // Keep an actively-retried pending recently-active for the ADR 0079
        // orphan backstop's grace. R3 (#722): now belt-and-suspenders —
        // placement counts a pending's reservation unconditionally and the
        // orphan sweep already skips a session with a live create_boot op
        // (which this retry is). Best-effort: a failed touch just means this
        // attempt didn't refresh.
        let _ = ctx
            .state
            .services
            .meta
            .touch_session_activity(ctx.op.session_id)
            .await;
        return OpOutcome::Retry(reason);
    }
    let state = ctx.state;
    let id = ctx.op.session_id;
    // Own event kind, NOT `queue_timeout`: this session was PLACED and its
    // boot kept failing — a different failure class from "waited out the
    // capacity queue". Sharing the kind made consumers conflate the two
    // (2026-07-11 campaign confusion while triaging queue deaths).
    if let Err(e) = state
        .services
        .meta
        .append_session_event(
            id,
            "boot_retry_exhausted",
            serde_json::json!({
                "reason": "placed but the boot kept failing; retry budget exhausted",
                "detail": reason,
            }),
        )
        .await
    {
        tracing::warn!(session_id = %id, error = %e, "create_boot: boot_retry_exhausted event failed");
    }
    match crate::session_ops::transition_with_fence(
        state,
        id,
        ctx.fence(),
        SessionState::Failed,
        BindingDisposition::Detach,
    )
    .await
    {
        Ok(prev) => {
            let _ = state
                .emit_fenced(
                    id,
                    ctx.fence(),
                    crate::state::SessionEvent::StatusChanged {
                        from: prev,
                        to: SessionState::Failed,
                        at: state.services.clock.now_utc(),
                    },
                )
                .await;
        }
        Err(e) => {
            tracing::warn!(session_id = %id, error = %e,
                "create_boot: budget-exhaustion Failed transition failed");
        }
    }
    ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "failed").increment(1);
    OpOutcome::Failed(format!(
        "boot retry budget exhausted after {} attempts: {reason}",
        ctx.op.attempts
    ))
}

// ---------------------------------------------------------------------
// destroy — DELETE /sessions/:id, as an op.
// ---------------------------------------------------------------------

/// The destroy verb: drive the session to its FSM-legal terminal, then
/// tear down the sandbox. Steps `teardown → finalize`. Enqueued by
/// `delete_session_core` with idempotency key `"destroy"` (one destroy
/// per session, ever); an in-flight resume/evict op ahead in the queue
/// runs first — ordering by log closes the terminate-races-resume #211
/// interleaving, and this op's claim bumps `current_epoch` so any stale
/// predecessor's fenced writes are 0-row from here.
async fn destroy(ctx: &OpCtx<'_>) -> OpOutcome {
    let state = ctx.state;
    let id = ctx.op.session_id;
    if !ctx.step("teardown").await {
        return OpOutcome::Failed("fenced at teardown".into());
    }
    let session = match state.services.meta.get_session(id).await {
        Ok(s) => s,
        Err(engram_core::MetaError::NotFound) => return OpOutcome::Done,
        Err(e) => return OpOutcome::Retry(format!("get_session: {e}")),
    };
    // Capture the live binding (PG authority, read through the
    // per-replica cache) BEFORE the terminal transition, so the teardown
    // below works on any replica (ADR 0047).
    let bound_sandbox = state.resolve_sandbox(id).await;

    // Drive the session to its FSM-legal terminal BEFORE destroying the
    // sandbox: `terminal_target` picks `Completed` for states that ran,
    // `Failed` for the early states (Pending / Created) that never
    // became usable. Terminating first also removes this row from the
    // heartbeat reconcile's "active session whose sandbox is missing"
    // view, so reconcile can't race us into HostLost during the
    // (best-effort, can-take-seconds) destroy RPC below. Already
    // terminal = idempotent: skip the flip, still tidy up.
    if let Some(target) = session.status.terminal_target() {
        match crate::session_ops::transition_with_fence(
            state,
            id,
            ctx.fence(),
            target,
            BindingDisposition::Detach,
        )
        .await
        {
            Ok(prev) => {
                let _ = state
                    .emit_fenced(
                        id,
                        ctx.fence(),
                        crate::state::SessionEvent::StatusChanged {
                            from: prev,
                            to: target,
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
                // ADR 0023: drop the session's credential-broker token so
                // a terminated session can no longer mint git credentials.
                // ADR 0047: the PG row is the authority; the map is a cache.
                state.git_broker_tokens.remove(&id);
                if let Err(e) = state.services.meta.delete_broker_token(id).await {
                    tracing::warn!(session_id = %id, error = %e,
                        "delete_broker_token failed; ON DELETE CASCADE is the backstop");
                }
            }
            Err(engram_core::MetaError::Conflict(msg)) if msg.starts_with("fenced:") => {
                return OpOutcome::Failed("fenced at the terminal transition".into());
            }
            Err(engram_core::MetaError::Conflict(msg)) => {
                // A sibling (reconcile, dead-host) raced the row into a
                // state whose terminal edge differs — idempotent 204
                // shape: tear down whatever's left below.
                tracing::info!(session_id = %id, error = %msg,
                    "destroy op: terminal transition raced; proceeding with best-effort teardown");
            }
            Err(e) => return OpOutcome::Retry(format!("terminal transition: {e}")),
        }
    }

    if !ctx.step("finalize").await {
        return OpOutcome::Failed("fenced at finalize".into());
    }
    // Status is terminal; reconcile won't touch this row anymore. Tear
    // down the sandbox and clear the routing columns.
    if let Some(sandbox_id) = bound_sandbox {
        if let Err(e) = state.services.host.destroy(sandbox_id, ctx.fence()).await {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during session delete; host-agent reconcile will GC",
            );
        }
        // ADR 0006: the host-agent unregisters its local proxy entry as
        // part of `destroy`. No coordinator-side cleanup.
        //
        // Clear the binding so an audit query "what sandboxes does the
        // coordinator think exist" matches reality. Fenced by OUR epoch
        // (issue #211's guarded clear, subsumed): anything that
        // legitimately re-bound the row also re-claimed past us, so a
        // 0-row write here is exactly "leave it for the new owner".
        // Post-#896 the terminal flip above already detached
        // `sandbox_id` atomically (BindingDisposition::Detach); this
        // write is now load-bearing ONLY for clearing the terminal
        // row's dead-weight host affinity (`host_id`), which the
        // disposition deliberately does not touch.
        match state
            .services
            .meta
            .fenced_assign_sandbox(id, ctx.epoch, None, None)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                crate::metrics::note_fenced_write();
                tracing::debug!(session_id = %id, %sandbox_id,
                    "destroy op: fenced binding clear was a no-op (successor re-claimed)");
            }
            Err(e) => {
                tracing::warn!(session_id = %id, error = %e,
                    "destroy op: binding clear failed (best-effort)");
            }
        }
    }
    state.services.host.unbind_session(id).await;
    OpOutcome::Done
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use engram_core::types::session::{Session, SessionMode};
    use engram_core::types::session_op::{EnqueueOutcome, OpState};
    use engram_core::SessionId;

    /// ADR 0078 re-review (finding #2): a transient placement failure —
    /// `PickError::HostUnreachable` (the picked host couldn't be dialed)
    /// or `PickError::Internal` (the hosts read hiccuped) — must surface
    /// through the `SandboxError → ApiError` chain as a RETRYABLE verb
    /// outcome, not terminal `Failed`. Pre-0079 the wire caller retried
    /// around the resume; the verb now owns the only attempt (bounded by
    /// `RESUME_MAX_ATTEMPTS`).
    #[test]
    fn transient_pick_errors_map_to_retry_not_failed() {
        for pick_err in [
            crate::placement::PickError::HostUnreachable(
                engram_core::HostId::new(),
                "dial refused".into(),
            ),
            crate::placement::PickError::Internal("list_active_hosts: pg timeout".into()),
        ] {
            let sandbox_err: engram_core::SandboxError = pick_err.clone().into();
            let outcome = outcome_from_api_error(ApiError::from(sandbox_err));
            assert!(
                matches!(outcome, OpOutcome::Retry(_)),
                "{pick_err:?} must map to OpOutcome::Retry, the transient contract",
            );
        }
    }

    /// #810 finding 2, amended by the 2026-08-02 durability-rollback RCA:
    /// only NOMINATED evictions settle `HostLost` on budget exhaustion.
    /// The quarantine flavor no longer reaches the flip at all — it parks
    /// on the slow retry lane instead of destroying, so there is no
    /// destroyed VM for the straggler sweep to converge.
    #[test]
    fn budget_exhaustion_settles_host_lost_for_nominated_only() {
        assert!(
            exhaustion_settles_host_lost(true),
            "a nominated eviction settles to HostLost (cannot stay Active)"
        );
        assert!(
            !exhaustion_settles_host_lost(false),
            "a plain idle-evict that keeps failing is an operator-visible coord fault, \
             not a settle that fabricates a lost host"
        );
    }

    /// The `ManifestRef` version the reap fixtures publish + assert on.
    const REAP_MANIFEST_VERSION: u64 = 7;

    /// Build an Evicting, quarantined session bound to a fresh sandbox on a
    /// fresh host with a published disk manifest, plus a claimed Evict op —
    /// the shape the exhaustion arm reads. When `route_destroy` is true a
    /// fresh in-proc backend is registered under the session's host so a
    /// `destroy(sandbox_id)` WOULD succeed — which lets the park test assert
    /// "nothing was destroyed" against a fixture where destroying was
    /// possible, not merely unroutable. Returns everything the caller must
    /// keep alive (incl. the tempdirs) so the borrows in its `OpCtx` stay
    /// valid.
    #[allow(clippy::type_complexity)]
    async fn reap_fixture(
        route_destroy: bool,
    ) -> (
        crate::state::SharedState,
        std::sync::Arc<crate::state::tests::MiniMeta>,
        engram_core::types::session_op::SessionOp,
        engram_core::SandboxId,
        engram_core::types::manifest::ManifestRef,
        (tempfile::TempDir, Option<tempfile::TempDir>),
    ) {
        use engram_core::types::manifest::ManifestRef;
        use engram_core::{HostId, SandboxId};
        use std::sync::Arc;

        let id = SessionId::new();
        let sandbox_id = SandboxId::new();
        let host_id = HostId::new();
        let manifest = ManifestRef {
            manifest_id: uuid::Uuid::new_v4(),
            version: REAP_MANIFEST_VERSION,
        };

        let mut session = idle_session(id);
        session.status = SessionState::Evicting;
        session.host_id = Some(host_id);
        session.sandbox_id = Some(sandbox_id);
        session.live_disk_manifest = Some(manifest);
        let (state, mini, local) = crate::state::tests::build_state_for_session(session);

        let backend_dir = if route_destroy {
            // mode=all shape: register a host under the session's host id and
            // pin the ownership row so `resolve_owner`'s fast path returns
            // this backend (ProcessBackend::destroy on an unknown id is a
            // successful no-op).
            let backend_dir = tempfile::TempDir::new().unwrap();
            let backend: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(
                engram_sandbox_process::ProcessBackend::new(backend_dir.path().join("sandboxes")),
            );
            let client: Arc<dyn engram_core::traits::HostClient> =
                Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(backend));
            state.host_registry.register(host_id, client);
            state
                .host_registry
                .record_sandbox_owner(sandbox_id, host_id);
            Some(backend_dir)
        } else {
            // Deliberately wire NO backend for this sandbox: `resolve_owner`
            // finds the session's host id (via `host_for_sandbox`) but no
            // registered/dialable host, so `destroy` returns Err.
            None
        };

        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                id,
                OpKind::Evict,
                serde_json::json!({ "quarantine": true }),
                None,
                "test-pod",
            )
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("lane busy: {other:?}"),
        };
        (state, mini, op, sandbox_id, manifest, (local, backend_dir))
    }

    /// 2026-08-02 durability-rollback RCA regression: a quarantined
    /// survivor's evict exhaustion must PARK the op — a non-terminal
    /// `RetryAfter` on the slow cadence — and must not record a rollback
    /// or move the session. The old arm destroyed the VM, emitted
    /// `durability_rollback`, and settled `HostLost` here; every one of
    /// those effects is now forbidden (the VM holds the only copy of the
    /// acked writes, and the host's rehydrate retry can still recover it).
    #[tokio::test]
    async fn quarantine_exhaustion_parks_the_op_and_records_no_rollback() {
        let (state, mini, op, _sandbox_id, _manifest, _keep) = reap_fixture(true).await;
        let ctx = crate::session_ops::OpCtx {
            state: &state,
            op: &op,
            epoch: op.epoch.unwrap(),
        };
        let err = crate::idle_evictor::EvictError::Meta("capture refused (disk unserved)".into());

        let outcome = super::park_quarantined_survivor_stuck(&ctx, &err);

        match outcome {
            OpOutcome::RetryAfter(delay, msg) => {
                assert_eq!(
                    delay, QUARANTINE_STUCK_RETRY,
                    "the park must use the slow-lane cadence",
                );
                assert!(
                    msg.contains("parked"),
                    "the requeue reason explains the park, got {msg:?}",
                );
            }
            other => panic!("exhaustion must park (RetryAfter), got {other:?}"),
        }
        let events = mini.events.lock();
        assert!(
            !events.iter().any(|e| e.kind == "durability_rollback"),
            "no rollback is recorded — nothing was destroyed and nothing rewinds",
        );
        assert!(
            !events.iter().any(|e| e.kind == "status_changed"),
            "the session stays where it was (no HostLost settle)",
        );
    }

    /// Moved with `failure_backoff` from the retired `outbox_delivery`
    /// driver (ADR 0079 pass 2).
    #[test]
    fn failure_backoff_grows_and_caps() {
        use std::time::Duration;
        assert_eq!(failure_backoff(0), Duration::from_secs(2));
        assert_eq!(failure_backoff(4), Duration::from_secs(10));
        assert_eq!(failure_backoff(1000), Duration::from_secs(60));
    }

    fn idle_session(id: SessionId) -> Session {
        Session {
            id,
            status: SessionState::Idle,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:deliver-verb".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            last_event_at: None,
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    fn outbox_prompt(id: SessionId, prompt_id: &str) -> engram_core::types::outbox::OutboxRow {
        engram_core::types::outbox::OutboxRow {
            prompt_id: prompt_id.to_string(),
            session_id: id,
            kind: engram_core::types::outbox::OutboxKind::Prompt,
            payload: serde_json::json!({ "text": "hello" }),
            created_at: chrono::Utc::now(),
            attempts: 0,
            not_before: chrono::Utc::now(),
            delivered_at: None,
            acked_at: None,
        }
    }

    fn outbox_tool_result(
        id: SessionId,
        tool_call_id: &str,
    ) -> engram_core::types::outbox::OutboxRow {
        engram_core::types::outbox::OutboxRow {
            prompt_id: engram_core::types::outbox::tool_result_outbox_id(id, tool_call_id),
            session_id: id,
            kind: engram_core::types::outbox::OutboxKind::ToolResult,
            payload: serde_json::json!({
                "tool_call_id": tool_call_id,
                "result_json": r#"{"saved":true}"#,
            }),
            created_at: chrono::Utc::now(),
            attempts: 0,
            not_before: chrono::Utc::now(),
            delivered_at: None,
            acked_at: None,
        }
    }

    fn outbox_legacy_answer(
        id: SessionId,
        tool_call_id: &str,
    ) -> engram_core::types::outbox::OutboxRow {
        engram_core::types::outbox::OutboxRow {
            prompt_id: format!("answer:{tool_call_id}"),
            session_id: id,
            kind: engram_core::types::outbox::OutboxKind::Answer,
            payload: serde_json::json!({
                "tool_call_id": tool_call_id,
                "answers": { "Ship?": ["Yes"] },
            }),
            created_at: chrono::Utc::now(),
            attempts: 0,
            not_before: chrono::Utc::now(),
            delivered_at: None,
            acked_at: None,
        }
    }

    #[tokio::test]
    async fn legacy_answer_outbox_row_is_retired_without_host_delivery() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        let row = outbox_legacy_answer(id, "legacy-call");
        state.services.meta.outbox_enqueue(&row).await.unwrap();
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue deliver")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        let ctx = crate::session_ops::OpCtx {
            state: &state,
            epoch: op.epoch.unwrap(),
            op: &op,
        };

        assert!(forward_outbox_row(&ctx, &row).await.is_ok());
        assert!(
            mini.acked_outbox
                .lock()
                .contains(&"answer:legacy-call".to_string()),
            "a pre-flag-day answer row must be terminally retired"
        );
    }

    /// ADR 0101 C (engrams review, #836): a Deliver op on a PARKED
    /// session must wake it — inline ascent (un-pause + flip Active) or,
    /// failing that, an enqueued Resume — never the bare "its scanner
    /// owns recovery" retry: no scanner un-parks a session with a
    /// pending delivery, so the prompt path IS the wake-up (the UI's
    /// "sending wakes it in about a second"). The regression this pins:
    /// a `_ => false` deliverable arm plus a fallback list without
    /// `Parked` left the deliver op spinning until pressure or the 8h
    /// TTL descended the session.
    #[tokio::test]
    async fn deliver_on_parked_wakes_or_enqueues_resume() {
        use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
        let id = SessionId::new();
        let mut session = idle_session(id);
        session.status = SessionState::Parked;
        session.park_rung = 2;
        session.parked_at = Some(chrono::Utc::now());
        let (state, mini, _local) = crate::state::tests::build_state_for_session(session);
        // Bind a real (process) sandbox so the ascent has something to
        // un-pause.
        let spec = SandboxSpec {
            image: "deliver-parked-test".into(),
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
        let sandbox_id = state.services.host.create(spec).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(id, Some(sandbox_id))
            .await
            .unwrap();
        state
            .services
            .meta
            .outbox_enqueue(&outbox_prompt(id, "p-parked"))
            .await
            .unwrap();

        let deliver = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue deliver")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        crate::session_ops::drive_claimed(&state, deliver).await;

        let status = mini.session.lock().status;
        let resume_enqueued = mini.ops.all().iter().any(|o| o.kind == OpKind::Resume);
        assert!(
            status == SessionState::Active || resume_enqueued,
            "a parked session with a pending delivery must be woken (Active) or have a \
             Resume queued; got status={status:?} with no resume op — the deliver verb \
             is spinning against a state nothing else recovers",
        );
    }

    /// ADR 0079 pass 2, the headline ordering property: a Deliver op on
    /// a non-Active (Idle) session enqueues a RESUME op and requeues
    /// itself with backoff — the resume (higher id, due) becomes the
    /// claimable head via `op_claim_head`'s due-gating and runs FIRST.
    /// The latency fix then WAKES the backed-off deliver the instant the
    /// resume reaches terminal (`op_wake_queued_kind`), so the same
    /// drive_claimed pass re-drives the deliver to completion against the
    /// settled state — one pass, zero sleeps, no manual backoff clear.
    #[tokio::test]
    async fn deliver_on_idle_enqueues_resume_and_completes_after_it() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        // A due outbox row for the session.
        state
            .services
            .meta
            .outbox_enqueue(&outbox_prompt(id, "p-1"))
            .await
            .unwrap();

        // Claim + drive the deliver op exactly as the executor would.
        let deliver = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue deliver")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        let deliver_id = deliver.id;
        crate::session_ops::drive_claimed(&state, deliver).await;

        // The deliver op observed Idle: it must have enqueued a Resume op
        // and requeued itself with a not_before backoff. The completion
        // re-drive inside drive_claimed then claims the RESUME (higher id
        // but due — the deliver's backoff gates it out of claim_head) and
        // runs it to ITS terminal state first: with no snapshot seeded the
        // resume fails `gone:` and marks the session Dead — proving the
        // resume ran, strictly before any deliver retry.
        let ops = mini.ops.all();
        let resume = ops
            .iter()
            .find(|o| o.kind == OpKind::Resume)
            .expect("the deliver verb must enqueue a resume op for an Idle session");
        assert!(
            resume.id > deliver_id,
            "the resume op is enqueued behind the deliver's row id"
        );
        assert_eq!(
            resume.state,
            OpState::Failed,
            "the resume must have been claimed + driven (completion re-drive) \
             while the deliver backed off: {:?}",
            resume.error,
        );
        assert!(
            resume.error.as_deref().unwrap_or("").starts_with("gone:"),
            "no snapshot seeded → the resume fails terminal-gone, got {:?}",
            resume.error,
        );
        let session = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(
            session.status,
            SessionState::Dead,
            "the resume verb's own outcome landed before the deliver retry"
        );

        // ADR 0079 latency fix: the for_delivery resume reaching terminal
        // WAKES the backed-off deliver (op_wake_queued_kind resets its
        // not_before), so drive_claimed's own completion re-drive claims
        // it immediately — in this SAME pass, with no 5 s poll wait and no
        // manual backoff clear. The session settled Dead, so the woken
        // deliver drops the row (the Gone→Terminal arm) and completes.
        let deliver_row = mini.ops.get(deliver_id).expect("deliver row");
        assert_eq!(
            deliver_row.state,
            OpState::Done,
            "the woken deliver re-drives and completes in the same pass: {:?}",
            deliver_row.error,
        );
        assert!(
            mini.acked_outbox.lock().contains(&"p-1".to_string()),
            "the terminal session's row is acked (dropped), not redelivered forever"
        );
    }

    #[tokio::test]
    async fn tool_result_delivery_on_idle_enqueues_resume_before_completion() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        state
            .services
            .meta
            .outbox_enqueue(&outbox_tool_result(id, "call_1"))
            .await
            .unwrap();

        let deliver = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue deliver")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        let deliver_id = deliver.id;
        crate::session_ops::drive_claimed(&state, deliver).await;

        let ops = mini.ops.all();
        let resume = ops
            .iter()
            .find(|op| op.kind == OpKind::Resume)
            .expect("ToolResult delivery on Idle must enqueue Resume");
        assert!(resume.id > deliver_id, "Resume is queued behind Deliver");
        assert_eq!(
            resume.state,
            OpState::Failed,
            "the resume runs before the delivery retry"
        );
        assert_eq!(
            mini.ops.get(deliver_id).expect("deliver row").state,
            OpState::Done,
            "delivery completes after the session settles terminal"
        );
        assert!(
            mini.acked_outbox
                .lock()
                .contains(&engram_core::types::outbox::tool_result_outbox_id(
                    id, "call_1",
                )),
            "the completed delivery lane retires the ToolResult row"
        );
    }

    /// The shim's duplicate guard: a pending Deliver op suppresses the
    /// wake-driven enqueue; with none pending, the enqueue lands.
    #[tokio::test]
    async fn shim_enqueue_skips_when_a_deliver_op_is_pending() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        // Seed a rival-held running Deliver op.
        let _running = mini.ops.seed_running(id, OpKind::Deliver);
        crate::outbox_delivery::enqueue_deliver_op(&state, id).await;
        assert_eq!(
            mini.ops.all().len(),
            1,
            "a pending deliver op must suppress the duplicate enqueue"
        );
        // Finish it; the next wake enqueues for real.
        let running = mini.ops.running_for(id).expect("running");
        assert!(mini
            .ops
            .finish(running.id, running.epoch.unwrap(), OpState::Done, None));
        crate::outbox_delivery::enqueue_deliver_op(&state, id).await;
        assert_eq!(
            mini.ops
                .all()
                .iter()
                .filter(|o| o.kind == OpKind::Deliver)
                .count(),
            2,
            "with nothing pending the shim enqueues a fresh deliver op"
        );
    }

    fn evicting_session(id: SessionId) -> Session {
        Session {
            status: SessionState::Evicting,
            ..idle_session(id)
        }
    }

    /// Review finding #9: a resume verb landing on an Evicting session
    /// (nomination window — the resume op holds the running slot, so no
    /// evict pipeline is mid-capture) ASCENDS to Active instead of
    /// Retry-forever behind a nonexistent evict.
    #[tokio::test]
    async fn resume_verb_ascends_evicting_instead_of_retrying() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));
        // The nomination window has a LIVE VM and a cancellable queued
        // evict — the ascent's two-gate check (ADR 0101 C settle-window
        // fix) refuses phantom sandboxes and op-less Evicting rows.
        crate::state::tests::bind_live_sandbox(&state, &mini).await;

        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Resume, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue resume")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("lane busy: {other:?}"),
        };
        // The nomination evict queues BEHIND the running resume op —
        // exactly the shape the verb's inline ascent cancels.
        assert!(matches!(
            state
                .services
                .meta
                .op_enqueue_and_claim(
                    id,
                    OpKind::Evict,
                    serde_json::json!({ "target": "idle", "allow_park": true, "nominated": false }),
                    None,
                    "test-pod",
                )
                .await
                .expect("enqueue nomination evict"),
            EnqueueOutcome::Queued(_)
        ));
        let op_id = op.id;
        crate::session_ops::drive_claimed(&state, op).await;

        assert_eq!(
            mini.session.lock().status,
            SessionState::Active,
            "the resume verb's Evicting arm must ascend to Active, not retry forever",
        );
        assert_eq!(
            mini.ops.get(op_id).unwrap().state,
            OpState::Done,
            "the ascended resume op completes Done",
        );
    }

    /// Review finding #10: the resume crash-shortcut is BOUNDED — after
    /// `SHORTCUT_MAX_ATTEMPTS` failed finishes (a stale binding: Idle+bound
    /// over a dead VM) it falls through to full dispatch (`resume_from_idle`),
    /// which with no snapshot lands the session terminal instead of
    /// re-entering the finish-only shortcut forever.
    #[tokio::test]
    async fn resume_shortcut_falls_through_after_budget_on_stale_binding() {
        let id = SessionId::new();
        // Idle + a (stale) bound sandbox + a recorded "bind" step is the
        // shortcut precondition.
        let mut s = idle_session(id);
        s.sandbox_id = Some(engram_core::SandboxId::new());
        let (state, mini, _local) = crate::state::tests::build_state_for_session(s);

        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Resume, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue resume")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("lane busy: {other:?}"),
        };
        // Simulate a row that already recorded "bind" and retried past the
        // shortcut budget (a live binding would have finished on attempt 1).
        assert!(mini
            .ops
            .force_attempts_and_step(op.id, SHORTCUT_MAX_ATTEMPTS + 1, Some("bind"),));
        let refreshed = mini.ops.get(op.id).unwrap();
        crate::session_ops::drive_claimed(&state, refreshed).await;

        // Fell through to full dispatch: no snapshot → the resume verb
        // drives the session terminal (Dead) rather than looping the
        // finish-only shortcut against the dead binding forever.
        assert_eq!(
            mini.session.lock().status,
            SessionState::Dead,
            "a stale-binding shortcut must fall through to full re-restore, \
             not latch the dead binding",
        );
    }

    /// Review finding #12: a deliver op on an Idle session probes for an
    /// already-pending Resume op before enqueuing, so a stuck session's
    /// repeated deliver retries don't churn out one Resume row per retry.
    #[tokio::test]
    async fn deliver_skips_resume_enqueue_when_one_is_pending() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        state
            .services
            .meta
            .outbox_enqueue(&outbox_prompt(id, "p-1"))
            .await
            .unwrap();
        // Seed a Resume op already queued behind (a prior deliver retry).
        // seed_running would occupy the lane; instead enqueue a queued
        // Resume by first claiming a Deliver, so the Resume queues.
        let deliver = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "test-pod")
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("lane busy: {other:?}"),
        };
        // A Resume already queued behind the running deliver.
        state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Resume, serde_json::json!({}), None, "test-pod")
            .await
            .unwrap();
        let resumes_before = mini
            .ops
            .all()
            .iter()
            .filter(|o| o.kind == OpKind::Resume)
            .count();
        assert_eq!(resumes_before, 1);

        // Drive JUST the deliver verb body (not the completion re-drive, to
        // isolate the enqueue decision).
        let ctx = crate::session_ops::OpCtx {
            state: &state,
            op: &deliver,
            epoch: deliver.epoch.unwrap(),
        };
        let _ = super::deliver(&ctx).await;

        let resumes_after = mini
            .ops
            .all()
            .iter()
            .filter(|o| o.kind == OpKind::Resume)
            .count();
        assert_eq!(
            resumes_after, 1,
            "deliver must not enqueue a second Resume when one is already pending",
        );
    }

    /// Review finding #6: a fenced-out predecessor's lifecycle emit and
    /// park-rung write are no-ops — `emit_fenced` does NOT append after a
    /// successor bumped the epoch, and `fenced_set_session_park_rung` at a
    /// stale epoch can't wipe the successor's fresh rung.
    #[tokio::test]
    async fn fenced_emit_and_park_rung_are_noops_for_a_stale_epoch() {
        use engram_core::traits::SessionFence;
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));

        // Claim + finish op1 (epoch 1), then claim op2 (epoch 2). The mock
        // CAS-bumps current_epoch on each claim, so epoch 1 is now stale.
        let op1 = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "pod")
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("{other:?}"),
        };
        assert!(mini
            .ops
            .finish(op1.id, op1.epoch.unwrap(), OpState::Done, None));
        let op2 = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Deliver, serde_json::json!({}), None, "pod")
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("{other:?}"),
        };
        assert_eq!(op2.epoch, Some(2));

        let events_before = mini.events.lock().len();
        // Stale (epoch 1) emit: must NOT append.
        let stale = state
            .emit_fenced(
                id,
                SessionFence::new(id, 1),
                crate::state::SessionEvent::Evicted {
                    at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
        assert!(
            stale.is_none(),
            "a stale-epoch emit must be fenced (Ok(None))"
        );
        assert_eq!(
            mini.events.lock().len(),
            events_before,
            "a fenced emit must not append to the event log",
        );
        // Current (epoch 2) emit lands.
        let current = state
            .emit_fenced(
                id,
                SessionFence::new(id, 2),
                crate::state::SessionEvent::Evicted {
                    at: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
        assert!(current.is_some(), "a current-epoch emit lands");

        // fenced_set_session_park_rung: stale epoch is a no-op; current
        // applies. Seed rung=2 under epoch 2 first.
        assert!(state
            .services
            .meta
            .fenced_set_session_park_rung(id, 2, 2, Some(chrono::Utc::now()))
            .await
            .unwrap());
        // A stale (epoch 1) compensation trying to clear the rung is a no-op.
        assert!(
            !state
                .services
                .meta
                .fenced_set_session_park_rung(id, 1, 0, None)
                .await
                .unwrap(),
            "a stale-epoch park-rung write must be a no-op",
        );
        assert_eq!(
            mini.session.lock().park_rung,
            2,
            "the fenced-out compensation must NOT wipe the successor's park_rung=2",
        );
    }
}
