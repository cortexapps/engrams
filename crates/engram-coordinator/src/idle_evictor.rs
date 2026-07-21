//! Idle-session eviction: the snapshot+destroy+mark-Idle *pipeline*
//! (the evict VERB's body, ADR 0079) plus (ADR 0034) the *eviction
//! scanner* that keeps `Evicting` rows enqueued.
//!
//! ADR 0013 + ADR 0011 follow-up #2 retired the polling driver that
//! lived here. In a stateless coord, no single pod's local
//! `HarnessHub` is authoritative for "is this sandbox idle?" — the
//! host owns that view. The host scans its local hub on a tick and
//! POSTs candidates to `/api/hosts/:id/idle-eviction-candidates`.
//!
//! ADR 0034 split nomination from execution; ADR 0079 made execution a
//! durable `session_ops` row. The nomination side (idle detector /
//! handler) flips `Active → Evicting` and ENQUEUES the evict op; the
//! op executor (`session_ops.rs`) drives [`run_evict_pipeline`] to a
//! terminal row state, detached from any request lifetime, resuming at
//! the recorded step after a coordinator death. [`spawn_eviction_scanner`]
//! is now only the backstop that (re-)enqueues evict ops for `Evicting`
//! rows whose op went missing (e.g. cancelled by an ascent that then
//! never resumed) — it drives no pipeline itself.
//!
//! Auto-resume on next request is wired separately (`api/sessions.rs`
//! exec/exec_stream/SSE handlers): if status is `Idle`, enqueue-and-
//! observe the resume op before routing.

use engram_core::traits::SandboxBackend;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::SessionState;
use engram_core::{SandboxId, SessionId};

use crate::session_ops::OpCtx;
use crate::state::{SessionEvent, SharedState};

/// Terminal shape of one [`run_evict_pipeline`] drive.
///
/// Issue #214 heritage: the pipeline has legitimate "no-op" arms that
/// return without evacuating (the K5 status guard). Distinguishing them
/// lets the verb complete the op honestly and lets synchronous
/// observers (admin `EvictIdle`) report what actually happened.
#[derive(Debug, Clone)]
pub enum EvictOutcome {
    /// The pipeline ran: the session was suspended to the target state
    /// (for the D5 fast path, the host-owned finalize row also landed
    /// or the bounded watch expired — the op held through it).
    Evacuated,
    /// ADR 0074 rung 2: the host had memory headroom, so instead of a
    /// full snapshot+destroy the VM was PAUSED in place (park_rung=2,
    /// session stays Evicting, harness alive in RAM). A returning
    /// user's prompt un-pauses it in <100ms (the rung-2 ascent); a
    /// later pressure/dwell sweep descends it via a FRESH evict op.
    ParkedPaused,
    /// A re-entry guard fired; the session was NOT evacuated by this
    /// call (a concurrent op moved it first). The op completes Done.
    Skipped { reason: &'static str },
    /// A fenced step/write: a successor re-claimed the op — this
    /// executor stopped silently mid-pipeline.
    Fenced,
    /// The op's cooperative cancel flag was observed BEFORE any capture
    /// side effect (the rung-1 window): the session is exactly as the
    /// nomination left it (`Evicting`, VM untouched); the canceller's
    /// `Active` transition owns the ascent.
    CancelRequested,
    /// ADR 0090 (2026-07-21 livelock incident): a QUARANTINE-flavored op
    /// found the session in a state the pipeline can't evict from
    /// (e.g. `Created` after an ADR 0077 harness-failed park) while it
    /// still binds the quarantined sandbox. A plain `Skipped` here
    /// livelocks: the crippled VM keeps advertising every 5s heartbeat,
    /// each advertise enqueues a fresh op (the idempotency key only
    /// dedups queued/running rows), and each op skips in ~10ms — prod
    /// session 8174b7aa looped for 2.5 days / ~43k ops. Instead the
    /// guard CONVERGES: destroy the crippled VM (capture is impossible
    /// by definition of quarantine — its disk is unserved; the destroy
    /// clears the host's quarantine entry, ending the advertise loop)
    /// and settle the session per its state (see the guard's match).
    QuarantineReaped,
}

/// ADR 0074 rung-2 park bookkeeping, run after a successful `pause`:
/// stamp `park_rung=2`/`parked_at`, and make the STATUS say what the VM
/// is doing. The natural path enters already `Evicting` (nominated); the
/// admin `EvictIdle` path enters `Active`, and returning `ParkedPaused`
/// without a transition used to leave an ACTIVE session over a frozen VM
/// — `park_rung == 2` uniformly means `Evicting` now. (The `ensure_active`
/// Active-arm un-park stays as the backstop for the crash window between
/// the pause and this transition.)
async fn park_paused_bookkeeping(
    ctx: &OpCtx<'_>,
    session_id: SessionId,
    entry_status: SessionState,
) -> Result<(), engram_core::MetaError> {
    let state = ctx.state;
    let now = state.services.clock.now_utc();
    // ADR 0079 (review finding #6): FENCED — a fenced-out predecessor must
    // not stamp park_rung=2 over a successor's fresh state. Ok(false) =
    // fenced; surface it as the `fenced:` Conflict the caller already maps
    // to `EvictOutcome::Fenced`.
    if !state
        .services
        .meta
        .fenced_set_session_park_rung(session_id, ctx.epoch, 2, Some(now))
        .await?
    {
        crate::metrics::note_fenced_write();
        return Err(engram_core::MetaError::Conflict(
            "fenced: successor re-claimed during park bookkeeping".into(),
        ));
    }
    if entry_status == SessionState::Active {
        crate::session_ops::transition_with_fence(
            state,
            session_id,
            ctx.fence(),
            SessionState::Evicting,
        )
        .await?;
        let _ = state
            .emit_fenced(
                session_id,
                ctx.fence(),
                crate::state::SessionEvent::StatusChanged {
                    from: SessionState::Active,
                    to: SessionState::Evicting,
                    at: now,
                },
            )
            .await;
    }
    // ADR 0101 C: parked is a real state, not a rung stamp over
    // Evicting — the paused-in-place VM reads `parked` (cheap to wake,
    // intentionally retained); `evicting` is reserved for an actual
    // descent in flight. The rung stamp above is kept as host-ledger /
    // ascent metadata; lifecycle identity is the status.
    crate::session_ops::transition_with_fence(state, session_id, ctx.fence(), SessionState::Parked)
        .await?;
    let _ = state
        .emit_fenced(
            session_id,
            ctx.fence(),
            crate::state::SessionEvent::StatusChanged {
                from: SessionState::Evicting,
                to: SessionState::Parked,
                at: now,
            },
        )
        .await;
    Ok(())
}

/// ADR 0074 rung 2: does the session's host have memory headroom to
/// keep a VM PAUSED (rung 2) rather than fully evicting it? Reads the
/// heartbeat-persisted `hosts.utilization`. Fails CLOSED (no headroom →
/// full eviction) on a telemetry gap: a paused VM frees no RAM, so
/// parking under UNKNOWN pressure is the dangerous direction (it could
/// leave the host overcommitted), the mirror of the idle detector's
/// fail-open-toward-eviction.
///
/// Headroom threshold reuses the detector's mem floor (default 15% free)
/// plus a margin, so a host that is not "under pressure" for eviction
/// purposes has room to hold a paused VM.
async fn host_has_memory_headroom(state: &SharedState, session_id: SessionId) -> bool {
    // Resolve the host through PG, NOT the in-memory `host_registry`: this
    // runs on whichever coord replica took the RPC / scanner tick, and a
    // replica that never cached the sandbox→host bind would fail closed
    // here — silently degrading every park into a full eviction on that
    // pod (a coin flip in a 2-replica deployment).
    let host_id = match state.services.meta.get_session(session_id).await {
        Ok(s) => s.host_id,
        Err(_) => return false,
    };
    let Some(host_id) = host_id else {
        return false;
    };
    let hosts = match state.services.meta.list_active_hosts().await {
        Ok(h) => h,
        Err(_) => return false,
    };
    let Some(host) = hosts.into_iter().find(|h| h.id == host_id) else {
        return false;
    };
    let u = &host.utilization;
    if u.mem_total_mib == 0 {
        return false; // unmeasured → fail closed (no parking)
    }
    // Prefer the RAM ledger's `allocatable_mib` (issue #540): it already
    // nets out other parked-but-resident VMs' PSS, so parking decisions
    // don't double-count a host that is already holding parked sandboxes
    // (which is exactly how a naive `total − used` would over-park a host
    // into pressure). Fall back to raw physical free on pre-ledger /
    // non-Linux hosts where `allocatable_mib == 0`.
    let free_mib = if u.allocatable_mib > 0 {
        u.allocatable_mib
    } else {
        u.mem_total_mib.saturating_sub(u.mem_used_mib)
    };
    let free_pct = free_mib.saturating_mul(100) / u.mem_total_mib;
    // A comfortable margin above the eviction floor: only park when the
    // host has real slack, so parked residency can't tip it into
    // pressure.
    free_pct as u8 >= park_headroom_floor_pct()
}

/// Free-RAM percent above which a host will PARK (rung 2) instead of
/// evict. Above the eviction mem floor by design (env-tunable).
fn park_headroom_floor_pct() -> u8 {
    std::env::var("ENGRAM_PARK_HEADROOM_FLOOR_PCT")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(30)
}

/// ADR 0079: the evict VERB's pipeline — the same pause → flush →
/// Wall-clock bound on ONE capture attempt of a quarantined survivor
/// (ADR 0090 — `payload.quarantine`), covering BOTH capture flavors —
/// the D5 `snapshot_begin` split (the normal prod-FC path) and the
/// composed `snapshot()` fallback. Generous next to a healthy capture
/// (upload legs run seconds-to-a-couple-minutes) but a hard stop for the
/// pathological crawl class; see the timeout sites in
/// [`run_evict_pipeline`]. Env-tunable for tests.
fn quarantine_capture_timeout() -> std::time::Duration {
    let secs = std::env::var("ENGRAM_QUARANTINE_CAPTURE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300);
    std::time::Duration::from_secs(secs)
}

/// Bound on the best-effort pre-capture guest RPCs (`stop_browser` /
/// `stop_ide`) for a quarantined survivor: they route into a guest whose
/// rootfs is unserved and can hang in-guest indefinitely; their results
/// are discarded anyway.
const QUARANTINE_GUEST_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ADR 0101 C: how long the eviction scanner trusts a COMPLETED evict
/// op's capture before re-minting (see `scanner_advance_one`). The
/// settle normally lands within one heartbeat (~5s) of the host-owned
/// finalize finishing — with Phases A+B that whole tail is seconds —
/// so 60s covers slow uploads without materially delaying the
/// wedged-finalize retry path.
const EVICT_SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// ADR 0090 (2026-07-21 livelock incident): converge a QUARANTINE evict
/// that found its session in a state the entry guard can't evict from
/// while the session still binds the quarantined sandbox. Capture is
/// impossible by definition of quarantine (the survivor's disk is
/// unserved), so the only useful moves are destroying the crippled VM —
/// which clears the host's quarantine entry and ends the 5s
/// advertise → enqueue → skip loop — and settling the row in a lane its
/// owning machinery re-drives. Wildcard-free over `SessionState` on
/// purpose: a future state must pick its arm here, not inherit a silent
/// skip that re-opens the loop.
async fn quarantine_reap_unevictable(
    ctx: &OpCtx<'_>,
    session: &engram_core::types::Session,
    sandbox_id: SandboxId,
) -> Result<EvictOutcome, EvictError> {
    let state = ctx.state;
    let session_id = session.id;
    match session.status {
        // Unreachable from the caller (these are the entry-legal states);
        // kept so the match stays total.
        SessionState::Active | SessionState::Evicting => Ok(EvictOutcome::Skipped {
            reason: "session no longer evictable (a concurrent op moved it first)",
        }),
        // In-flight placement/relocation lanes: the queue scanner / evac
        // resumer own these rows, and destroying the VM under a mid-evac
        // capture would race their machinery. Their own settle paths
        // converge (evac falls back to Idle, queued rows re-place); if
        // the binding survives that, the next advertise re-enters here.
        SessionState::Pending | SessionState::Queued | SessionState::Evacuating => {
            tracing::warn!(
                session_id = %session_id,
                %sandbox_id,
                state = session.status.as_str(),
                "quarantined survivor bound to an in-flight placement/relocation \
                 lane; leaving convergence to its owning machinery",
            );
            Ok(EvictOutcome::Skipped {
                reason: "quarantined survivor owned by in-flight placement/relocation",
            })
        }
        // The livelock class. `Created` is the ADR 0077 harness-failed
        // park (prod 8174b7aa: a resume's start_agent failed against the
        // crippled VM, parked at Created, and no evict could ever run);
        // `Unreachable` is its dead-guest cousin. Nothing user-visible
        // ran in the VM (the harness never (re)started), so destroy it
        // and settle HostLost — the one lane the dead-host straggler
        // sweep re-drives to Idle/recoverable; the next prompt resumes
        // from the last checkpoint (ADR 0090's designed blast radius).
        //
        // ADR 0101 C: `Parked` joins this arm — a quarantined survivor's
        // disk is unserved, so the paused VM can neither wake usefully
        // nor descend (capture needs the data plane). Unlike
        // Created/Unreachable the session DID run user work; destroying
        // it loses the un-captured tail, exactly a host-death loss —
        // HostLost is the honest settle (recovery from the last
        // published checkpoint, surfaced as CheckpointLag on resume).
        SessionState::Created | SessionState::Unreachable | SessionState::Parked => {
            if let Err(e) = state.services.host.destroy(sandbox_id, ctx.fence()).await {
                // Leave the binding + state alone: the op's retry budget
                // (and, on exhaustion, the verb's destroy-then-HostLost
                // arm) own the re-drive at op-backoff cadence, not the
                // 5s heartbeat's.
                return Err(EvictError::Meta(format!(
                    "quarantine reap: destroy of unevictable survivor failed: {e}"
                )));
            }
            // A destroyed PARKED survivor is real, user-visible data loss
            // (the paused VM held user work newer than the last durable
            // checkpoint) — the same fact the budget-exhaustion arm
            // records (PR #829), and it must be exactly as loud here:
            // counter + durable `durability_rollback` row, emitted the
            // moment the destroy lands, independent of the status flip
            // (2026-07-21 61a03b7e: this arm destroyed a healthy parked
            // VM — 93 events rewound — with a single WARN as the only
            // trace). Created/Unreachable stay quiet: the harness never
            // (re)started, so no user-visible work is being rolled back.
            if session.status == SessionState::Parked {
                ::metrics::counter!(crate::metrics::DURABILITY_ROLLBACK_TOTAL).increment(1);
                tracing::error!(
                    session_id = %session_id,
                    %sandbox_id,
                    rewind_disk_manifest = ?session.live_disk_manifest,
                    "quarantined PARKED survivor destroyed — un-checkpointed user \
                     work in the paused VM is LOST; the next resume rewinds to the \
                     last durable checkpoint (CheckpointLag)",
                );
                let _ = state
                    .emit_fenced(
                        session_id,
                        ctx.fence(),
                        SessionEvent::DurabilityRollback {
                            sandbox_id,
                            rewind_disk_manifest: session.live_disk_manifest,
                            reason: "quarantined parked survivor was unevictable \
                                     (disk unserved); VM destroyed — resume rewinds \
                                     to the last durable checkpoint"
                                .to_string(),
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
            }
            match crate::session_ops::transition_with_fence(
                state,
                session_id,
                ctx.fence(),
                SessionState::HostLost,
            )
            .await
            {
                Ok(prev) => {
                    tracing::warn!(
                        session_id = %session_id,
                        %sandbox_id,
                        from = prev.as_str(),
                        "quarantined survivor was unevictable (harness-failed park / \
                         dead guest); destroyed the crippled VM and settled HostLost \
                         for the straggler sweep to recover",
                    );
                    let _ = state
                        .emit_fenced(
                            session_id,
                            ctx.fence(),
                            SessionEvent::StatusChanged {
                                from: prev,
                                to: SessionState::HostLost,
                                at: state.services.clock.now_utc(),
                            },
                        )
                        .await;
                }
                Err(e) => {
                    // The destroy landed (the advertise loop is dead);
                    // a failed flip means a successor moved the session
                    // first — it owns convergence from here.
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "quarantine reap: destroyed the survivor but the HostLost \
                         flip did not land (a successor owns the session)",
                    );
                }
            }
            Ok(EvictOutcome::QuarantineReaped)
        }
        // Durable-or-settled rows: whatever is recoverable is already
        // recorded (Idle implies a durable capture; HostLost is mid-
        // recovery; terminals are terminal). The crippled VM is garbage —
        // reap it and drop the stale binding so nothing (e.g. the resume
        // crash-shortcut) latches a destroyed sandbox. No status change.
        SessionState::Idle
        | SessionState::HostLost
        | SessionState::Failed
        | SessionState::Completed
        | SessionState::Dead => {
            if let Err(e) = state.services.host.destroy(sandbox_id, ctx.fence()).await {
                return Err(EvictError::Meta(format!(
                    "quarantine reap: destroy of settled-session survivor failed: {e}"
                )));
            }
            match state
                .services
                .meta
                .fenced_assign_sandbox(session_id, ctx.epoch, None, session.host_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    crate::metrics::note_fenced_write();
                    tracing::warn!(
                        session_id = %session_id,
                        "quarantine reap: unbind fenced (successor re-claimed); \
                         the destroy already ended the advertise loop",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "quarantine reap: unbind failed after destroy; the stale \
                         binding resolves on the session's next lifecycle op",
                    );
                }
            }
            tracing::warn!(
                session_id = %session_id,
                %sandbox_id,
                state = session.status.as_str(),
                "reaped a quarantined survivor bound to a settled session",
            );
            Ok(EvictOutcome::QuarantineReaped)
        }
    }
}

/// memory-snapshot → destroy sequence the legacy `evict_session_to_state`
/// ran, now driven under an op claim (the mutual exclusion; the
/// `session_ops_one_running` index replaces the session lease) with
/// durable step markers (`park_or_capture → mark_idle`).
///
/// `target_state = Idle` is the user-paused (manual /resume) shape;
/// `Evacuating` is the operator-drain shape where the `evac_resumer`
/// scanner drives `Evacuating → Created → Active` on a peer host — the
/// non-target-state code paths are IDENTICAL, so both flows share the
/// recoverability invariants (snapshot durable before destroy, PG flips
/// before the best-effort destroy so reconcile can't race the orphan
/// path).
///
/// `nominated = true` (idle-detector / scanner / rung-descent ops)
/// tightens the entry guard to `status == Evicting`: a rung-1/2 ascent
/// that flipped the session back to `Active` is an authoritative cancel,
/// and a nominated op that claims after it must no-op — this is what
/// makes the cancel-then-transition in `try_cancel_nominated_eviction`
/// race-free without a lease. Admin/drain ops (`nominated = false`)
/// accept `Active | Evicting` (the ADR 0044 K5 legal-entry set).
pub(crate) async fn run_evict_pipeline(
    ctx: &OpCtx<'_>,
    target_state: SessionState,
    allow_park: bool,
    nominated: bool,
) -> Result<EvictOutcome, EvictError> {
    let state = ctx.state;
    let session_id = ctx.op.session_id;
    // The pipeline only knows about Idle and Evacuating as legal
    // targets. Both share the "Active → captured-snapshot → suspended"
    // semantic; any other target would skip half the steps and break
    // the recovery invariants. Reject early with a clean error.
    if !matches!(target_state, SessionState::Idle | SessionState::Evacuating) {
        return Err(EvictError::Meta(format!(
            "run_evict_pipeline: target {target_state:?} not supported \
             (only Idle and Evacuating)",
        )));
    }

    // ADR 0044 K5: race-free re-entry guard on the authoritative PG
    // state. The op claim is the exclusion, so once we hold it any
    // concurrent lifecycle op has *fully* completed — read the state and
    // skip if the session is no longer evictable. Anything outside the
    // legal-entry set (already `Idle`, mid-`Evacuating`, terminal, or —
    // for nominated ops — back at `Active` via an ascent) means a peer
    // already handled it.
    let session = match state.services.meta.get_session(session_id).await {
        Ok(s) => s,
        Err(e) => {
            return Err(EvictError::Meta(format!(
                "evict: read session state under the op claim: {e}"
            )));
        }
    };
    let entry_status = session.status;
    let entry_legal = if nominated {
        entry_status == SessionState::Evicting
    } else {
        matches!(entry_status, SessionState::Active | SessionState::Evicting)
    };
    let quarantine = ctx
        .op
        .payload
        .get("quarantine")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !entry_legal {
        // ADR 0090 (2026-07-21 livelock incident): a quarantine op must
        // never settle as a plain skip while the session still binds the
        // quarantined sandbox — the host re-advertises the survivor every
        // 5s heartbeat, each advertise re-enqueues (the idempotency key
        // only dedups queued/running rows), and each op would skip again
        // in ~10ms, forever (prod session 8174b7aa: 2.5 days, ~43k ops).
        // Converge instead: see [`quarantine_reap_unevictable`].
        if quarantine {
            if let Some(sandbox_id) = session.sandbox_id {
                return quarantine_reap_unevictable(ctx, &session, sandbox_id).await;
            }
        }
        tracing::info!(
            session_id = %session_id,
            state = entry_status.as_str(),
            nominated,
            "evict skipped: session no longer evictable (a concurrent op moved it first)",
        );
        return Ok(EvictOutcome::Skipped {
            reason: "session no longer evictable (a concurrent op moved it first)",
        });
    }
    // The binding read here (PG authority, ADR 0047) replaces the old
    // "sandbox_id param still bound?" guard — the row IS the binding.
    let Some(sandbox_id) = session.sandbox_id else {
        // Structurally inconsistent for a nominated row (`Evicting`
        // implies a live VM): fall back to HostLost exactly as the
        // retired scanner arm did — HostLost is not Active (no detector
        // re-nominates) and not Idle (no durable snapshot is implied).
        if nominated {
            match crate::session_ops::transition_with_fence(
                state,
                session_id,
                ctx.fence(),
                SessionState::HostLost,
            )
            .await
            {
                Ok(prev) => {
                    tracing::warn!(
                        %session_id,
                        "evict op: Evicting row has no bound sandbox; falling back to HostLost",
                    );
                    let _ = state
                        .emit_fenced(
                            session_id,
                            ctx.fence(),
                            SessionEvent::StatusChanged {
                                from: prev,
                                to: SessionState::HostLost,
                                at: state.services.clock.now_utc(),
                            },
                        )
                        .await;
                }
                Err(e) => {
                    tracing::warn!(%session_id, error = %e,
                        "evict op: no-sandbox HostLost fallback transition failed");
                }
            }
            return Ok(EvictOutcome::Skipped {
                reason: "Evicting row had no bound sandbox (fell back to HostLost)",
            });
        }
        // Admin/drain flavor: nothing to evict — another actor already
        // unbound it. A no-op, not an error (retrying can't grow a
        // sandbox back; the admin entry points gate on a binding before
        // enqueueing anyway).
        return Ok(EvictOutcome::Skipped {
            reason: "session has no bound sandbox (already evicted or relocated)",
        });
    };

    // Cooperative cancel — honored ONLY here, before any capture side
    // effect (the rung-1 window): the session is exactly as we found it
    // (`Evicting` with the VM untouched, or `Active` for an admin op),
    // and the canceller's `Active` transition owns the ascent. Once the
    // capture starts we run the pipeline to completion — a post-capture
    // cancel would strand host-side finalize state, and the standard
    // resume-behind-evict ordering already covers the returning user.
    if ctx.cancel_requested().await {
        tracing::info!(%session_id, %sandbox_id, "evict op cancelled before capture (rung-1 ascent)");
        return Ok(EvictOutcome::CancelRequested);
    }

    // ADR 0016 A.1.1: entry log. Pair this with the "completed"/
    // "skipping" logs and the per-step warn arms so the full pipeline is
    // auditable end-to-end.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        target = target_state.as_str(),
        allow_park,
        "eviction pipeline started (op claim held)",
    );

    // Durable step boundary: park-or-capture begins. A crash after this
    // point resumes here; every leg below is idempotent-from-step (the
    // host's snapshot_begin re-observes its pending job, the composed
    // capture re-captures, the PG flips are legality/fence-guarded).
    if !ctx.step("park_or_capture").await {
        return Ok(EvictOutcome::Fenced);
    }

    // ADR 0074 rung descent: a parked-paused VM (park_rung == 2) being
    // force-evicted (`allow_park = false`, the reaper's descent op) must
    // be UN-PAUSED first so the capture pipeline (browser reap, flush,
    // guest agent RPCs) talks to a live guest. Idempotent-from-step: a
    // re-driven op re-issues the resume; un-pausing a running VM is a
    // host-side no-op.
    if session.park_rung == 2 && !allow_park {
        if let Err(e) = state.services.host.resume(sandbox_id, ctx.fence()).await {
            return Err(EvictError::Sandbox(e));
        }
    }

    // Step 1: take a snapshot. ADR 0007 Phase 6: backend owns its
    // staging dir; coord no longer pre-allocates one. Durability
    // flows through the chunked manifests on `SnapshotMetadata`.
    //
    // ADR 0014 issue #1/#2: the host tracks this as an in-flight
    // snapshot. Every exit path after this point MUST end with either
    // `commit_snapshot` (full pipeline succeeded) or `abort_snapshot`
    // (anything else). Without this, a coord-side flake leaves the
    // 4 GiB local snapshot dir + per-snapshot blob keys orphaned —
    // host-side `idle_evictor` re-POSTs the candidate on the next tick,
    // a fresh SnapshotId is minted, and we leak ~4 GiB per retry. That
    // was the failure on `engrams-fc-xngk` (25 dirs × 4 GiB in 13 min).
    // ADR 0045 D5: for plain idle eviction (target Idle), split the
    // capture from the upload — the session is user-visibly Idle as soon
    // as the pause-side capture lands, and the chunk+upload runs in a
    // background finalize task under the (touched) lease, with the PG
    // snapshot row written only at finalize ("row-only-at-finalize": no
    // half-durable rows; a finalize failure means resume falls back to
    // the prior checkpoint — the same blast radius as an active host
    // death). Drain/teleport (target Evacuating) keeps the composed path:
    // the evac scanner resumes from the row, so it must be durable first.
    // Hosts that don't support the split (pre-D5, non-FC) surface
    // InvalidSpec and fall through to the composed path too.
    //
    // Quarantine flavor (ADR 0090): bound EVERY leg that talks to the
    // crippled sandbox. A quarantined survivor's capture can legitimately
    // succeed (its memory + dirty-chunk upload don't need the dead
    // guest-visible NBD device), but it can also crawl for hours
    // (2026-07-13 incident: a chain-poisoned re-chunk at ~0.5 MB/s held
    // the session lane ~50 min with the user's resume queued behind it —
    // and the within-step heartbeat keeps an in-flight attempt
    // unreclaimable by design). The timeouts turn a hang or crawl into a
    // failed attempt; the verb's small quarantine budget then converges
    // to destroy + rewind. Covers BOTH capture flavors — `snapshot_begin`
    // (the normal prod-FC D5 path; adversarial-review finding: the first
    // cut bounded only the composed fallback) and composed `snapshot()`.
    // (`quarantine` itself is read above the entry guard now — the
    // 2026-07-21 livelock fix consumes it there too.)
    if target_state == SessionState::Idle {
        // ADR 0074 rung 2 (parked-paused): if the host has memory
        // headroom, PAUSE the VM in place instead of snapshot+destroy.
        // Frees CPU (not RAM), keeps the harness alive in RAM, and lets
        // a returning user un-pause in <100ms rather than pay a full
        // 12.2s-p50 rebuild. Under real memory pressure this branch is
        // skipped and the full eviction below runs (rung 4). Only the
        // idle-evict path parks; drain/evac (Evacuating) always captures.
        // `allow_park == false` is the reaper's DESCENT path (already
        // parked, now forcing the full capture) — it must never re-park.
        if allow_park && host_has_memory_headroom(state, session_id).await {
            match state.services.host.pause(sandbox_id, ctx.fence()).await {
                Ok(()) => match park_paused_bookkeeping(ctx, session_id, entry_status).await {
                    Ok(()) => {
                        ::metrics::counter!(crate::metrics::EVICTION_PARKED_PAUSED_TOTAL)
                            .increment(1);
                        tracing::info!(session_id = %session_id, %sandbox_id,
                                "rung-2 park: VM paused in place (host has memory headroom)");
                        return Ok(EvictOutcome::ParkedPaused);
                    }
                    // Fenced mid-bookkeeping: stop silently — never
                    // compensate (un-pause) from a fenced executor.
                    Err(engram_core::MetaError::Conflict(msg)) if msg.starts_with("fenced:") => {
                        return Ok(EvictOutcome::Fenced);
                    }
                    Err(e) => {
                        tracing::warn!(session_id = %session_id, error = %e,
                                "rung-2 park: bookkeeping failed; un-pausing and falling through to full eviction");
                        let _ = state.services.host.resume(sandbox_id, ctx.fence()).await;
                        // Fenced compensation (review finding #6): a no-op
                        // if a successor re-claimed — never wipe its rung.
                        if matches!(
                            state
                                .services
                                .meta
                                .fenced_set_session_park_rung(session_id, ctx.epoch, 0, None)
                                .await,
                            Ok(false)
                        ) {
                            crate::metrics::note_fenced_write();
                        }
                    }
                },
                Err(engram_core::SandboxError::InvalidSpec(_)) => {
                    // Backend can't pause (VZ/Process) — fall through to
                    // the full eviction below.
                }
                // ADR 0091: TRANSIENT refusal — the host declined the park
                // because a capture (usually the periodic checkpoint, 600s
                // cadence) holds the sandbox's capture lock. Pre-fix this
                // fell through to a full 15-19 min eviction on EVERY
                // overlap (4/4 observed in the 2026-07-11 campaign, since
                // any session >10 min old overlaps a checkpoint window).
                // Skip like the checkpoint driver's own skip posture: the
                // detector re-nominates next tick and the park succeeds
                // once the capture drains.
                Err(engram_core::SandboxError::Unavailable(msg)) => {
                    tracing::info!(session_id = %session_id, %sandbox_id, %msg,
                        "rung-2 park: host busy (capture in flight); retrying next nomination");
                    return Ok(EvictOutcome::Skipped {
                        reason: "park deferred: capture in flight on the host",
                    });
                }
                Err(e) => {
                    tracing::warn!(session_id = %session_id, error = %e, error_kind = ?e,
                        "rung-2 park: pause failed; falling through to full eviction");
                }
            }
        }
        // ADR 0065: reap the ephemeral in-guest browser stack before the eviction
        // snapshot so a live Chrome is never frozen into it (re-lazy-started on
        // the next EnsureBrowser after resume). Best-effort; never blocks eviction
        // (a quarantined guest can hang these — bounded above discard).
        if quarantine {
            let _ = tokio::time::timeout(
                QUARANTINE_GUEST_RPC_TIMEOUT,
                state.services.host.stop_browser(sandbox_id),
            )
            .await;
        } else {
            let _ = state.services.host.stop_browser(sandbox_id).await;
        }
        // ADR 0085: same for the IDE — a live code-server's listeners would
        // resurrect wedged after restore (issue #567's lesson); the next
        // EnsureIde re-lazy-starts it. Best-effort; never blocks eviction.
        if quarantine {
            let _ = tokio::time::timeout(
                QUARANTINE_GUEST_RPC_TIMEOUT,
                state.services.host.stop_ide(sandbox_id),
            )
            .await;
        } else {
            let _ = state.services.host.stop_ide(sandbox_id).await;
        }
        let begin_fut = state.services.host.snapshot_begin(sandbox_id, ctx.fence());
        let begin_res = if quarantine {
            match tokio::time::timeout(quarantine_capture_timeout(), begin_fut).await {
                Ok(res) => res,
                Err(_elapsed) => {
                    abort_inflight_snapshot(
                        ctx,
                        session_id,
                        sandbox_id,
                        "quarantine snapshot_begin timeout",
                    )
                    .await;
                    return Err(EvictError::Meta(format!(
                        "quarantined-survivor capture (snapshot_begin) timed out after {}s",
                        quarantine_capture_timeout().as_secs()
                    )));
                }
            }
        } else {
            begin_fut.await
        };
        match begin_res {
            Ok(snapshot_id) => {
                return finish_eviction_d5(ctx, session_id, sandbox_id, snapshot_id, entry_status)
                    .await;
            }
            Err(engram_core::SandboxError::InvalidSpec(reason)) => {
                tracing::debug!(
                    session_id = %session_id,
                    %reason,
                    "snapshot_begin unsupported; using the composed eviction pipeline",
                );
            }
            Err(e) => return Err(EvictError::Sandbox(e)),
        }
    }

    let snapshot_fut = state.services.host.snapshot(sandbox_id, ctx.fence());
    let metadata = if quarantine {
        match tokio::time::timeout(quarantine_capture_timeout(), snapshot_fut).await {
            Ok(res) => res.map_err(EvictError::Sandbox)?,
            Err(_elapsed) => {
                abort_inflight_snapshot(ctx, session_id, sandbox_id, "quarantine capture timeout")
                    .await;
                return Err(EvictError::Meta(format!(
                    "quarantined-survivor capture timed out after {}s",
                    quarantine_capture_timeout().as_secs()
                )));
            }
        }
    } else {
        snapshot_fut.await.map_err(EvictError::Sandbox)?
    };

    let host_id = state.host_registry.host_of(sandbox_id);
    let now = state.services.clock.now_utc();
    // ADR 0028 A.log / issue #529: the event-log leg of the coherence
    // triple. Resolve the cursor from the host's EXACT pause instant
    // (`metadata.paused_at`, stamped in `SnapshotFinisher::finish`) when
    // present — this closes the skew a coordinator wall-clock `now`
    // sampled AFTER the (possibly multi-second) capture/upload
    // introduces, which is what made a clean evict→resume roll back the
    // coordinator's own lifecycle events (rewind is now also kind-scoped
    // to guest-derived events; the two fixes are complementary — this
    // one shrinks the skew window, that one makes the skew harmless).
    // `unwrap_or(now)` is the pre-#529 behavior, preserved for backends
    // that don't set it (a mixed wire-version roll; VZ/Process's raw,
    // unwrapped-by-PooledBackend snapshot() calls).
    let events_cursor = state
        .services
        .meta
        .latest_event_idx_at_or_before(session_id, metadata.paused_at.unwrap_or(now))
        .await
        .unwrap_or_default();
    // ADR 0068: stamp the capturing host's FC snapshot-version so a
    // later restore can be paired against it at placement. Best-effort:
    // a lookup failure degrades to NULL, same posture as events_cursor
    // above — never fails the eviction over it.
    let fc_snapshot_version = match host_id {
        Some(h) => state
            .services
            .meta
            .fc_snapshot_version_for_host(h)
            .await
            .unwrap_or_default(),
        None => None,
    };
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: Some(session_id),
        host_id,
        image_version: metadata.image_version.clone(),
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
        // ADR 0007: chunked manifests are the durability primitive.
        disk_manifest: metadata.disk_manifest,
        memory_manifest: metadata.memory_manifest,
        // ADR 0009 Phase 2: HEAD-verify the chunked manifests so
        // reconcile flips this session to Idle (not Dead) on a
        // future sandbox-loss event.
        recoverable: crate::api::snapshot::verify_snapshot_recoverable(
            state.services.blob.as_ref(),
            metadata.disk_manifest.as_ref(),
            metadata.memory_manifest.as_ref(),
        )
        .await,
        // ADR 0035: pin the generations this snapshot references.
        aux_bundles: metadata.aux_bundles.clone(),
        events_cursor,
        fc_snapshot_version,
    };
    // ADR 0079 (re-review findings #3/#4): record the row UNDER OUR FENCE.
    // The whole `park_or_capture` step brackets pause → snapshot → record →
    // commit with no intermediate `ctx.step()` fence check, and the capture
    // leg can run minutes; a coord↔PG partition that outlasts `RECLAIM_STALE`
    // lets a successor re-claim this session (bumping `current_epoch`) while
    // we're mid-capture. A plain INSERT here would land a phantom
    // `recoverable` row a resume could pick (the 89f7984d durability-lie
    // class), and we'd then issue `commit_snapshot` under the stale epoch.
    // `fenced_record_snapshot` writes NOTHING when the epoch has moved:
    match state
        .services
        .meta
        .fenced_record_snapshot(record.clone(), ctx.epoch)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            crate::metrics::note_fenced_write();
            // Fenced out mid-capture: do NOT record, do NOT commit. Abort
            // the host's in-flight snapshot (best-effort — rejected if the
            // successor already advanced the host high-water, which is fine)
            // and stop; the successor op owns the capture now.
            abort_inflight_snapshot(ctx, session_id, sandbox_id, "fenced_record_snapshot").await;
            return Ok(EvictOutcome::Fenced);
        }
        Err(e) => {
            abort_inflight_snapshot(ctx, session_id, sandbox_id, "record_snapshot").await;
            return Err(EvictError::Meta(e.to_string()));
        }
    }

    // #792 (R4, ADR 0098 Phase 3): the recoverable-before-Idle guard. The
    // capture produced manifests and we recorded the row, but the
    // `verify_snapshot_recoverable` above does a live BlobStorage HEAD of
    // each manifest — a TRANSIENT head blip stamps `recoverable = false` on
    // a capture whose artifacts are actually durable. Landing the session
    // `Idle` on that lie strands it un-resumable: the falsehood surfaces
    // only at `/resume`, which then fails (→ `Dead` via the #782 honest
    // predicate) after the user already tried to come back. Gate the
    // terminal transition on the SAME honest predicate the dead-host
    // stage-2 ladder uses (#782 `dead_host::recovery_target`): a capture
    // with no recoverable snapshot AND no live disk manifest is not a safe
    // basis for `Idle`.
    //
    // Scope is exactly the transient-blip class — `manifests_present &&
    // !recoverable` means "the capture DID produce manifests but a HEAD
    // failed." A manifest-LESS capture keeps its prior behavior on purpose:
    // the dev backends (Process/VZ) and the pre-#791 sim fidelity gap
    // produce no chunked manifests, #791 already closed the manifest-less
    // case at the sim-fidelity layer, and gating it here would wedge those
    // backends into an eternal retry. Only the idle path is gated (drain /
    // `Evacuating` is out of #792's scope and routes differently).
    //
    // Recovery routes through the existing ladder — no new state or RPC:
    // abort the still-in-flight snapshot and return a RETRYABLE error (do
    // NOT commit, NOT flip `Idle`, NOT destroy the still-live VM). The op
    // machinery redrives within the evict budget (`session_verbs::evict`);
    // a transient blip clears on the fresh capture+verify → `Idle`
    // recoverable. A PERSISTENTLY unrecoverable capture exhausts the budget
    // and falls back (nominated) to `HostLost`, where the dead-host
    // straggler sweep applies `recovery_target` and settles the session
    // `Dead` WITH the distinct unrecoverable-snapshot signal
    // (`note_unrecoverable_if_dead`) — never a lying `Idle`. The
    // `recoverable = false` row recorded just above is precisely what lets
    // that sweep emit the signal.
    let manifests_present = metadata.disk_manifest.is_some() || metadata.memory_manifest.is_some();
    if target_state == SessionState::Idle
        && manifests_present
        && !record.recoverable
        && session.live_disk_manifest.is_none()
    {
        ::metrics::counter!(crate::metrics::EVICTION_UNRECOVERABLE_CAPTURE_GUARD_TOTAL)
            .increment(1);
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            snapshot_id = %record.id,
            "idle eviction: capture recorded recoverable=false (manifests present but a \
             BlobStorage HEAD failed) with no live disk manifest — refusing to land Idle on \
             an un-resumable capture; aborting and retrying verification via the op budget",
        );
        abort_inflight_snapshot(ctx, session_id, sandbox_id, "recoverable-guard").await;
        return Err(EvictError::Meta(format!(
            "capture {} recorded recoverable=false (transient BlobStorage HEAD failure) with \
             no live disk manifest; refusing to land Idle on an un-resumable capture — \
             retrying verification within the evict op budget",
            record.id,
        )));
    }

    // ADR 0034 durability: commit the host's in-flight snapshot NOW —
    // while the sandbox is still bound and its owner resolvable, and
    // BEFORE `unbind`/`destroy` below or the racing periodic-checkpoint
    // driver can `abort_prior_inflight_snapshot` the artifacts out from
    // under the `recoverable = true` row we just wrote. The old
    // placement (after destroy()) could NEVER succeed: destroy() plus
    // the `sandbox_id = NULL` transition below make `resolve_owner`
    // return NotFound, so commit no-op'd, the host kept the snapshot
    // "in-flight", and the next checkpoint tick deleted
    // state.bin/sidecar from BlobStorage while PG still advertised the
    // snapshot as recoverable — bricking the resume. Prod incident
    // 89f7984d (2026-06-04). Best-effort: a genuine host RPC failure
    // here is rare and backstopped by resume-time blob verification
    // (see `api::snapshot::resume_from_idle`).
    if let Err(e) = state
        .services
        .host
        .commit_snapshot(sandbox_id, ctx.fence())
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: commit_snapshot failed; host in-flight tracking not \
             cleared (resume-time verification will catch a deleted snapshot)",
        );
    }

    // ADR 0016 §A.1.6: PG-side transitions happen BEFORE
    // `destroy()`. The reconciler (ADR 0009) runs on every host
    // heartbeat and treats `session.sandbox_id IS NOT NULL` +
    // `host.running_sandboxes does not contain sandbox_id` as an
    // orphan to be recovered via HostLost→Idle. If destroy() ran
    // before transition_session, a heartbeat landing in the window
    // between those two steps would race ahead and flip the
    // session to Idle via the recovery path, leaving this
    // pipeline's later `transition_session(Idle→Idle)` to fail
    // (state-machine rejects same-state), `abort_snapshot` to
    // fire spuriously, and the matched `pipeline completed` log
    // never to appear. Validated on session 1edf09a3 (2026-05-24).
    //
    // Reordering to PG-first means: by the time the host's next
    // heartbeat reports `running_sandboxes` missing this sandbox,
    // `session.status` is already Idle and reconcile's
    // active-only guard no-ops. The destroy() call's host-side
    // bookkeeping (proxy unregister, jail teardown) still runs;
    // a failed destroy() is best-effort the same as before.

    // Durable step boundary: the capture is committed; what remains is
    // the PG flip + destroy. A crash-resume from here re-runs the guard
    // (already-Idle → Skipped) or re-captures (idempotent — a second
    // snapshot row is a fresh checkpoint).
    if !ctx.step("mark_idle").await {
        return Ok(EvictOutcome::Fenced);
    }

    // Step 3 (PG, Idle-before-destroy): clear sandbox_id on the
    // session row — ADR 0047, this is the authoritative unbind (no
    // in-memory registry to drop). Fenced; `host_id` is re-written
    // unchanged (the fenced write sets both columns) to preserve the
    // resume path's origin-affinity hint.
    match state
        .services
        .meta
        .fenced_assign_sandbox(session_id, ctx.epoch, None, session.host_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            crate::metrics::note_fenced_write();
            return Ok(EvictOutcome::Fenced);
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "idle eviction: fenced_assign_sandbox(None) failed",
            );
        }
    }
    // Step 3c (PG, Idle-before-destroy): flip to the target. Once this
    // commits, the reconciler will no-op on every subsequent
    // heartbeat for this session because the reconcile pass keys
    // on Active status only.
    let prev = match crate::session_ops::transition_with_fence(
        state,
        session_id,
        ctx.fence(),
        target_state,
    )
    .await
    {
        Ok(prev) => prev,
        // Fenced (a successor re-claimed): stop silently — never
        // compensate from a fenced executor; the successor owns the
        // in-flight snapshot's fate.
        Err(engram_core::MetaError::Conflict(msg)) if msg.starts_with("fenced:") => {
            return Ok(EvictOutcome::Fenced);
        }
        Err(e) => {
            abort_inflight_snapshot(ctx, session_id, sandbox_id, "transition_session").await;
            return Err(EvictError::Meta(e.to_string()));
        }
    };
    // The parking ladder leaves no trace in the terminal state — a
    // descent (or a plain eviction of a never-parked session, where this
    // is a no-op) clears the rung with the Idle flip.
    if session.park_rung != 0 {
        // Fenced (review finding #6): clears the rung under OUR epoch only.
        if matches!(
            state
                .services
                .meta
                .fenced_set_session_park_rung(session_id, ctx.epoch, 0, None)
                .await,
            Ok(false)
        ) {
            crate::metrics::note_fenced_write();
        }
    }

    // Step 4 (host destroy, post-PG): now the session is Idle,
    // destroy the sandbox. Best-effort — failures don't bubble
    // because the PG state is already correct; the host's
    // orphan_reap background task cleans up a stuck sandbox.
    if let Err(e) = state.services.host.destroy(sandbox_id, ctx.fence()).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: destroy failed after Idle transition; orphan_reap will clean up",
        );
    }
    // ADR 0006: host-agent unregisters its local proxy entry as
    // part of `destroy`. No coordinator-side cleanup needed.

    // Review finding #6: fenced emits. Ok(None) = a successor re-claimed
    // between our committed transition and here — stop emitting silently
    // (never compensate/abort from a fenced predecessor); the transition
    // already committed under our epoch, so the eviction is done.
    if let Err(e) = state
        .emit_fenced(
            session_id,
            ctx.fence(),
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await
    {
        abort_inflight_snapshot(ctx, session_id, sandbox_id, "emit SnapshotTaken").await;
        return Err(EvictError::Emit(e.to_string()));
    }
    if let Err(e) = state
        .emit_fenced(session_id, ctx.fence(), SessionEvent::Evicted { at: now })
        .await
    {
        abort_inflight_snapshot(ctx, session_id, sandbox_id, "emit Evicted").await;
        return Err(EvictError::Emit(e.to_string()));
    }
    if let Err(e) = state
        .emit_fenced(
            session_id,
            ctx.fence(),
            SessionEvent::StatusChanged {
                from: prev,
                to: target_state,
                at: now,
            },
        )
        .await
    {
        abort_inflight_snapshot(ctx, session_id, sandbox_id, "emit StatusChanged").await;
        return Err(EvictError::Emit(e.to_string()));
    }

    // ADR 0016 A.1.1: success log. Pairs with the entry log so a
    // pipeline that flushes (host log) without committing (no PG
    // row) shows up as an unmatched start/end pair in a grep.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        snapshot_id = %metadata.id,
        "idle eviction pipeline completed",
    );

    Ok(EvictOutcome::Evacuated)
}

/// ADR 0045 D5 (rewritten for issue #529, ADR 0079, then ADR 0101 C):
/// the fast-path tail of an idle eviction. The capture has landed
/// (`snapshot_begin` returned) and the finalize is a HOST-OWNED job:
/// the host durably persisted its inputs before `snapshot_begin`
/// returned, and it lands the snapshot row itself via the heartbeat
/// reconcile (`api/host_http.rs::heartbeat`) — surviving this
/// coordinator dying, restarting, or never seeing the upload complete.
///
/// ADR 0101 C: this function no longer flips the session Idle — `idle`
/// means "closure verified durable + recoverable PG row", and the
/// reconcile that records that row performs the fused settle
/// (`settle_evicted_session_idle`). The evict op still FINISHES here
/// (ADR 0079 review finding #11: never hold the session's one-running
/// op lane to watch an upload).
async fn finish_eviction_d5(
    ctx: &OpCtx<'_>,
    session_id: SessionId,
    sandbox_id: SandboxId,
    snapshot_id: engram_core::types::SnapshotId,
    entry_status: SessionState,
) -> Result<EvictOutcome, EvictError> {
    if !ctx.step("mark_idle").await {
        return Ok(EvictOutcome::Fenced);
    }
    // An admin-path evict enters at `Active` (a nominated one already
    // transitioned at nomination): make the descent visible BEFORE the
    // op finishes, so the reconcile's `Evicting → Idle` settle has a CAS
    // to land on. Fenced; a non-fenced failure means the session raced
    // (delete / host death) — the settle then no-ops harmlessly and the
    // raced state's own machinery converges.
    if entry_status == SessionState::Active {
        match crate::session_ops::transition_with_fence(
            ctx.state,
            session_id,
            ctx.fence(),
            SessionState::Evicting,
        )
        .await
        {
            Ok(prev) => {
                let _ = ctx
                    .state
                    .emit_fenced(
                        session_id,
                        ctx.fence(),
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::Evicting,
                            at: ctx.state.services.clock.now_utc(),
                        },
                    )
                    .await;
            }
            Err(engram_core::MetaError::Conflict(msg)) if msg.starts_with("fenced:") => {
                return Ok(EvictOutcome::Fenced);
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id, error = %e,
                    "D5 eviction: Active→Evicting descent transition failed (raced); \
                     the raced state's machinery owns convergence");
            }
        }
    }
    // ADR 0101 C: the Idle-before-durable flip is RETIRED. The capture
    // landed (`snapshot_begin` returned — the host's finalize inputs
    // are crash-durable on its node), but `idle` now means "the
    // snapshot closure is verified durable": the heartbeat reconcile
    // (`api/host_http.rs::heartbeat`) performs the fused
    // detach + `Evicting → Idle` settle (`settle_evicted_session_idle`)
    // the moment it records the recoverable eviction-final row — with
    // Phases A+B that is seconds behind us, not the old ~25-95s. The op
    // still FINISHES here (ADR 0079 finding #11: never hold the
    // one-running op lane to watch an upload); the session simply stays
    // `evicting` — honestly — for the short publication window, and a
    // resume in that window queues behind the settle instead of
    // silently rolling back to a stale checkpoint. If the host dies
    // before the row lands, the scanner's bounded attempts fall the
    // session to HostLost (recovery from the last published checkpoint,
    // surfaced as ADR 0091 CheckpointLag) — the same floor as today,
    // minus the silence.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        snapshot_id = %snapshot_id,
        "idle eviction: capture landed; session stays Evicting until the heartbeat \
         reconcile records the recoverable snapshot row and settles it Idle (ADR 0101 C)",
    );

    // ADR 0079 (review finding #11): the evict op FINISHES here — the
    // 900s inline finalize row-watch that once lived here HELD the
    // session's one-running op lane for the whole upload, blocking any
    // queued resume/deliver behind it. The settle is the reconcile's
    // job now; this op has nothing left to hold the lane for.
    Ok(EvictOutcome::Evacuated)
}

/// ADR 0014 issue #1/#2: best-effort `abort_snapshot` after a
/// downstream pipeline failure in [`run_evict_pipeline`]. Logs but
/// never propagates — the caller's pipeline error is what surfaces.
/// Hosts implement abort idempotently so spurious double-calls are
/// safe.
async fn abort_inflight_snapshot(
    ctx: &OpCtx<'_>,
    session_id: SessionId,
    sandbox_id: SandboxId,
    scope: &'static str,
) {
    if let Err(e) = ctx
        .state
        .services
        .host
        .abort_snapshot(sandbox_id, ctx.fence())
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            scope,
            error = %e,
            "idle eviction: abort_snapshot failed after pipeline failure \
             (orphan local dir + blobs may persist until next retry)",
        );
    }
}

#[derive(Debug)]
pub enum EvictError {
    Io(String),
    Sandbox(engram_core::SandboxError),
    Meta(String),
    Emit(String),
}

impl std::fmt::Display for EvictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "idle evict io: {m}"),
            Self::Sandbox(e) => write!(f, "idle evict sandbox: {e}"),
            Self::Meta(m) => write!(f, "idle evict meta: {m}"),
            Self::Emit(m) => write!(f, "idle evict event emit: {m}"),
        }
    }
}

impl std::error::Error for EvictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sandbox(e) => Some(e),
            _ => None,
        }
    }
}

// The TTL env helpers + the soft/hard-TTL detection driver live on
// the host-agent (`engram_host_agent::idle_evictor`). The coord owns
// the `run_evict_pipeline` verb body above and (ADR 0034 / ADR 0079)
// the eviction scanner below that keeps `Evicting` rows ENQUEUED.

// ─── ADR 0034 / ADR 0079: eviction scanner ───────────────────────
//
// The nomination side (idle detector / handler) flips Active →
// Evicting and enqueues the evict op; the op EXECUTOR drives the
// pipeline. This scanner is the backstop only: an `Evicting` row with
// no queued/running evict op (its op was cancelled by an ascent that
// then never resumed, or was enqueued by a pre-op-log coordinator) is
// re-enqueued. It drives no pipeline itself — the retry budget lives
// on the op row (`session_verbs::EVICT_MAX_ATTEMPTS`).

#[derive(Clone, Debug)]
pub struct EvictionScannerConfig {
    /// How often to sweep for Evicting sessions. 10s matches
    /// `EvacResumerConfig::poll_interval` — same operational cadence
    /// for all session-lifecycle scanners.
    pub poll_interval: std::time::Duration,
}

impl Default for EvictionScannerConfig {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_secs(10),
        }
    }
}

/// Spawn the eviction scanner as a background task. Caller holds the
/// JoinHandle for the process lifetime; dropping aborts the loop.
/// Mirrors [`crate::evac_resumer::spawn`].
///
/// The first sweep after coord startup is part of the deploy-recovery
/// story: a row left `Evicting` with no op (see the module doc) gets a
/// fresh evict op here; a row whose op is still queued/running is left
/// to the op executor (its reclaim sweep owns a dead pod's op).
pub fn spawn_eviction_scanner(
    cfg: EvictionScannerConfig,
    state: SharedState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        loop {
            tick.tick().await;
            if let Err(e) = scanner_run_once(&state).await {
                tracing::warn!(error = %e, "eviction scanner tick failed; will retry");
            }
        }
    })
}

/// Single scanner tick. `pub(crate)` so tests can drive the scanner
/// deterministically without `tokio::spawn`-ing the loop.
pub async fn scanner_run_once(
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ADR 0101 C: parked sessions are their own state now — the reaper
    // owns their pressure / hard-TTL descent candidacy. They are no
    // longer `Evicting` rows the scanner "finds" every tick forever
    // (the 8h-in-evicting weirdness, and half of the ADR 0077×0090
    // livelock surface).
    let parked = state.services.meta.list_parked_sessions().await?;
    for session in parked {
        if let Err(e) = park_reaper_advance_one(state, session).await {
            tracing::warn!(error = %e, "park reaper per-session advance failed");
        }
    }

    let candidates = state.services.meta.list_evicting_sessions().await?;
    // Queue-depth gauge even when 0 — a flatline at 0 is the healthy
    // signal; a climbing value means evictions arrive faster than
    // pipelines complete. (Parked sessions no longer count: parked is
    // a resting state, not a backlog.)
    ::metrics::gauge!(crate::metrics::EVICTION_SCANNER_QUEUE).set(candidates.len() as f64);
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "eviction scanner found Evicting sessions"
    );
    for (session, _attempts) in candidates {
        if let Err(e) = scanner_advance_one(state, session).await {
            // Keep going — one wedged session shouldn't stall the
            // sweep.
            tracing::warn!(error = %e, "eviction scanner per-session advance failed");
        }
    }
    Ok(())
}

// ADR 0019 / telemetry restoration (#526): scanner-driven work has no
// request span to inherit — give it an explicit root so the enqueue's
// spans correlate by `session_id` instead of exporting as disconnected
// roots with no shared attribute.
#[tracing::instrument(name = "idle_evictor.advance_one", skip_all, fields(session_id = %session.id))]
async fn scanner_advance_one(
    state: &SharedState,
    session: engram_core::types::Session,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;

    // An Evicting row with no bound sandbox is structurally
    // inconsistent (nothing to evict): fall back to HostLost NOW rather
    // than enqueue a guaranteed no-op. The dead_host HostLost straggler
    // sweep owns recovery from there, and HostLost is not Active so neither
    // detector re-nominates — the loop is broken by construction.
    // (The evict verb has the same fallback for a binding that vanishes
    // between this enqueue and its claim.)
    if session.sandbox_id.is_none() {
        match state
            .services
            .meta
            .transition_session(session_id, SessionState::HostLost)
            .await
        {
            Ok(prev) => {
                ::metrics::counter!(crate::metrics::EVICTION_BUDGET_EXHAUSTED_TOTAL).increment(1);
                tracing::warn!(
                    %session_id,
                    "eviction scanner: Evicting row has no bound sandbox; falling back to HostLost",
                );
                let _ = state
                    .emit(
                        session_id,
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::HostLost,
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                // Already moved (delete raced us, host died) — fine;
                // anything else logs and retries next tick.
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "eviction scanner fallback transition Evicting→HostLost failed",
                );
            }
        }
        return Ok(());
    }

    // A running evict op already owns this row — nothing to do. (Queued
    // dedup rides the idempotency key below.)
    if let Some(op) = state.services.meta.op_running_for(session_id).await? {
        if op.kind == engram_core::types::session_op::OpKind::Evict {
            return Ok(());
        }
    }

    // (Re-)enqueue the evict op. The idempotency key is stable per
    // NOMINATION (`last_active_at` is stamped by the Active → Evicting
    // transition and doesn't move while the row stays Evicting), so a
    // 10s-tick scanner enqueues at most ONE op per nomination — a
    // duplicate sweep (or a sibling replica's) hits `Duplicate`. A
    // cancelled/failed op burns the key; the next NOMINATION mints a new
    // one (and a cancelled-op session that stays Evicting because its
    // canceller never resumed it gets re-enqueued via a fresh key only
    // after re-nomination — the ascent's Active transition is what
    // normally ends that state).
    let key = format!("evict:{}", session.last_active_at.timestamp_millis());

    // ADR 0101 C (engrams review, #836 rounds 2+3): a COMPLETED evict op
    // means the capture already landed — the session is honestly
    // `Evicting` for the publication window and the heartbeat reconcile's
    // settle owns the tail. Terminal rows leave the dedup index by
    // design, so without this check every tick would mint a fresh op that
    // re-runs stop_browser/stop_ide + the (idempotent) snapshot_begin
    // against the finalizing sandbox — the terminal-op churn shape the
    // #837 op-mint oracle exists to condemn, transiently. KEY-AGNOSTIC
    // (round 3): the three post-capture paths mint under three different
    // keys — this scanner's `evict:<last_active>`, the park reaper's
    // `evict-descend:<parked_at>`, and the admin/evict_local path's none
    // at all — so a key-scoped read protected only one of three.
    //
    // The suppression is TIME-BOUNDED, not absolute: past the grace the
    // scanner re-mints — but as a CAPTURE RETRY (`allow_park: false`).
    // The stale-Done case means the settle never landed (a wedged or
    // quarantined finalize); re-parking there would defeat the eviction
    // the prior op already committed to (and against a lock-free
    // quarantined survivor, `host.pause` can succeed — stranding a
    // "parked" session whose durability upload failed). The capture
    // retry preserves the wedged-upload → bounded-attempts → HostLost
    // floor; a DEAD host is the dead-host detector's job either way.
    let mut allow_park = true;
    if let Some(prior) = state
        .services
        .meta
        .op_latest_for_kind(session_id, engram_core::types::session_op::OpKind::Evict)
        .await?
    {
        match prior.state {
            // A pending evict already exists (e.g. the reaper's descent
            // op, minted under its own key, not yet claimed) — a second
            // mint is pure waste; the op lane serializes anyway.
            engram_core::types::session_op::OpState::Queued => {
                return Ok(());
            }
            engram_core::types::session_op::OpState::Done => {
                let within_grace = prior.finished_at.is_some_and(|t| {
                    state
                        .services
                        .clock
                        .now_utc()
                        .signed_duration_since(t)
                        .to_std()
                        .is_ok_and(|elapsed| elapsed < EVICT_SETTLE_GRACE)
                });
                if within_grace {
                    tracing::debug!(
                        %session_id,
                        "eviction scanner: capture landed (Done evict op); the reconcile \
                         settle owns the tail — not re-minting",
                    );
                    return Ok(());
                }
                allow_park = false;
            }
            // Failed / Cancelled / Running: the pre-existing behavior
            // (Running is already handled above; a burned key re-mints).
            _ => {}
        }
    }
    let outcome = crate::session_ops::enqueue(
        state,
        session_id,
        engram_core::types::session_op::OpKind::Evict,
        serde_json::json!({ "target": "idle", "allow_park": allow_park, "nominated": true }),
        Some(&key),
    )
    .await?;
    if let engram_core::types::session_op::EnqueueOutcome::Claimed(_) = outcome {
        tracing::info!(
            %session_id,
            allow_park,
            "eviction scanner: enqueued evict op (claimed)"
        );
    }
    Ok(())
}

/// ADR 0074 addendum (2026-07-13): the dwell cap is RETIRED as a reclaim
/// trigger. `None` (the default) = descent is pressure-driven only, which
/// is what this ADR's own Decision always said ("Pressure-driven descent
/// becomes the ONLY mode; the clock TTL survives as candidacy, never as a
/// reclaim trigger"). The shipped 900s clock violated that: it descended
/// parked VMs on hosts with abundant free RAM, and a descent costs the
/// full guest rebuild on return — measured 26.9s of `wait_agent_ready` +
/// `SpawnHarness` against 562ms of actual byte movement (prod trace
/// 2026-07-13). Its stated rationale ("stale guest TCP") doesn't
/// distinguish the paths: a rung-4 restore resumes the guest from a
/// memory snapshot whose TCP state is equally stale.
///
/// `ENGRAM_PARK_DWELL_SECS` survives as an operator escape hatch: set it
/// to re-arm a clock-based descent. The absolute ceiling remains the idle
/// detector's hard TTL, which evicts parked sessions regardless.
fn park_dwell_cap() -> Option<chrono::Duration> {
    let secs = std::env::var("ENGRAM_PARK_DWELL_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())?;
    (secs > 0).then(|| chrono::Duration::seconds(secs))
}

/// ADR 0074 rungs 2-3: advance one PARKED session (park_rung >= 2). The
/// pipeline already ran and left it deliberately at a cheaper rung; this
/// reaper decides whether to keep holding or DESCEND to a full eviction.
///
/// Descent triggers (rung 2, parked-paused):
/// - **dwell**: parked longer than [`park_dwell_cap`] — reclaim the RAM.
/// - **pressure**: the host lost memory headroom — the whole point of
///   parking was to use spare RAM; once it's scarce the parked VM must
///   yield.
///
/// ADR 0079: descending ENQUEUES an evict op with `allow_park = false`
/// (forcing the full snapshot+destroy regardless of headroom); the verb
/// un-pauses the VM as its first pipeline leg. The idempotency key is
/// stable per park instant, so repeat ticks while the op is in flight
/// dedup to `Duplicate`. On success the session lands at `Idle` with a
/// durable snapshot and `park_rung` cleared, exactly like a plain idle
/// eviction — the parking was a transparent latency optimization that
/// left no trace in the terminal state.
#[tracing::instrument(name = "idle_evictor.park_reaper", skip_all, fields(session_id = %session.id, park_rung = session.park_rung))]
async fn park_reaper_advance_one(
    state: &SharedState,
    session: engram_core::types::Session,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;
    let Some(_sandbox_id) = session.sandbox_id else {
        // A parked row with no bound sandbox is inconsistent (park
        // implies a live VM). ADR 0101 C: fall it to HostLost directly —
        // a `parked` row is no longer in the eviction scanner's sweep,
        // so there is no "next tick" that would repair it.
        tracing::warn!(%session_id, "park reaper: parked row has no sandbox; falling to HostLost");
        let _ = state
            .services
            .meta
            .set_session_park_rung(session_id, 0, None)
            .await;
        match state
            .services
            .meta
            .transition_session(session_id, SessionState::HostLost)
            .await
        {
            Ok(prev) => {
                let _ = state
                    .emit(
                        session_id,
                        crate::state::SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::HostLost,
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                tracing::warn!(%session_id, error = %e,
                    "park reaper: Parked→HostLost fallback failed (raced; retrying next tick)");
            }
        }
        return Ok(());
    };

    // Rung 2 (parked-paused) is the only rung this reaper descends today;
    // rung 3 (parked-local) is handled by the data-plane retention path.
    if session.park_rung != 2 {
        return Ok(());
    }

    // ADR 0074 addendum: pressure is the primary (normally the only)
    // trigger. A parked VM on a host with headroom KEEPS ITS RAM — the
    // user's return is then an un-pause (ms) instead of a ~27s rebuild.
    let has_headroom = host_has_memory_headroom(state, session_id).await;
    // The absolute ceiling. The idle DETECTOR's hard TTL can't reach a
    // parked session (it scans `Active` rows; a parked one is `Parked`),
    // so with the dwell clock retired the reaper owns the ceiling: an
    // abandoned park descends after the same hard TTL, measured from the
    // park instant (which trails the session's last event by the soft
    // window — an intentional, bounded overshoot, not a second clock).
    let hard_cap = crate::idle_detector::IdleDetectorConfig::from_env().hard_ttl;
    let parked_for = session
        .parked_at
        .map(|at| state.services.clock.now_utc().signed_duration_since(at))
        .unwrap_or_else(chrono::Duration::zero);
    let hard_exceeded = parked_for.to_std().is_ok_and(|elapsed| elapsed >= hard_cap);
    // Operator escape hatch (off by default) — see `park_dwell_cap`.
    let dwell_exceeded = park_dwell_cap().is_some_and(|cap| parked_for >= cap);
    let reason = if !has_headroom {
        "pressure"
    } else if hard_exceeded {
        "hard_ttl"
    } else if dwell_exceeded {
        "dwell"
    } else {
        // Host has room and the ceiling is far off — HOLD THE PARK. This
        // is the ladder's whole point: idle sessions keep their VM (and
        // therefore their ms-fast ascent) until the RAM is actually needed.
        return Ok(());
    };

    descend_parked_session(state, &session, reason).await?;
    Ok(())
}

/// ADR 0101 C: THE parked-descent primitive — flip `Parked → Evicting`
/// BEFORE enqueueing, so the descent op's entry guard (which accepts
/// nominated work only from `Evicting`) can never skip-loop against a
/// still-`parked` row (enqueue-first was the ADR 0077×0090 livelock
/// shape: a state-guard Skip terminalizes the op, terminal rows leave
/// the dedup index, and the next tick mints a fresh op forever). Crash
/// between the transition and the enqueue is self-correcting: the
/// eviction scanner finds the op-less `Evicting` row and enqueues a
/// generic evict, which re-parks if pressure has abated or descends if
/// it persists.
///
/// Shared by the park reaper and the admin drain — the drain's first
/// version enqueued a raw NON-nominated evict against the still-parked
/// row, which the pipeline's entry guard skipped straight to `Done`
/// while the drain reported "evacuating" (engrams review on PR #843):
/// transition-first + `nominated: true` is a contract, so it lives in
/// one place.
///
/// Returns `Ok(true)` when the descent is in flight after this call
/// (fresh enqueue or an already-queued duplicate), `Ok(false)` when
/// the `Parked → Evicting` flip raced (un-park ascent, delete, host
/// death) — the fresh status owns the next move and nothing was
/// enqueued.
pub(crate) async fn descend_parked_session(
    state: &SharedState,
    session: &engram_core::types::Session,
    reason: &'static str,
) -> Result<bool, engram_core::MetaError> {
    let session_id = session.id;
    match state
        .services
        .meta
        .transition_session(session_id, SessionState::Evicting)
        .await
    {
        Ok(prev) => {
            let _ = state
                .emit(
                    session_id,
                    crate::state::SessionEvent::StatusChanged {
                        from: prev,
                        to: SessionState::Evicting,
                        at: state.services.clock.now_utc(),
                    },
                )
                .await;
        }
        Err(e) => {
            tracing::debug!(%session_id, error = %e, reason,
                "parked descent: Parked→Evicting transition raced; skipping");
            return Ok(false);
        }
    }
    // Key the descent to the park instant: at most one descent op per
    // park, dedup'd across ticks, replicas, AND entry points (a drain
    // racing the reaper collapses to one op).
    let key = format!(
        "evict-descend:{}",
        session
            .parked_at
            .map(|t| t.timestamp_millis())
            .unwrap_or_default()
    );
    let outcome = crate::session_ops::enqueue(
        state,
        session_id,
        engram_core::types::session_op::OpKind::Evict,
        serde_json::json!({ "target": "idle", "allow_park": false, "nominated": true }),
        Some(&key),
    )
    .await?;
    if !matches!(
        outcome,
        engram_core::types::session_op::EnqueueOutcome::Duplicate
    ) {
        ::metrics::counter!(crate::metrics::EVICTION_PARK_DESCEND_TOTAL, "reason" => reason)
            .increment(1);
        tracing::info!(%session_id, reason, "enqueued descent op (parked-paused → full eviction)");
    }
    Ok(true)
}

/// Marker that this module exists so unused-arg checkers don't
/// flag the `Arc<dyn SandboxBackend>` we explicitly take below.
#[allow(dead_code)]
fn _backend_unused_check<B: SandboxBackend>(_: std::sync::Arc<B>) {}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_core::traits::SessionFence;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
    use engram_core::types::session::SessionMode;
    use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState, SessionOp};
    use engram_core::types::{Session, SessionState};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Claim an evict op for `id` and drive it to a terminal row state —
    /// the executor's own drive path, run synchronously so tests are
    /// deterministic. Returns the terminal op row.
    async fn drive_evict(
        state: &SharedState,
        id: engram_core::SessionId,
        allow_park: bool,
        nominated: bool,
    ) -> SessionOp {
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                id,
                OpKind::Evict,
                serde_json::json!({
                    "target": "idle", "allow_park": allow_park, "nominated": nominated
                }),
                None,
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy at claim: {other:?}"),
        };
        let op_id = op.id;
        crate::session_ops::drive_claimed(state, op).await;
        state
            .services
            .meta
            .op_get(op_id)
            .await
            .expect("op_get")
            .expect("op row exists")
    }

    fn build_state_with_session(session: Session, sandbox_root: &Path) -> SharedState {
        build_state_and_meta(session, sandbox_root).0
    }

    /// Variant that also hands back the MiniMeta so tests can reach
    /// its failure-injection toggles (`fail_next_record_snapshot`).
    fn build_state_and_meta(session: Session, sandbox_root: &Path) -> (SharedState, Arc<MiniMeta>) {
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes")));
        build_state_and_meta_with_backend(session, sandbox_root, backend)
    }

    /// ADR 0045 D5: variant taking the sandbox backend, so tests can
    /// wire one that supports the snapshot begin/wait split.
    fn build_state_and_meta_with_backend(
        session: Session,
        sandbox_root: &Path,
        backend: Arc<dyn SandboxBackend>,
    ) -> (SharedState, Arc<MiniMeta>) {
        build_state_and_meta_with_backend_and_blob(session, sandbox_root, backend, None)
    }

    /// #792: variant that also injects the coordinator's `BlobStorage`, so
    /// the recoverable-guard tests can script `head` faults
    /// (`FaultyBlobStorage`) that drive `verify_snapshot_recoverable` to
    /// `recoverable = false` on a manifest-bearing capture. `None` keeps the
    /// default shared LocalBlobStorage.
    fn build_state_and_meta_with_backend_and_blob(
        session: Session,
        sandbox_root: &Path,
        backend: Arc<dyn SandboxBackend>,
        blob: Option<Arc<dyn engram_core::traits::BlobStorage>>,
    ) -> (SharedState, Arc<MiniMeta>) {
        let local_path = sandbox_root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let services = Services {
            meta: meta.clone(),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
            blob: blob.unwrap_or_else(|| {
                std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ))
            }),
            chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = CoordinatorConfig {
            local_path,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
        )
    }

    /// ADR 0047: `resolve_sandbox` must read the live binding from
    /// Postgres (`sessions.sandbox_id`), NOT a per-replica in-memory
    /// map. This is what makes `/exec` / `/prompt` / `/shell` work
    /// behind a multi-replica coordinator: a replica that never fielded
    /// the create/bind still resolves the sandbox. The ADR 0048 fleet
    /// load test surfaced the bug this guards against — with the old
    /// in-memory `SandboxRegistry`, a create on pod A then an exec on
    /// pod B 409'd with "session has no live sandbox" even though the
    /// sandbox was alive. We simulate "another replica bound it" by
    /// writing the binding straight to the store; this AppState never
    /// sees a bind call.
    #[tokio::test]
    async fn resolve_sandbox_reads_pg_so_any_replica_routes() {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:resolve".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let (state, _meta) = build_state_and_meta(session, sandbox_root.path());

        // Nothing bound yet → genuinely no live sandbox.
        assert_eq!(state.resolve_sandbox(session_id).await, None);

        // Another replica persists the binding (the create/resume path
        // writes `sessions.sandbox_id`). This AppState never cached it.
        let sandbox_id = engram_core::SandboxId::new();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        assert_eq!(
            state.resolve_sandbox(session_id).await,
            Some(sandbox_id),
            "a replica that never bound the session must still resolve it via PG",
        );

        // Eviction/teardown clears the binding → resolves None again.
        state
            .services
            .meta
            .assign_session_sandbox(session_id, None)
            .await
            .unwrap();
        assert_eq!(state.resolve_sandbox(session_id).await, None);
    }

    /// ADR 0050 B: a long-lived stream wrapped with the shutdown signal
    /// (as `/events` and `/exec/stream` are) must END when the
    /// coordinator begins graceful shutdown — otherwise it blocks hyper's
    /// drain until SIGKILL (tokio-rs/axum#2673). Exercises the real
    /// `subscribe_shutdown` / `trigger_shutdown` wiring + the `take_until`
    /// pattern the SSE handlers use.
    #[tokio::test]
    async fn shutdown_ends_a_subscribed_stream() {
        use futures::StreamExt as _;
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:shutdown".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let (state, _meta) = build_state_and_meta(session, sandbox_root.path());

        // A stream that never ends on its own — like an idle SSE subscriber.
        let mut shutdown_rx = state.subscribe_shutdown();
        let shutdown = async move {
            let _ = shutdown_rx.wait_for(|shutting_down| *shutting_down).await;
        };
        let stream = futures::stream::pending::<u8>().take_until(shutdown);
        tokio::pin!(stream);

        // Begin graceful shutdown; the never-ending stream must terminate.
        state.trigger_shutdown();
        let next = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("take_until must end the stream promptly on shutdown");
        assert!(
            next.is_none(),
            "stream must end (None), not yield, on shutdown"
        );
    }

    fn process_spec() -> SandboxSpec {
        SandboxSpec {
            image: "evict-test".into(),
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
        }
    }

    /// ADR 0045 D5 (issue #529): a backend exposing `snapshot_begin` —
    /// the coordinator no longer calls `snapshot_wait`/`commit_snapshot`/
    /// `abort_snapshot`/`destroy` on this flavor at all (the finalize is
    /// host-owned from here), so the mock only needs to hand back a
    /// `SnapshotId` and stash it for the test to simulate the host's
    /// heartbeat reconcile landing the row. Delegates everything else to
    /// ProcessBackend.
    struct D5SplitBackend {
        inner: Arc<dyn SandboxBackend>,
        stashed: Arc<PlMutex<Option<engram_core::types::snapshot::SnapshotMetadata>>>,
    }
    use engram_core::SandboxError;
    use parking_lot::Mutex as PlMutex;

    #[async_trait::async_trait]
    impl SandboxBackend for D5SplitBackend {
        async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            self.inner.create(spec).await
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.inner.destroy(id).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            self.inner.list().await
        }
        async fn exec_stream(
            &self,
            id: SandboxId,
            cmd: ExecRequest,
        ) -> Result<engram_core::types::sandbox::ExecStream, SandboxError> {
            self.inner.exec_stream(id, cmd).await
        }
        async fn snapshot(
            &self,
            id: SandboxId,
        ) -> Result<engram_core::types::snapshot::SnapshotMetadata, SandboxError> {
            self.inner.snapshot(id).await
        }
        async fn snapshot_begin(
            &self,
            id: SandboxId,
        ) -> Result<engram_core::types::SnapshotId, SandboxError> {
            let m = self.inner.snapshot(id).await?;
            let sid = m.id;
            *self.stashed.lock() = Some(m);
            Ok(sid)
        }
        async fn restore(
            &self,
            metadata: engram_core::types::snapshot::SnapshotMetadata,
        ) -> Result<SandboxId, SandboxError> {
            self.inner.restore(metadata).await
        }
        fn snapshot_path_for(&self, id: engram_core::types::SnapshotId) -> std::path::PathBuf {
            self.inner.snapshot_path_for(id)
        }
    }

    fn d5_state(
        session: Session,
        sandbox_root: &Path,
    ) -> (
        SharedState,
        Arc<MiniMeta>,
        Arc<PlMutex<Option<engram_core::types::snapshot::SnapshotMetadata>>>,
    ) {
        let stashed = Arc::new(PlMutex::new(None));
        let backend: Arc<dyn SandboxBackend> = Arc::new(D5SplitBackend {
            inner: Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes"))),
            stashed: stashed.clone(),
        });
        let (state, meta) = build_state_and_meta_with_backend(session, sandbox_root, backend);
        (state, meta, stashed)
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Issue #529: the D5 fast path flips Idle before the finalize row
    /// exists (unchanged — the whole point of D5), but the coordinator
    /// no longer writes that row itself. It lands only once something
    /// (in prod: the host's heartbeat reconcile) calls `record_snapshot`
    /// — simulated here directly, standing in for the host-owned
    /// finalize job this test harness doesn't run. The row-watcher must
    /// notice it via `get_snapshot` polling and release its lease
    /// without the coordinator ever calling `commit_snapshot`/`destroy`
    /// on this path.
    /// Review finding #11: the D5 evict op FINISHES the instant the
    /// session is durably Idle — it does NOT hold the session's one-running
    /// op lane through the (host-owned) finalize upload. The old inline
    /// 900s finalize row-watch blocked any queued resume/deliver behind
    /// it; now the finalize is genuinely host-owned (lands via the
    /// heartbeat reconcile) and the lane is free the moment it's Idle.
    #[tokio::test]
    async fn d5_eviction_is_idle_and_frees_the_lane_without_watching_finalize() {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:d5".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let (state, meta, _stashed) = d5_state(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // The evict op returns Done promptly — NO finalize watch to wait
        // out (this would have blocked up to 900s before finding #11).
        let op = drive_evict(&state, session_id, false, false).await;
        assert_eq!(
            op.state,
            OpState::Done,
            "the evict op finishes the instant the capture lands, not after finalize",
        );
        // ADR 0101 C: the session stays HONESTLY Evicting until the
        // recoverable snapshot row lands — Idle-at-capture is retired.
        assert_eq!(
            meta.session.lock().status,
            SessionState::Evicting,
            "no Idle before the recoverable snapshot row exists (ADR 0101 C floor)",
        );
        assert_eq!(
            meta.session.lock().sandbox_id,
            Some(sandbox_id),
            "the sandbox stays bound until the settle detaches it",
        );
        assert!(
            meta.snapshots.lock().is_empty(),
            "row-only-at-host-finalize: no snapshot row before the host lands it",
        );
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the coordinator must NOT destroy the sandbox — that's the host-owned \
             finalize job's job now",
        );
        // The lane is FREE the instant it's Idle: a fresh resume op claims
        // immediately (the finding-#11 property — no 900s block).
        let resume = state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Resume,
                serde_json::json!({}),
                None,
                "pod",
            )
            .await
            .expect("enqueue resume");
        assert!(
            matches!(resume, EnqueueOutcome::Claimed(_)),
            "a resume must claim the lane immediately after the evict op finishes; got {resume:?}",
        );

        // ADR 0101 C: the settle — once the host's recoverable row lands
        // (the heartbeat reconcile's `record_snapshot`), one guarded
        // store op flips `Evicting → Idle` and detaches. Simulated here
        // by planting the row and calling the settle directly.
        let snapshot_id = engram_core::types::SnapshotId::new();
        meta.snapshots
            .lock()
            .push(engram_core::types::snapshot::SnapshotRecord {
                id: snapshot_id,
                session_id: Some(session_id),
                host_id: None,
                image_version: "test/repo:d5".into(),
                size_bytes: 0,
                created_at: chrono::Utc::now(),
                last_accessed_at: chrono::Utc::now(),
                disk_manifest: None,
                memory_manifest: None,
                recoverable: true,
                aux_bundles: Vec::new(),
                events_cursor: Some(0),
                fc_snapshot_version: None,
            });
        assert!(
            state
                .services
                .meta
                .settle_evicted_session_idle(session_id, sandbox_id, snapshot_id)
                .await
                .unwrap(),
            "a recoverable row + matching binding must settle the session Idle",
        );
        assert_eq!(meta.session.lock().status, SessionState::Idle);
        assert_eq!(
            meta.session.lock().sandbox_id,
            None,
            "the settle detaches the sandbox in the same guarded write",
        );
    }

    #[tokio::test]
    async fn evict_op_runs_full_pipeline() {
        // ADR 0005: the eviction pipeline is now snapshot + destroy +
        // mark-Idle only. The auto-checkpoint pre-step is gone — git
        // is no longer the platform's durability primitive.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "echo agent > out.txt".into()],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        state.services.host.exec(sandbox_id, req).await.unwrap();

        let mut sub = state.events.subscribe(session_id);

        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done, "eviction op should succeed");

        // Session is Idle and its sandbox_id is cleared (PG authority).
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
        assert_eq!(after.sandbox_id, None);

        // A snapshot was recorded.
        let snaps = state
            .services
            .meta
            .list_snapshots_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1, "exactly one snapshot recorded");

        // Drain the bus and confirm the post-checkpoint sequence:
        // SnapshotTaken / Evicted / StatusChanged.
        let mut kinds: Vec<String> = Vec::new();
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
            if let Ok(indexed) = ev {
                kinds.push(indexed.event.kind().to_string());
            }
        }
        for required in ["snapshot_taken", "evicted", "status_changed"] {
            assert!(
                kinds.iter().any(|k| k == required),
                "missing event {required} in {kinds:?}"
            );
        }
        assert!(
            !kinds.iter().any(|k| k == "checkpoint_pushed"),
            "ADR 0005: checkpoint_pushed must no longer be emitted (got {kinds:?})"
        );
    }

    /// ADR 0014 issue #1/#2 regression guard. When `record_snapshot`
    /// fails post-snapshot, the pipeline MUST call
    /// `host.abort_snapshot(sandbox_id)` before bubbling — without
    /// this, the host leaks the per-snapshot dir (4 GiB on FC), which
    /// is exactly the failure mode that filled `engrams-fc-xngk` in
    /// 13 minutes.
    #[tokio::test]
    async fn evict_op_aborts_snapshot_when_record_snapshot_fails() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // Spy that wraps the real HostRegistry and counts
        // commit_snapshot + abort_snapshot calls.
        struct SpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
            commits: StdArc<AtomicU32>,
        }

        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for SpyHost {
            // Pass through all required methods to inner.
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id, fence).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn probe_sandbox(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError>
            {
                self.inner.probe_sandbox(id).await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id, fence).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.inner.commit_snapshot(id, fence).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id, fence).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
                fence: SessionFence,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata, fence).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy, fence).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<std::net::Ipv4Addr> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
                binding_epoch: u64,
            ) {
                self.inner
                    .bind_session(session_id, sandbox_id, binding_epoch)
                    .await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                prompt_id: String,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, prompt_id, text).await
            }
        }

        // Wire up state with the spy wrapping the standard
        // HostRegistry → LocalHostClient → ProcessBackend stack, then
        // toggle MiniMeta to fail the next record_snapshot.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        *meta.fail_next_record_snapshot.lock() = true;
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let aborts = StdArc::new(AtomicU32::new(0));
        let commits = StdArc::new(AtomicU32::new(0));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(SpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
            commits: commits.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-abort-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-abort-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = drive_evict(&state, session_id, false, false).await;
        assert_eq!(
            op.state,
            OpState::Queued,
            "a record_snapshot failure is retryable — the op requeues with backoff",
        );
        assert!(
            op.error
                .as_deref()
                .unwrap_or("")
                .contains("idle evict meta"),
            "the pipeline error is recorded on the row: {:?}",
            op.error,
        );

        assert_eq!(
            aborts.load(Ordering::SeqCst),
            1,
            "host.abort_snapshot must fire exactly once after record_snapshot failure",
        );
        assert_eq!(
            commits.load(Ordering::SeqCst),
            0,
            "host.commit_snapshot must NOT fire when pipeline failed",
        );
    }

    /// Sanity inverse: full pipeline success → commit fires, no abort.
    #[tokio::test]
    async fn evict_op_commits_snapshot_on_full_success() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // Tiny duplicate of the spy from the abort test — keeps the
        // tests independently readable.
        struct SpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
            commits: StdArc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for SpyHost {
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id, fence).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn probe_sandbox(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError>
            {
                self.inner.probe_sandbox(id).await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id, fence).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.inner.commit_snapshot(id, fence).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id, fence).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
                fence: SessionFence,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata, fence).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy, fence).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<std::net::Ipv4Addr> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
                binding_epoch: u64,
            ) {
                self.inner
                    .bind_session(session_id, sandbox_id, binding_epoch)
                    .await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                prompt_id: String,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, prompt_id, text).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let aborts = StdArc::new(AtomicU32::new(0));
        let commits = StdArc::new(AtomicU32::new(0));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(SpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
            commits: commits.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-commit-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-commit-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = drive_evict(&state, session_id, false, false).await;
        assert_eq!(op.state, OpState::Done, "full pipeline must succeed");

        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "commit_snapshot must fire exactly once on full pipeline success",
        );
        assert_eq!(
            aborts.load(Ordering::SeqCst),
            0,
            "abort_snapshot must NOT fire on full pipeline success",
        );
    }

    /// ADR 0034 durability regression guard. `commit_snapshot` MUST run
    /// while the sandbox is still bound — BEFORE `unbind`/`destroy`.
    /// With the old ordering (commit after destroy), `resolve_owner`
    /// returned NotFound, the host never cleared its in-flight tracking,
    /// and the racing periodic-checkpoint driver
    /// `abort_prior_inflight_snapshot`-ed the committed blobs out of
    /// BlobStorage while PG still advertised the snapshot as
    /// `recoverable=true` — bricking the resume (prod incident
    /// 89f7984d). Asserts the call order snapshot → commit → destroy.
    #[tokio::test]
    async fn evict_op_commits_before_destroy() {
        use std::sync::Arc as StdArc;
        use std::sync::Mutex as StdMutex;

        // Spy that records the order of the lifecycle calls we care
        // about; everything else passes straight through to inner.
        struct OrderSpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            seq: StdArc<StdMutex<Vec<&'static str>>>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for OrderSpyHost {
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.seq.lock().unwrap().push("destroy");
                self.inner.destroy(id, fence).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn probe_sandbox(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError>
            {
                self.inner.probe_sandbox(id).await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.seq.lock().unwrap().push("snapshot");
                self.inner.snapshot(id, fence).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.seq.lock().unwrap().push("commit");
                self.inner.commit_snapshot(id, fence).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.seq.lock().unwrap().push("abort");
                self.inner.abort_snapshot(id, fence).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
                fence: SessionFence,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata, fence).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy, fence).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<std::net::Ipv4Addr> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
                binding_epoch: u64,
            ) {
                self.inner
                    .bind_session(session_id, sandbox_id, binding_epoch)
                    .await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                prompt_id: String,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, prompt_id, text).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-order".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let seq = StdArc::new(StdMutex::new(Vec::new()));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(OrderSpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            seq: seq.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-order-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-order-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = drive_evict(&state, session_id, false, false).await;
        assert_eq!(op.state, OpState::Done, "full pipeline must succeed");

        let seq = seq.lock().unwrap().clone();
        let snapshot_pos = seq
            .iter()
            .position(|&s| s == "snapshot")
            .expect("snapshot must be called");
        let commit_pos = seq
            .iter()
            .position(|&s| s == "commit")
            .expect("commit_snapshot must be called");
        let destroy_pos = seq
            .iter()
            .position(|&s| s == "destroy")
            .expect("destroy must be called");
        assert!(
            snapshot_pos < commit_pos,
            "snapshot must precede commit; seq={seq:?}",
        );
        assert!(
            commit_pos < destroy_pos,
            "commit_snapshot must run BEFORE destroy (else resolve_owner NotFound \
             → committed blobs aborted while recoverable=true); seq={seq:?}",
        );
        assert!(
            !seq.contains(&"abort"),
            "no abort on the happy path; seq={seq:?}",
        );
    }

    /// ADR 0079 regression guard (the retired session-lease's job): with
    /// a rival pod's op RUNNING on the session, a fresh evict enqueue
    /// QUEUES behind it — the session is untouched until the rival
    /// finishes, at which point the queue drive runs the eviction to
    /// completion, in order.
    #[tokio::test]
    async fn op_claim_serializes_concurrent_drivers() {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:reentry".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // A rival pod's op is mid-flight (one running op per session —
        // the `session_ops_one_running` invariant).
        let rival = meta.ops.seed_running(session_id, OpKind::Resume);

        // A fresh evict enqueue must QUEUE, not claim.
        let outcome = state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": false, "nominated": false }),
                None,
                "test-pod",
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, EnqueueOutcome::Queued(_)),
            "an enqueue behind a running op must queue, got {outcome:?}",
        );

        // Session untouched while the rival runs.
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active);
        assert!(after.sandbox_id.is_some());

        // The rival finishes; the queue drive picks the evict up in
        // order and runs it to completion.
        assert!(meta
            .ops
            .finish(rival.id, rival.epoch.unwrap(), OpState::Done, None));
        crate::session_ops::drive_session(&state, session_id).await;

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Idle,
            "after the rival op finishes, the queued eviction must complete",
        );
        assert!(
            meta.ops.running_for(session_id).is_none(),
            "no op left running after the drive",
        );
    }

    /// ADR 0016 §A.1.6 regression guard. The eviction pipeline
    /// must commit `transition_session(Idle)` to PG **before**
    /// calling `host.destroy(sandbox_id)`. Pre-fix ordering put
    /// destroy() at step 3 with transition_session at step 6 —
    /// the ~ms-to-seconds gap let the heartbeat-driven reconciler
    /// observe the now-orphan session, run HostLost→Idle, and
    /// race ahead of the pipeline's own PG flip.
    ///
    /// This test wraps the standard HostClient stack with a spy
    /// whose `destroy()` reads `MiniMeta`'s current session status
    /// at call time and stashes it. After the evict op
    /// completes, the stashed status must be Idle — proving the
    /// transition committed before destroy() was invoked.
    #[tokio::test]
    async fn evict_op_transitions_to_idle_before_destroy() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        struct OrderedSpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            meta: StdArc<MiniMeta>,
            destroys: StdArc<AtomicU32>,
            status_at_destroy: StdArc<parking_lot::Mutex<Option<SessionState>>>,
            session_id: SessionId,
        }

        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for OrderedSpyHost {
            async fn create(
                &self,
                spec: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.destroys.fetch_add(1, Ordering::SeqCst);
                // Read the session's status straight from MiniMeta
                // at the precise moment destroy() is invoked. The
                // pipeline reorder means this must already be Idle.
                let snapshot = self.meta.session.lock().clone();
                if snapshot.id == self.session_id {
                    *self.status_at_destroy.lock() = Some(snapshot.status);
                }
                self.inner.destroy(id, fence).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn probe_sandbox(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError>
            {
                self.inner.probe_sandbox(id).await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id, fence).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.commit_snapshot(id, fence).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.abort_snapshot(id, fence).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
                fence: SessionFence,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata, fence).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy, fence).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<std::net::Ipv4Addr> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
                binding_epoch: u64,
            ) {
                self.inner
                    .bind_session(session_id, sandbox_id, binding_epoch)
                    .await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                prompt_id: String,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, prompt_id, text).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:reorder".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let destroys = StdArc::new(AtomicU32::new(0));
        let status_at_destroy: StdArc<parking_lot::Mutex<Option<SessionState>>> =
            StdArc::new(parking_lot::Mutex::new(None));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(OrderedSpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            meta: meta.clone(),
            destroys: destroys.clone(),
            status_at_destroy: status_at_destroy.clone(),
            session_id,
        });

        let services = Services {
            meta: meta.clone(),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-reorder-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-reorder-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = drive_evict(&state, session_id, false, false).await;
        assert_eq!(
            op.state,
            OpState::Done,
            "full pipeline must succeed under the reordered steps"
        );

        assert_eq!(
            destroys.load(Ordering::SeqCst),
            1,
            "destroy must be called exactly once on full success",
        );
        // The crux of A.1.6: at the moment destroy() ran, the
        // session was already Idle in MiniMeta. If this regresses
        // (Idle-after-destroy ordering returns), the snapshot will
        // be Active (or Created/some-mid-state) and the
        // reconciler-race window reopens.
        assert_eq!(
            *status_at_destroy.lock(),
            Some(SessionState::Idle),
            "ADR 0016 §A.1.6: transition_session(Idle) must commit \
             to PG BEFORE host.destroy() is invoked",
        );

        // Final state: still Idle (sanity).
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
    }

    #[tokio::test]
    async fn evict_op_is_a_noop_when_sandbox_already_unbound() {
        // Race-safe path: another actor already unbound the sandbox
        // before this op claimed. The pipeline must complete as a no-op
        // (op Done) without touching anything else.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        let op = drive_evict(&state, session_id, false, false).await;
        assert_eq!(op.state, OpState::Done, "unbound session evict is a no-op");

        // Session stays Active (no eviction happened).
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active);
    }

    // ─── ADR 0034: eviction scanner tests ─────────────────────────

    fn evicting_session(id: engram_core::SessionId) -> Session {
        Session {
            id,
            status: SessionState::Evicting,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-scanner".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    /// Happy path: the scanner sweeps an Evicting row and drives the
    /// full pipeline — session lands Idle with a recorded snapshot,
    /// sandbox unbound. This is the nomination handler's other half.
    #[tokio::test]
    async fn scanner_drives_evicting_to_idle() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(evicting_session(session_id), sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // The scanner ENQUEUES the evict op (claimed → driven on a
        // detached task); observe the outcome.
        scanner_run_once(&state).await.expect("scanner tick");

        let st = state.clone();
        wait_for("scanner-enqueued evict op lands the session at Idle", {
            let st = st.clone();
            move || {
                futures::executor::block_on(st.services.meta.get_session(session_id))
                    .map(|s| s.status == SessionState::Idle)
                    .unwrap_or(false)
            }
        })
        .await;
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
        assert_eq!(after.sandbox_id, None);
        let snaps = state
            .services
            .meta
            .list_snapshots_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1, "pipeline recorded its snapshot");
    }

    /// A failed pipeline attempt leaves the row Evicting with the op
    /// REQUEUED (backoff + attempt counted on the row) — the executor
    /// retries. (This is the crash-/flake-tolerant replacement for the
    /// pre-0034 silent cancellation.)
    #[tokio::test]
    async fn scanner_failure_stays_evicting_and_requeues_the_op() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let (state, mini) = build_state_and_meta(evicting_session(session_id), sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // Force the pipeline's record_snapshot step to fail once.
        *mini.fail_next_record_snapshot.lock() = true;

        scanner_run_once(&state)
            .await
            .expect("tick itself succeeds; per-session failure rides the op row");

        // The scanner's enqueue claimed + spawned the drive; wait for the
        // failed attempt to land back at queued-with-backoff.
        let m = mini.clone();
        wait_for(
            "evict op requeued with backoff after the failure",
            move || {
                m.ops.all().iter().any(|o| {
                    o.kind == OpKind::Evict
                        && o.state == OpState::Queued
                        && o.attempts == 1
                        && o.not_before.is_some()
                        && o.error.is_some()
                })
            },
        )
        .await;
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Evicting,
            "stays in lane for retry"
        );
    }

    /// Budget exhaustion falls back to HostLost (not Active — would
    /// re-nominate forever; not Idle — no durable snapshot exists;
    /// not Dead — the runtime is healthy). The budget now lives on the
    /// op row (`attempts`, bumped at every claim): drive the verb with a
    /// hand-aged attempt count past `EVICT_MAX_ATTEMPTS` and a failing
    /// pipeline.
    #[tokio::test]
    async fn evict_op_budget_exhaustion_falls_back_to_host_lost() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let (state, mini) = build_state_and_meta(evicting_session(session_id), sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        *mini.fail_next_record_snapshot.lock() = true;

        let mut sub = state.events.subscribe(session_id);
        // Claim honestly (epoch is real), then age the attempt count on
        // the ctx's view of the row — the verb reads `op.attempts`.
        let mut op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": false, "nominated": true }),
                None,
                "test-pod",
            )
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("expected Claimed, got {other:?}"),
        };
        op.attempts = 21; // past session_verbs::EVICT_MAX_ATTEMPTS
        let epoch = op.epoch.expect("claimed");
        let ctx = crate::session_ops::OpCtx {
            state: &state,
            op: &op,
            epoch,
        };
        let outcome = crate::session_verbs::dispatch(&ctx).await;
        assert!(
            matches!(outcome, crate::session_ops::OpOutcome::Failed(_)),
            "budget exhaustion must be terminal",
        );

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::HostLost);
        // StatusChanged(Evicting → HostLost) emitted for the timeline.
        let indexed = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("event within 1s")
            .expect("bus open");
        match indexed.event {
            SessionEvent::StatusChanged { from, to, .. } => {
                assert_eq!(from, SessionState::Evicting);
                assert_eq!(to, SessionState::HostLost);
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    /// An Evicting row with no bound sandbox is structurally
    /// inconsistent — nothing to evict. Falls back to HostLost
    /// rather than burning 20 attempts on a guaranteed failure.
    #[tokio::test]
    async fn scanner_no_sandbox_falls_back_to_host_lost() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(evicting_session(session_id), sandbox_root.path());

        scanner_run_once(&state).await.expect("tick");

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::HostLost);
    }

    /// No Evicting rows → the tick is a cheap no-op.
    #[tokio::test]
    async fn scanner_empty_sweep_is_noop() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.status = SessionState::Active;
        let state = build_state_with_session(session, sandbox_root.path());

        scanner_run_once(&state).await.expect("tick");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active, "untouched");
    }

    /// Op finishes may land from a detached drive task; tests poll here
    /// so the next claim-acquiring step doesn't race the finish.
    async fn wait_for_op_lane_free(meta: &Arc<MiniMeta>, id: engram_core::SessionId) {
        wait_for("session op lane free", || {
            meta.ops.running_for(id).is_none()
        })
        .await;
    }

    /// ADR 0074 rung 2: push a `HostRecord` into the mock's
    /// `list_active_hosts` set whose id matches the registry host that
    /// owns `sandbox_id`, with a memory utilization that either has or
    /// lacks parking headroom. `free_mib` feeds `allocatable_mib` (the
    /// ledger signal `host_has_memory_headroom` prefers).
    fn seed_host_with_free_ram(meta: &Arc<MiniMeta>, host_id: engram_core::HostId, free_mib: u64) {
        meta.hosts
            .lock()
            .push(engram_core::types::host::HostRecord {
                id: host_id,
                hostname: "park-test".into(),
                cloud_metadata: engram_core::types::host::HostMetadata::default(),
                capacity: engram_core::types::host::HostCapacity {
                    total_gb: 100,
                    used_gb: 10,
                    total_mib: 65_536,
                    used_mib: 0,
                    running_sandboxes: 1,
                },
                utilization: engram_core::types::host::HostUtilization {
                    mem_total_mib: 65_536,
                    mem_used_mib: 65_536 - free_mib,
                    allocatable_mib: free_mib,
                    ..Default::default()
                },
                status: engram_core::types::host::HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: Some("http://127.0.0.1:1".into()),
                ready_images: Vec::new(),
                current_bundles: Vec::new(),
                cordoned: false,
                total_vcpus: 0,
                wire_version: 0,
                stages_images: true,
                capabilities: Default::default(),
            });
    }

    /// ADR 0101 C (engrams review, #836 round 2): once a nomination's
    /// evict op is DONE (capture landed; the session honestly `Evicting`
    /// for the publication window), the scanner must NOT re-mint an op —
    /// the reconcile settle owns the tail. Terminal rows leave the dedup
    /// index by design, so without the grace check every 10s tick minted
    /// a fresh op that re-ran guest RPCs + snapshot_begin against the
    /// finalizing sandbox for the whole window.
    #[tokio::test]
    async fn scanner_does_not_remint_after_capture_landed() {
        let session_id = engram_core::SessionId::new();
        let session = evicting_session(session_id);
        let last_active_at = session.last_active_at;
        let sandbox_root = TempDir::new().unwrap();
        let (state, meta, _stashed) = d5_state(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // Drive the nominated evict op under the SCANNER'S idempotency
        // key — the D5 path completes it with the session left Evicting.
        let key = format!("evict:{}", last_active_at.timestamp_millis());
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": false, "nominated": true }),
                Some(&key),
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        crate::session_ops::drive_claimed(&state, op).await;
        assert_eq!(
            meta.session.lock().status,
            SessionState::Evicting,
            "capture landed; awaiting the reconcile settle",
        );

        let ops_before = meta.ops.all().len();
        scanner_run_once(&state).await.expect("tick");
        assert_eq!(
            meta.ops.all().len(),
            ops_before,
            "within the settle grace the scanner must not re-mint an evict op \
             (the terminal-op/dedup churn shape)",
        );
    }

    /// ADR 0101 C (engrams review, #836 round 3): the suppression must be
    /// KEY-AGNOSTIC — the admin/evict_local path mints its evict op with
    /// NO idempotency key, and the park-descent path mints under
    /// `evict-descend:<parked_at>`; a key-scoped grace check protected
    /// only the scanner's own nomination key and re-minted (with
    /// `allow_park: true`!) for the other two paths' whole publication
    /// window.
    #[tokio::test]
    async fn scanner_suppression_is_key_agnostic_for_admin_evicts() {
        let session_id = engram_core::SessionId::new();
        let mut session = evicting_session(session_id);
        // Admin evicts enter at Active; finish_eviction_d5 transitions
        // Active → Evicting itself. Start Active to mirror that shape.
        session.status = SessionState::Active;
        let sandbox_root = TempDir::new().unwrap();
        let (state, meta, _stashed) = d5_state(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // The admin shape: evict op with key = None (enqueue_and_observe_evict).
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": false, "nominated": false }),
                None,
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        crate::session_ops::drive_claimed(&state, op).await;
        assert_eq!(
            meta.session.lock().status,
            SessionState::Evicting,
            "admin capture landed; awaiting the reconcile settle",
        );

        let ops_before = meta.ops.all().len();
        scanner_run_once(&state).await.expect("tick");
        assert_eq!(
            meta.ops.all().len(),
            ops_before,
            "the keyless admin evict's Done op must suppress the re-mint too",
        );
    }

    /// ADR 0074 rung 2 (parked-paused), ADR 0101 C shape: with host
    /// memory headroom, an idle eviction PAUSES the VM in place instead
    /// of capturing — the session lands in the real `Parked` state
    /// (park_rung=2 kept as ascent/ledger metadata), sandbox alive,
    /// nothing snapshotted. When the user returns, the cancel path
    /// un-pauses and flips back to Active with the same live sandbox.
    #[tokio::test]
    async fn parked_paused_with_headroom_then_ascends_on_cancel() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id); // status = Evicting
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state
            .host_registry
            .host_of(sandbox_id)
            .expect("sandbox routed");
        meta.session.lock().host_id = Some(host_id); // headroom resolves via PG now
        seed_host_with_free_ram(&meta, host_id, 60_000); // ~91% free ≥ 30 floor

        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done, "park completes the op early");
        let s = meta.session.lock().clone();
        assert_eq!(
            s.status,
            SessionState::Parked,
            "parked is a real state (ADR 0101 C), not Evicting + rung stamp",
        );
        assert_eq!(s.park_rung, 2);
        assert!(s.parked_at.is_some());
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "parked VM is NOT destroyed"
        );
        assert!(
            meta.snapshots.lock().is_empty(),
            "parking captures no snapshot"
        );

        // Ascent: the user comes back → un-pause + flip Active. Wait for
        // the park op's lane to free first (prod retries this).
        wait_for_op_lane_free(&meta, session_id).await;
        let cancelled = crate::api::snapshot::try_cancel_nominated_eviction(&state, session_id)
            .await
            .unwrap();
        assert!(cancelled, "parked-paused session is cancellable");
        let s = meta.session.lock().clone();
        assert_eq!(s.status, SessionState::Active);
        assert_eq!(s.park_rung, 0, "rung cleared on ascent");
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "same live sandbox after un-pause — no rebuild"
        );
    }

    /// ADR 0074 addendum (2026-07-13): a long-parked VM on a host with
    /// HEADROOM keeps its RAM — no clock reclaims it. This is the
    /// behavior the shipped 900s dwell cap violated (it descended parked
    /// VMs on empty hosts, costing a ~27s guest rebuild on return for
    /// RAM nobody wanted).
    #[tokio::test]
    async fn long_parked_vm_with_headroom_is_not_descended_by_a_clock() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id);
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state
            .host_registry
            .host_of(sandbox_id)
            .expect("sandbox routed");
        meta.session.lock().host_id = Some(host_id);
        seed_host_with_free_ram(&meta, host_id, 60_000); // abundant headroom

        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done);
        assert_eq!(meta.session.lock().park_rung, 2, "parked-paused");

        // An hour parked — past the RETIRED 900s dwell, far short of the
        // 8h hard ceiling.
        let stale = chrono::Utc::now() - chrono::Duration::seconds(3600);
        state
            .services
            .meta
            .set_session_park_rung(session_id, 2, Some(stale))
            .await
            .unwrap();

        wait_for_op_lane_free(&meta, session_id).await;
        scanner_run_once(&state).await.expect("tick");
        // Give any (incorrectly) enqueued descent a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let s = meta.session.lock().clone();
        assert_eq!(
            s.park_rung, 2,
            "still parked — pressure, not a clock, reclaims a parked VM"
        );
        assert_eq!(s.status, SessionState::Parked, "still parked (ADR 0101 C)");
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the VM (and its ms-fast un-pause ascent) survives"
        );
    }

    /// The absolute ceiling (ADR 0074: "hard TTL remains the absolute
    /// ceiling"). With the dwell clock retired, the reaper owns it — the
    /// idle DETECTOR can't reach a parked session (it scans `Active`; a
    /// parked one is `Evicting`), so without this an abandoned park would
    /// hold RAM until pressure arrived.
    #[tokio::test]
    async fn abandoned_park_descends_at_the_hard_ceiling() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id);
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state
            .host_registry
            .host_of(sandbox_id)
            .expect("sandbox routed");
        meta.session.lock().host_id = Some(host_id);
        seed_host_with_free_ram(&meta, host_id, 60_000); // headroom the whole time

        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done);
        assert_eq!(meta.session.lock().park_rung, 2, "parked-paused");

        // Parked past the 8h hard ceiling with nobody returning.
        let ancient = chrono::Utc::now()
            - chrono::Duration::seconds(crate::idle_detector::DEFAULT_HARD_TTL_SECS as i64 + 60);
        state
            .services
            .meta
            .set_session_park_rung(session_id, 2, Some(ancient))
            .await
            .unwrap();

        wait_for_op_lane_free(&meta, session_id).await;
        scanner_run_once(&state).await.expect("tick");

        {
            let m = meta.clone();
            wait_for("descended to Idle", move || {
                m.session.lock().status == SessionState::Idle
            })
            .await;
        }
        assert_eq!(
            meta.session.lock().park_rung,
            0,
            "the hard ceiling reclaims an abandoned park even with headroom"
        );
    }

    /// ADR 0074's primary trigger: the host loses headroom → the parked
    /// VM yields its RAM via a full eviction (un-pause → capture →
    /// destroy → Idle), leaving no trace of the parking.
    #[tokio::test]
    async fn parked_paused_descends_to_full_eviction_under_pressure() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id);
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state
            .host_registry
            .host_of(sandbox_id)
            .expect("sandbox routed");
        meta.session.lock().host_id = Some(host_id);
        seed_host_with_free_ram(&meta, host_id, 60_000);

        // Park first (needs headroom).
        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done);
        assert_eq!(meta.session.lock().park_rung, 2, "parked-paused");

        // The host now has none — the RAM the park was borrowing is needed.
        // (Mutate the seeded row in place; `seed_host_with_free_ram` PUSHES,
        // so re-seeding would leave the roomy record ahead of it.)
        // MEASURED-but-tiny, not 0: `host_has_memory_headroom` treats a 0
        // allocatable as "pre-ledger host" and falls back to raw physical
        // free, which the fixture reports as plentiful.
        for h in meta.hosts.lock().iter_mut().filter(|h| h.id == host_id) {
            h.utilization.allocatable_mib = 1;
        }

        wait_for_op_lane_free(&meta, session_id).await;
        scanner_run_once(&state).await.expect("tick");

        {
            let m = meta.clone();
            wait_for("descended to Idle", move || {
                m.session.lock().status == SessionState::Idle
            })
            .await;
        }
        let s = meta.session.lock().clone();
        assert_eq!(
            s.status,
            SessionState::Idle,
            "memory pressure descends the park to a full eviction"
        );
        assert_eq!(s.park_rung, 0, "rung cleared after descent");
        assert!(
            !state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "descent destroys the sandbox"
        );
        assert!(
            !meta.snapshots.lock().is_empty(),
            "descent captures a durable snapshot"
        );
    }

    /// ADR 0101 C's third `Evicting` shape — the post-capture settle
    /// window (capture landed, VM DESTROYED, session honestly `Evicting`
    /// until a later heartbeat's advert settles it Idle) — must NOT be
    /// ascendable: pre-fix, a resume racing that window flipped the row
    /// `Active` over a destroyed sandbox (main's e2e stack: no `evicted`
    /// event, "sandbox not found" on first exec, the wedged row held its
    /// reservation until the fleet read as full). The ascent now probes
    /// VM liveness and refuses without positive proof.
    #[tokio::test]
    async fn ascent_refuses_the_post_capture_settle_window() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id);
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state
            .host_registry
            .host_of(sandbox_id)
            .expect("sandbox routed");
        meta.session.lock().host_id = Some(host_id);
        seed_host_with_free_ram(&meta, host_id, 60_000);

        // The nomination window (VM alive, evict not yet run): the ascent
        // must still work — this is the "user came back in time" fast
        // path the probe gate must not break.
        assert_eq!(meta.session.lock().status, SessionState::Evicting);
        let ascended = crate::api::snapshot::try_cancel_nominated_eviction(&state, session_id)
            .await
            .expect("ascent (live VM)");
        assert!(ascended, "a live-VM nomination window still ascends");
        assert_eq!(meta.session.lock().status, SessionState::Active);

        // Now construct the settle window exactly as the D5 pipeline
        // leaves it: status Evicting, sandbox still BOUND in PG, VM
        // already destroyed on the host, no evict op running.
        meta.session.lock().status = SessionState::Evicting;
        state
            .services
            .host
            .destroy(sandbox_id, engram_core::traits::SessionFence::unfenced())
            .await
            .expect("destroy");
        assert!(
            !state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the sandbox is gone (post-capture)"
        );

        wait_for_op_lane_free(&meta, session_id).await;
        // The wire-path ascent (what a resume racing the settle runs).
        let ascended = crate::api::snapshot::try_cancel_nominated_eviction(&state, session_id)
            .await
            .expect("ascent probe");
        assert!(
            !ascended,
            "the post-capture settle window must not ascend to Active"
        );
        assert_eq!(
            meta.session.lock().status,
            SessionState::Evicting,
            "session stays Evicting for the settle — never Active over a destroyed VM"
        );
    }

    /// PR #843 review (engrams): the admin drain must descend a parked
    /// session through the real contract — `Parked → Evicting` FIRST,
    /// then the NOMINATED descent op (`descend_parked_session`) — not a
    /// raw non-nominated enqueue, which the pipeline's entry guard
    /// (`Active | Evicting` only) skips straight to `Done` while the
    /// drain reports "evacuating". Drives `admin_drain_host_core`
    /// against a rung-2 parked session end to end and asserts it lands
    /// durable-Idle with the sandbox destroyed.
    #[tokio::test]
    async fn admin_drain_descends_a_parked_session_to_durable_idle() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id);
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state
            .host_registry
            .host_of(sandbox_id)
            .expect("sandbox routed");
        meta.session.lock().host_id = Some(host_id);
        seed_host_with_free_ram(&meta, host_id, 60_000);

        // Park (rung 2, VM paused in place; status lands `Parked`).
        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done);
        assert_eq!(meta.session.lock().park_rung, 2, "parked-paused");
        assert_eq!(meta.session.lock().status, SessionState::Parked);

        wait_for_op_lane_free(&meta, session_id).await;
        let resp = crate::api::admin::admin_drain_host_core(&state, host_id)
            .await
            .expect("drain");
        assert_eq!(
            resp.evacuating,
            vec![session_id],
            "the parked session is drain work, not an empty success"
        );
        assert!(resp.failures.is_empty(), "failures: {:?}", resp.failures);

        {
            let m = meta.clone();
            wait_for("drained park descended to Idle", move || {
                m.session.lock().status == SessionState::Idle
            })
            .await;
        }
        let s = meta.session.lock().clone();
        assert_eq!(s.park_rung, 0, "rung cleared after descent");
        assert!(
            !state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "descent destroys the sandbox"
        );
        assert!(
            !meta.snapshots.lock().is_empty(),
            "descent captures a durable snapshot"
        );
    }

    /// ADR 0074: a parked row (park_rung >= 2) must NOT be re-driven
    /// through the eviction pipeline by the scanner — that would bump
    /// evict_attempts every tick and eventually fall the session to
    /// HostLost. The scanner routes it to the reaper instead, which (in
    /// dwell + headroom) leaves it parked untouched.
    #[tokio::test]
    async fn parked_row_is_not_re_evicted_by_scanner() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let session = evicting_session(session_id);
        let (state, meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        let host_id = state.host_registry.host_of(sandbox_id).expect("routed");
        meta.session.lock().host_id = Some(host_id); // headroom resolves via PG now
        seed_host_with_free_ram(&meta, host_id, 60_000);
        // Freshly parked (within dwell) with headroom → the reaper holds.
        state
            .services
            .meta
            .set_session_park_rung(session_id, 2, Some(chrono::Utc::now()))
            .await
            .unwrap();

        scanner_run_once(&state).await.expect("tick");

        let s = meta.session.lock().clone();
        assert_eq!(
            s.status,
            SessionState::Evicting,
            "still parked, not evicted"
        );
        assert_eq!(s.park_rung, 2);
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "parked VM untouched by the scanner"
        );
    }

    /// Adversarial-review regression (2026-07-13 incident fix): the
    /// quarantine capture deadline must cover `snapshot_begin` — the
    /// NORMAL prod-FC capture path — not only the composed fallback the
    /// first cut bounded. A wedged `snapshot_begin` (FC blocked on a dead
    /// NBD, or the crawling re-chunk) must become a FAILED attempt
    /// (retryable, counting against the small quarantine budget) with the
    /// in-flight host capture aborted — not an immortal running op whose
    /// heartbeat shields it from reclaim while the user's resume queues
    /// behind it.
    #[tokio::test]
    async fn quarantine_evict_times_out_a_hanging_snapshot_begin() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;
        // Nextest runs each test in its own process, so the override
        // cannot leak into sibling tests.
        std::env::set_var("ENGRAM_QUARANTINE_CAPTURE_TIMEOUT_SECS", "1");

        struct HangingBegin {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
        }

        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for HangingBegin {
            async fn snapshot_begin(
                &self,
                _id: engram_core::SandboxId,
                _fence: SessionFence,
            ) -> Result<engram_core::types::SnapshotId, engram_core::SandboxError> {
                // The prod-FC D5 path, wedged: never returns.
                std::future::pending().await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id, fence).await
            }
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id, fence).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn probe_sandbox(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError>
            {
                self.inner.probe_sandbox(id).await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id, fence).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.commit_snapshot(id, fence).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
                fence: SessionFence,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata, fence).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy, fence).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<std::net::Ipv4Addr> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
                binding_epoch: u64,
            ) {
                self.inner
                    .bind_session(session_id, sandbox_id, binding_epoch)
                    .await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                prompt_id: String,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, prompt_id, text).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:quarantine-timeout".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let aborts = StdArc::new(AtomicU32::new(0));
        let hanging: Arc<dyn engram_core::traits::HostClient> = Arc::new(HangingBegin {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
        });
        let services = Services {
            meta: meta.clone(),
            host: hanging,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-quarantine-timeout-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-quarantine-timeout-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // The ADR 0090 quarantine flavor, exactly as the heartbeat arm
        // enqueues it.
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({
                    "target": "idle",
                    "allow_park": false,
                    "nominated": false,
                    "quarantine": true,
                }),
                Some(&format!("adr0090-quarantine:{sandbox_id}")),
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy at claim: {other:?}"),
        };
        let op_id = op.id;
        crate::session_ops::drive_claimed(&state, op).await;
        let row = state
            .services
            .meta
            .op_get(op_id)
            .await
            .expect("op_get")
            .expect("op row exists");

        assert_eq!(
            row.state,
            OpState::Queued,
            "a timed-out quarantine capture attempt requeues (counts against the budget): {row:?}",
        );
        assert!(
            row.error
                .as_deref()
                .unwrap_or("")
                .contains("snapshot_begin) timed out"),
            "the snapshot_begin deadline is what fired: {:?}",
            row.error,
        );
        assert_eq!(
            aborts.load(Ordering::SeqCst),
            1,
            "the in-flight host-side capture must be aborted on timeout",
        );
    }

    /// The 2026-07-17 resume-stall incident (session 03e6535e): a verb
    /// `await` that never returns (there, `host.start_agent` on a resume
    /// `finish` step, on a dead rootfs device) keeps the op heartbeating
    /// forever, so the stale-op reclaim can never fire and the op is pinned
    /// until a deploy rolls the pod. `drive_one`'s wall-clock deadline
    /// (`op_deadline`) is the backstop: a hung dispatch is cancelled and the
    /// op REQUEUES with backoff — "executor alive but wedged" converges
    /// without waiting on a pod roll. This drives the parked-paused (rung-2)
    /// ascent, whose only host call is `resume`, and hangs it. Sibling of
    /// `quarantine_evict_times_out_a_hanging_snapshot_begin` (which bounds a
    /// DIFFERENT, capture-specific timeout); this one bounds the GENERAL op
    /// executor.
    #[tokio::test]
    async fn resume_op_whose_host_call_wedges_is_requeued_by_the_op_deadline() {
        // Process-local (nextest = process-per-test): a tiny global op
        // deadline so the hang is bounded in ~1s of real time.
        std::env::set_var("ENGRAM_OP_DEADLINE_SECS", "1");

        // Forwards everything to a real inner host, but `resume` never
        // returns — the wedged un-pause.
        struct HangingResume {
            inner: Arc<dyn engram_core::traits::HostClient>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for HangingResume {
            async fn resume(
                &self,
                _sandbox_id: engram_core::SandboxId,
                _fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                std::future::pending().await
            }
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id, fence).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn probe_sandbox(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::sandbox::SandboxProbe, engram_core::SandboxError>
            {
                self.inner.probe_sandbox(id).await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id, fence).await
            }
            async fn snapshot_begin(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<engram_core::types::SnapshotId, engram_core::SandboxError> {
                self.inner.snapshot_begin(id, fence).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.abort_snapshot(id, fence).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.commit_snapshot(id, fence).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
                fence: SessionFence,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata, fence).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
                fence: SessionFence,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy, fence).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<std::net::Ipv4Addr> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
                binding_epoch: u64,
            ) {
                self.inner
                    .bind_session(session_id, sandbox_id, binding_epoch)
                    .await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                prompt_id: String,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, prompt_id, text).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(evicting_session(session_id)));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let hanging: Arc<dyn engram_core::traits::HostClient> = Arc::new(HangingResume {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
        });
        let services = Services {
            meta: meta.clone(),
            host: hanging,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-op-deadline-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-op-deadline-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        // Create + bind a real sandbox so the rung-2 ascent's `resume`
        // routes to our hanging host, and stamp park_rung=2 so the ascent
        // takes the un-pause path.
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();
        {
            let mut s = meta.session.lock();
            s.park_rung = 2;
        }

        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Resume,
                serde_json::json!({}),
                None,
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy at claim: {other:?}"),
        };
        let op_id = op.id;
        crate::session_ops::drive_claimed(&state, op).await;

        let row = state
            .services
            .meta
            .op_get(op_id)
            .await
            .expect("op_get")
            .expect("op row exists");
        assert_eq!(
            row.state,
            OpState::Queued,
            "a wedged resume attempt must requeue via the op deadline, not pin the op: {row:?}",
        );
        assert!(
            row.error
                .as_deref()
                .unwrap_or("")
                .contains("wall-clock deadline"),
            "the op wall-clock deadline is what fired: {:?}",
            row.error,
        );
    }

    // =============== #792: recoverable-before-Idle guard ================

    use bytes::Bytes;
    use engram_core::traits::BlobStorage;
    use engram_core::types::manifest::ManifestRef;
    use engram_storage_local::LocalBlobStorage;
    use engram_testkit::storage::{
        FaultPlan, FaultyBlobStorage, HeadFault, HeadFaultKind, InjectedError, KeyMatch, When,
    };

    /// #792: a backend whose COMPOSED `snapshot()` returns metadata that
    /// carries a disk manifest — unlike ProcessBackend, which is
    /// manifest-less. That makes `verify_snapshot_recoverable` actually HEAD
    /// a manifest key (the surface the recoverable-guard gates). It does NOT
    /// implement `snapshot_begin` (inherits the InvalidSpec default), so the
    /// coordinator takes the COMPOSED idle-evict path where the guard lives.
    /// Everything else delegates to an inner ProcessBackend.
    struct ManifestSnapshotBackend {
        inner: Arc<dyn SandboxBackend>,
        manifest: ManifestRef,
    }

    #[async_trait::async_trait]
    impl SandboxBackend for ManifestSnapshotBackend {
        async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            self.inner.create(spec).await
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.inner.destroy(id).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            self.inner.list().await
        }
        async fn exec_stream(
            &self,
            id: SandboxId,
            cmd: ExecRequest,
        ) -> Result<engram_core::types::sandbox::ExecStream, SandboxError> {
            self.inner.exec_stream(id, cmd).await
        }
        async fn snapshot(
            &self,
            id: SandboxId,
        ) -> Result<engram_core::types::snapshot::SnapshotMetadata, SandboxError> {
            let mut m = self.inner.snapshot(id).await?;
            // Inject a disk manifest so the capture is "manifest-bearing":
            // verify_snapshot_recoverable will HEAD this ref's storage key.
            m.disk_manifest = Some(self.manifest);
            Ok(m)
        }
        async fn restore(
            &self,
            metadata: engram_core::types::snapshot::SnapshotMetadata,
        ) -> Result<SandboxId, SandboxError> {
            self.inner.restore(metadata).await
        }
        fn snapshot_path_for(&self, id: engram_core::types::SnapshotId) -> std::path::PathBuf {
            self.inner.snapshot_path_for(id)
        }
    }

    /// A fresh LocalBlobStorage with `manifest`'s key pre-seeded (so a
    /// non-faulted HEAD succeeds), wrapped in a FaultyBlobStorage running
    /// `plan` — the rig for driving `verify_snapshot_recoverable` to a
    /// scripted `recoverable = false` on a manifest-bearing capture.
    async fn faulty_blob_with_manifest(
        dir: std::path::PathBuf,
        manifest: &ManifestRef,
        plan: FaultPlan,
    ) -> Arc<dyn BlobStorage> {
        let inner: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir));
        inner
            .put(&manifest.storage_key(), Bytes::from_static(b"{}"))
            .await
            .expect("seed manifest blob");
        Arc::new(FaultyBlobStorage::new(inner, plan))
    }

    /// Claim a nominated idle-evict op and hand back the row, so a test can
    /// drive `run_evict_pipeline` directly (re-running the pipeline across a
    /// transient blip without waiting out the op's requeue backoff). Mirrors
    /// `drive_evict`'s claim.
    async fn claim_evict_op(state: &SharedState, session_id: SessionId) -> SessionOp {
        match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": false, "nominated": true }),
                None,
                "test-pod",
            )
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    /// The heartbeat-shaped ADR 0090 quarantine `evict_local`
    /// (non-nominated, no park) — the exact payload
    /// `api::host_http` enqueues per quarantined-survivor advertise.
    async fn claim_quarantine_evict_op(state: &SharedState, session_id: SessionId) -> SessionOp {
        match state
            .services
            .meta
            .op_enqueue_and_claim(
                session_id,
                OpKind::Evict,
                serde_json::json!({
                    "target": "idle", "allow_park": false,
                    "nominated": false, "quarantine": true,
                }),
                None,
                "test-pod",
            )
            .await
            .unwrap()
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("expected Claimed, got {other:?}"),
        }
    }

    /// 2026-07-21 livelock incident (prod 8174b7aa): a quarantine evict
    /// landing on an ADR 0077 harness-failed park (`Created`, still bound
    /// to the quarantined survivor) used to settle `Skipped` in ~10ms —
    /// the host re-advertised every 5s, each advertise re-enqueued, and
    /// the loop ran for 2.5 days. The guard must CONVERGE instead:
    /// destroy the crippled VM (clears the host's quarantine entry — the
    /// advertise source) and settle `HostLost`, the one lane the dead-host
    /// straggler sweep re-drives to Idle/recoverable.
    #[tokio::test]
    async fn quarantine_evict_on_created_park_reaps_and_settles_host_lost() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.status = SessionState::Created;
        let (state, _meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = claim_quarantine_evict_op(&state, session_id).await;
        let ctx = OpCtx {
            state: &state,
            op: &op,
            epoch: op.epoch.expect("claimed"),
        };
        let out = run_evict_pipeline(&ctx, SessionState::Idle, false, false).await;
        assert!(
            matches!(out, Ok(EvictOutcome::QuarantineReaped)),
            "a quarantine op on an unevictable-but-bound session must reap, \
             not skip (the skip is the livelock), got {out:?}",
        );
        assert!(
            !state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the crippled VM must be destroyed — the destroy is what clears \
             the host's quarantine entry and ends the 5s advertise loop",
        );
        assert_eq!(
            state
                .services
                .meta
                .get_session(session_id)
                .await
                .unwrap()
                .status,
            SessionState::HostLost,
            "the session settles HostLost so the straggler sweep re-drives \
             it to Idle/recoverable (resume rewinds to the last checkpoint)",
        );
    }

    /// The settled arm of the same fix: a quarantined survivor bound to a
    /// session whose recoverable state is already durable (`Idle`) is pure
    /// garbage — reap the VM and drop the stale binding (nothing may latch
    /// a destroyed sandbox, e.g. the resume crash-shortcut), but do NOT
    /// touch the status: Idle already means "resume from the durable
    /// capture".
    #[tokio::test]
    async fn quarantine_evict_on_settled_idle_reaps_and_unbinds_without_status_change() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.status = SessionState::Idle;
        let (state, _meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = claim_quarantine_evict_op(&state, session_id).await;
        let ctx = OpCtx {
            state: &state,
            op: &op,
            epoch: op.epoch.expect("claimed"),
        };
        let out = run_evict_pipeline(&ctx, SessionState::Idle, false, false).await;
        assert!(
            matches!(out, Ok(EvictOutcome::QuarantineReaped)),
            "got {out:?}"
        );
        assert!(
            !state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the crippled VM is reaped",
        );
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Idle,
            "Idle already implies a durable capture — no status change",
        );
        assert_eq!(
            after.sandbox_id, None,
            "the stale binding is dropped so no later op latches a \
             destroyed sandbox",
        );
    }

    /// Mid-relocation lanes stay owned by their machinery: a quarantine op
    /// finding the session `Evacuating` must NOT destroy the VM under the
    /// evac capture — it skips, and the evac path's own settle (fallback
    /// to Idle) converges.
    #[tokio::test]
    async fn quarantine_evict_on_evacuating_leaves_the_vm_to_the_evac_machinery() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.status = SessionState::Evacuating;
        let (state, _meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = claim_quarantine_evict_op(&state, session_id).await;
        let ctx = OpCtx {
            state: &state,
            op: &op,
            epoch: op.epoch.expect("claimed"),
        };
        let out = run_evict_pipeline(&ctx, SessionState::Idle, false, false).await;
        assert!(
            matches!(out, Ok(EvictOutcome::Skipped { .. })),
            "got {out:?}"
        );
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the VM must survive — the evac capture may be mid-flight",
        );
        assert_eq!(
            state
                .services
                .meta
                .get_session(session_id)
                .await
                .unwrap()
                .status,
            SessionState::Evacuating,
        );
    }

    /// The NON-quarantine skip is unchanged: an ordinary evict landing on
    /// a non-evictable state still no-ops without touching the VM (a
    /// concurrent op owns the session; destroying here would be the
    /// teardown-reconcile bug class).
    #[tokio::test]
    async fn plain_evict_on_created_still_skips_without_destroying() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.status = SessionState::Created;
        let (state, _meta) = build_state_and_meta(session, sandbox_root.path());
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = claim_evict_op(&state, session_id).await;
        let ctx = OpCtx {
            state: &state,
            op: &op,
            epoch: op.epoch.expect("claimed"),
        };
        let out = run_evict_pipeline(&ctx, SessionState::Idle, false, true).await;
        assert!(
            matches!(out, Ok(EvictOutcome::Skipped { .. })),
            "got {out:?}"
        );
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "a plain skip must never destroy — only the quarantine flavor \
             carries the reap authority",
        );
        assert_eq!(
            state
                .services
                .meta
                .get_session(session_id)
                .await
                .unwrap()
                .status,
            SessionState::Created,
        );
    }

    /// #792: a TRANSIENT BlobStorage-HEAD blip records `recoverable = false`
    /// on a manifest-bearing capture whose artifacts are actually durable.
    /// The recoverable-before-Idle guard must REFUSE to land Idle on that lie
    /// (retryable error; session stays Evicting; the VM stays alive), and once
    /// the blip clears on the redrive the fresh capture verifies recoverable
    /// and the session lands Idle. Proves "fails once → retries → Idle
    /// recoverable".
    #[tokio::test]
    async fn recoverable_guard_transient_head_blip_retries_then_lands_idle() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let manifest = ManifestRef::new();
        // HEAD faults on the FIRST manifests/ lookup (attempt 1), then passes
        // through to the seeded blob (attempt 2).
        let plan = FaultPlan::new().with_head(HeadFault {
            key: KeyMatch::Prefix("manifests/".into()),
            when: When::Nth(1),
            kind: HeadFaultKind::Error(InjectedError::Sdk("transient blob HEAD blip".into())),
        });
        let blob =
            faulty_blob_with_manifest(sandbox_root.path().join("blobs"), &manifest, plan).await;
        let backend: Arc<dyn SandboxBackend> = Arc::new(ManifestSnapshotBackend {
            inner: Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes"))),
            manifest,
        });
        let (state, meta) = build_state_and_meta_with_backend_and_blob(
            evicting_session(session_id),
            sandbox_root.path(),
            backend,
            Some(blob),
        );
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = claim_evict_op(&state, session_id).await;
        let epoch = op.epoch.expect("claimed");
        let ctx = OpCtx {
            state: &state,
            op: &op,
            epoch,
        };

        // Attempt 1: HEAD blips → recoverable=false → the guard refuses Idle.
        let first = run_evict_pipeline(&ctx, SessionState::Idle, false, true).await;
        assert!(
            matches!(first, Err(EvictError::Meta(ref m)) if m.contains("recoverable=false")),
            "the guard must return a retryable error on the transient blip, got {first:?}",
        );
        assert_eq!(
            state
                .services
                .meta
                .get_session(session_id)
                .await
                .unwrap()
                .status,
            SessionState::Evicting,
            "the session must stay Evicting for the retry — never Idle on the lie",
        );
        assert!(
            state
                .services
                .host
                .list()
                .await
                .unwrap()
                .contains(&sandbox_id),
            "the live VM must NOT be destroyed while retrying verification",
        );
        assert!(
            meta.snapshots.lock().iter().any(|s| !s.recoverable),
            "the recoverable=false row IS recorded (it feeds the dead-host bad-capture signal)",
        );

        // Attempt 2 (the op redrive): the blip has cleared; HEAD now succeeds.
        let second = run_evict_pipeline(&ctx, SessionState::Idle, false, true).await;
        assert!(
            matches!(second, Ok(EvictOutcome::Evacuated)),
            "the retry lands the eviction once the capture verifies recoverable, got {second:?}",
        );
        assert_eq!(
            state
                .services
                .meta
                .get_session(session_id)
                .await
                .unwrap()
                .status,
            SessionState::Idle,
            "the session lands Idle only once a recoverable capture backs it",
        );
        assert!(
            meta.snapshots.lock().iter().any(|s| s.recoverable),
            "a recoverable=true row now exists — a resume can honor it",
        );
    }

    /// #792: a PERSISTENTLY unrecoverable capture (HEAD fails every attempt)
    /// must never settle Idle-but-unresumable. The guard returns a retryable
    /// error each attempt; once the evict op budget is exhausted the verb
    /// falls the session back to HostLost — the existing honest terminal
    /// route (the dead-host straggler sweep then settles it Dead WITH the
    /// unrecoverable-snapshot signal against the recorded recoverable=false
    /// row). Asserts: HostLost, never Idle.
    #[tokio::test]
    async fn recoverable_guard_persistent_failure_exhausts_budget_to_host_lost() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let manifest = ManifestRef::new();
        let plan = FaultPlan::new().with_head(HeadFault {
            key: KeyMatch::Prefix("manifests/".into()),
            when: When::Always,
            kind: HeadFaultKind::Error(InjectedError::Sdk("persistent blob HEAD failure".into())),
        });
        let blob =
            faulty_blob_with_manifest(sandbox_root.path().join("blobs"), &manifest, plan).await;
        let backend: Arc<dyn SandboxBackend> = Arc::new(ManifestSnapshotBackend {
            inner: Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes"))),
            manifest,
        });
        let (state, meta) = build_state_and_meta_with_backend_and_blob(
            evicting_session(session_id),
            sandbox_root.path(),
            backend,
            Some(blob),
        );
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // Age the attempt count past EVICT_MAX_ATTEMPTS so this claim is the
        // budget-exhausting one (mirrors evict_op_budget_exhaustion_*).
        let mut op = claim_evict_op(&state, session_id).await;
        op.attempts = 21;
        let epoch = op.epoch.expect("claimed");
        let ctx = OpCtx {
            state: &state,
            op: &op,
            epoch,
        };
        let outcome = crate::session_verbs::dispatch(&ctx).await;
        assert!(
            matches!(outcome, crate::session_ops::OpOutcome::Failed(_)),
            "budget exhaustion on a persistently-unrecoverable capture must be terminal (Failed)",
        );

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::HostLost,
            "the honest terminal route is HostLost (→ dead-host sweep → Dead), never a lying Idle",
        );
        assert_ne!(after.status, SessionState::Idle, "never Idle-unrecoverable");
        assert!(
            meta.snapshots.lock().iter().all(|s| !s.recoverable),
            "only recoverable=false rows were recorded — the sweep's unrecoverable signal fires on them",
        );
    }

    /// #792 scope: the recoverable-before-Idle guard is deliberately scoped
    /// to MANIFEST-BEARING captures (the transient-HEAD-blip class). A
    /// manifest-LESS capture (ProcessBackend, VZ, the pre-#791 sim gap) still
    /// lands Idle even when every HEAD would fail — #791 closed the
    /// manifest-less case at the sim-fidelity layer, and gating it here would
    /// wedge the dev backends that never emit chunked manifests.
    #[tokio::test]
    async fn recoverable_guard_does_not_fire_for_a_manifestless_capture() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        // HEAD always fails — but ProcessBackend's capture is manifest-less,
        // so verify_snapshot_recoverable HEADs nothing (returns false without
        // a blob call) and `manifests_present` is false → the guard is skipped.
        let plan = FaultPlan::new().with_head(HeadFault {
            key: KeyMatch::Any,
            when: When::Always,
            kind: HeadFaultKind::Error(InjectedError::Sdk("head down".into())),
        });
        let inner: Arc<dyn BlobStorage> =
            Arc::new(LocalBlobStorage::new(sandbox_root.path().join("blobs")));
        let blob: Arc<dyn BlobStorage> = Arc::new(FaultyBlobStorage::new(inner, plan));
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let (state, _meta) = build_state_and_meta_with_backend_and_blob(
            evicting_session(session_id),
            sandbox_root.path(),
            backend,
            Some(blob),
        );
        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let op = drive_evict(&state, session_id, false, true).await;
        assert_eq!(
            op.state,
            OpState::Done,
            "a manifest-less capture completes the evict op unchanged",
        );
        assert_eq!(
            state
                .services
                .meta
                .get_session(session_id)
                .await
                .unwrap()
                .status,
            SessionState::Idle,
            "a manifest-less capture still lands Idle — the guard is scoped to manifest-bearing captures",
        );
    }
}
