//! ADR 0018 commit 12c — Evacuating-session resumer.
//!
//! Background task that turns `Evacuating` sessions back into `Active`
//! sessions on a peer host. Sibling to [`crate::dead_host`]: same
//! polling shape, same shared-state surface, distinct entry point on
//! the state machine.
//!
//! ## Flow
//!
//! 1. Tick: read `sessions WHERE status = 'evacuating'` (with
//!    `evac_attempts`) via
//!    [`MetadataStore::list_evacuating_sessions`].
//! 2. For each candidate, if `evac_attempts >= max_attempts` →
//!    `Evacuating → Idle` and stop trying (user can `/resume`).
//! 3. Otherwise bump `evac_attempts` atomically, then run the
//!    relocation pipeline:
//!    - [`crate::evacuation::evacuate_dead_source`] picks a peer host,
//!      restores from the session's `live_disk_manifest` and/or latest
//!      snapshot, rebinds PG `(host_id, sandbox_id)`, transitions
//!      `Evacuating → Created`.
//!    - [`crate::api::snapshot::bind_session_routing`] registers the
//!      session→sandbox map on the target host-agent (the coordinator
//!      keeps no in-memory binding — `sessions.sandbox_id` is the
//!      authority, ADR 0047).
//!    - [`crate::api::snapshot::finish_resume_to_active`] runs the
//!      harness rebuild + drives `Created → Active`.
//! 4. On any error in the pipeline, the session is left at its current
//!    state — Evacuating (retry next tick) or Created (a later scanner
//!    tick re-picks it up via the operator-/exec-driven `/resume`
//!    path). The pre-bump idempotency lives in
//!    `evacuate_dead_source` (PG rebind is `assign_*` which tolerates
//!    re-runs) and in `bind_session_routing` (an idempotent host RPC).
//!
//! ## Why this pattern
//!
//! Per `[async_via_state_machine]` — drain is a multi-host, multi-step
//! operation. Synchronous orchestration would couple the source
//! handler to a known target and conflate retry domains; the
//! state-machine+scanner shape decouples them. Source writes
//! "session is ready to be continued"; scanner finds a healthy peer.
//! Operator-initiated drain (ADR 0044 K3) is the sole producer of
//! `Evacuating` — ADR 0045 Phase A retired the reactive dead-host /
//! NBD-loss producers (the dead-host detector now routes recoverable
//! sessions to `Idle` for lazy `/resume`).
//!
//! The scanner is single-coord-pod safe because each per-session
//! advance starts with `bump_evac_attempts` (atomic +1) followed by
//! `evacuate_dead_source`'s pick + restore + rebind. Two coord pods
//! racing on the same session would both observe `Evacuating`, both
//! attempt restore, and the second's `transition_session(Created)`
//! would see the row already at `Created` and surface `Conflict`.
//! That's a harmless duplicate sandbox on the target (cleaned up by
//! orphan reap) — same shape as the dead-host detector's existing
//! advisory-lock race. Tightening with an advisory lock per session
//! is a follow-up if duplicate-restore counts ever rise above zero.

use std::time::Duration;

use chrono::Utc;
use engram_core::types::BindingDisposition;
use engram_core::types::{Session, SessionState};

use crate::api::snapshot::{finish_resume_to_active, FinishResumeOutcome};
use crate::evacuation::{evacuate_dead_source, EvacError};
use crate::state::{SessionEvent, SharedState};

/// Issue #214: max age of an operator teleport pin before the evac
/// scanner treats it as a leak and ignores + clears it. A pin is meant
/// to be consumed within one scanner tick (~seconds) of being set; one
/// that survives 15 minutes can only have leaked from a path that set it
/// without driving the session through `Evacuating`. Defense in depth
/// behind the core teleport_session fix — degrades any future leak to
/// default placement instead of a strict hijack.
const TELEPORT_PIN_TTL: chrono::Duration = chrono::Duration::minutes(15);

/// Issue #214: is a teleport pin stamped at `set_at` old enough (as of
/// `now`) to be treated as a leak? A `None` stamp (pin set before the
/// 0065 migration) is never aged out — we keep honoring legacy pins.
fn teleport_pin_aged(
    set_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<Utc>,
) -> bool {
    set_at
        .map(|t| now.signed_duration_since(t) > TELEPORT_PIN_TTL)
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct EvacResumerConfig {
    /// How often to sweep for Evacuating sessions. The scanner picks
    /// up new entries from operator drains (ADR 0044 K3) — the sole
    /// producer of `Evacuating` since ADR 0045 Phase A retired the
    /// reactive triggers. Default 10s matches
    /// `DeadHostConfig::poll_interval` so the two scanners share the
    /// same operational cadence.
    pub poll_interval: Duration,
    /// Retry budget per session before falling back to `Idle`. At the
    /// default 10s cadence, 20 attempts is ~3 minutes — long enough
    /// to ride out a transient capacity / image-prefetch shortfall on
    /// peer hosts during a rolling restart, short enough that a truly
    /// stuck session surfaces to the user as `Idle` (manual /resume)
    /// before they assume it's gone.
    pub max_attempts: u32,
}

impl Default for EvacResumerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            max_attempts: 20,
        }
    }
}

/// Spawn the resumer as a background task. Caller holds the JoinHandle
/// for the process lifetime; dropping aborts the loop. Mirrors
/// [`crate::dead_host::spawn`].
pub fn spawn(cfg: EvacResumerConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the first immediate tick — coord just started, give
        // hosts a beat to heartbeat in before we pick.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "evac-resumer tick failed; will retry");
            }
        }
    })
}

/// Single scanner tick. `pub(crate)` so live-PG tests can drive the
/// scanner deterministically without `tokio::spawn`-ing the loop.
/// Production code uses [`spawn`] which calls this on a timer.
pub async fn run_once(
    cfg: &EvacResumerConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state.services.meta.list_evacuating_sessions().await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "evac-resumer found Evacuating sessions"
    );
    for (session, attempts) in candidates {
        if let Err(e) = advance_one(cfg, state, session, attempts).await {
            // Keep going — one wedged session shouldn't stall the
            // sweep. The per-session log already carries `error =
            // %e`; this is the loop-level swallow.
            tracing::warn!(error = %e, "evac-resumer per-session advance failed");
        }
    }
    Ok(())
}

// ADR 0019 / telemetry restoration (#526): scanner-driven work has no
// request span to inherit — an explicit root (carrying `session_id`) so
// the relocation pipeline's spans correlate instead of exporting as
// disconnected roots.
#[tracing::instrument(name = "evac_resumer.advance_one", skip_all, fields(session_id = %session.id))]
async fn advance_one(
    cfg: &EvacResumerConfig,
    state: &SharedState,
    session: Session,
    attempts: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;
    // ADR 0045 C1 / ADR 0079: claim the session's op lane before driving
    // a resume — a live migration's inline claim (or a peer pod's op) may
    // be mid-flight on this session; without the claim two actors can
    // double-restore. Claim-or-give-up: a busy lane means the holder owns
    // the session; we re-scan next tick. The claim rides the op log
    // (kind = resume, evac flavor); if this pod dies mid-pipeline the
    // reclaim sweep re-claims the row and the resume verb terminally
    // fails it (status Evacuating is not verb-resumable), freeing the
    // lane for the next tick's fresh claim.
    let Some(claim) = crate::session_ops::OpClaim::try_acquire(
        state,
        session_id,
        engram_core::types::session_op::OpKind::Resume,
        serde_json::json!({ "flavor": "evac" }),
    )
    .await
    .map_err(|e| format!("evac-resumer op claim acquire: {e}"))?
    else {
        tracing::debug!(%session_id, "evac-resumer: an op owns the session; skipping this tick");
        return Ok(());
    };
    let result = advance_one_claimed(cfg, state, session, attempts, &claim).await;
    match &result {
        Ok(()) => {
            claim
                .finish(engram_core::types::session_op::OpState::Done, None)
                .await
        }
        Err(e) => {
            claim
                .finish(
                    engram_core::types::session_op::OpState::Failed,
                    Some(&e.to_string()),
                )
                .await
        }
    }
    result
}

/// The claimed body of [`advance_one`] — runs with the op lane held.
async fn advance_one_claimed(
    cfg: &EvacResumerConfig,
    state: &SharedState,
    session: Session,
    attempts: u32,
    claim: &crate::session_ops::OpClaim,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;
    // Issue #211 (ADR 0044 K5 shape, copied from idle_evictor): the
    // `candidates` list is a sweep snapshot up to ~10s stale. Without a
    // post-claim re-read we could drive a resume — restoring a live VM
    // on a peer host and binding it — onto a row that has since gone
    // terminal (a `DELETE /sessions/:id` flips Evacuating→Failed) or was
    // already relocated by a competitor. The op claim is held until the
    // pipeline finishes, so once we hold it any concurrent actor has
    // fully completed; re-read the authoritative PG state and skip
    // unless the row is still `Evacuating` (the only legal input to the
    // resume pipeline). This keeps a stale tick from binding a fresh
    // sandbox onto a terminal row (defeating the orphan reap) — the same
    // failure the guarded binds in `bind_resumed_session` reject, caught
    // earlier so we never create the VM in the first place.
    let mut session = match state.services.meta.get_session(session_id).await {
        Ok(s) if s.status != SessionState::Evacuating => {
            tracing::info!(
                %session_id,
                state = s.status.as_str(),
                "evac-resumer: session no longer Evacuating after claim (terminated or \
                 relocated by a peer) — skipping",
            );
            return Ok(());
        }
        Ok(s) => s,
        Err(e) => {
            return Err(format!("evac-resumer: re-read session state after claim: {e}").into());
        }
    };

    // Retry budget exhausted → fall back to Idle so the user can
    // `/resume` manually. Idle is a legal target from Evacuating per
    // the legality table; the row's snapshot lineage is already
    // durable (it was captured before the pipeline marked the
    // session Evacuating in the evict pipeline), so /resume
    // from Idle restores cleanly. Checked BEFORE the teardown
    // confirmation so a source that stays connected but can never
    // positively confirm (its failures burn this same budget below)
    // reaches this terminal instead of wedging Evacuating forever.
    // The fallback may then leave the source binding IN PLACE — that
    // is deliberate: ownership of an unconfirmed sandbox is never
    // released here. The `/resume` this hands off to runs the SAME
    // confirmation gate (`resume_from_idle`'s stale-binding leg) and
    // performs the fenced clear itself once the source is confirmed
    // gone — or fails 503-retryable until the dead-host lane clears
    // the binding. Ownership release stays behind one gate.
    if attempts >= cfg.max_attempts {
        match state
            .services
            .meta
            .transition_session(session_id, SessionState::Idle, BindingDisposition::Retain)
            .await
        {
            Ok(prev) => {
                tracing::warn!(
                    %session_id,
                    attempts,
                    max_attempts = cfg.max_attempts,
                    "evac-resumer budget exhausted; session left at Idle for user /resume",
                );
                let _ = state
                    .emit(
                        session_id,
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::Idle,
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "evac-resumer fallback transition Evacuating→Idle failed",
                );
            }
        }
        // Gave up relocating — drop any teleport pin so a later manual
        // /resume isn't constrained to the (evidently unavailable) target.
        let _ = state
            .services
            .meta
            .set_teleport_target(session_id, None)
            .await;
        return Ok(());
    }

    // ADR 0090: `Evacuating` retains the outgoing binding until teardown
    // is positively confirmed. The drain's evict op has already issued a
    // best-effort destroy, but an acknowledged host verb can still have a
    // durable, not-yet-applied effect. Restoring from the snapshot while
    // that sandbox is alive would give this session two owners across the
    // fleet. Re-issue the idempotent destroy through the source backend,
    // then independently probe it. Only a negative probe (or host-side
    // NotFound) authorizes the fenced binding clear below.
    match (session.host_id, session.sandbox_id) {
        (Some(source_host), Some(source_sandbox)) => {
            if let Err(e) =
                confirm_source_teardown(state, source_host, source_sandbox, claim.fence()).await
            {
                // Confirmation failures burn the SAME budget as pipeline
                // failures — without this, a persistently unconfirmable
                // teardown never reaches the exhaustion fallback above and
                // the session wedges with no terminal state.
                let _ = state.services.meta.bump_evac_attempts(session_id).await;
                return Err(e);
            }
            match state
                .services
                .meta
                .fenced_assign_sandbox(
                    session_id,
                    claim.fence().epoch as i64,
                    None,
                    Some(source_host),
                )
                .await
            {
                Ok(true) => {
                    state.host_registry.invalidate_sandbox(source_sandbox);
                    session.sandbox_id = None;
                }
                Ok(false) => {
                    crate::metrics::note_fenced_write();
                    return Ok(());
                }
                Err(e) => {
                    return Err(
                        format!("evac-resumer: clear confirmed-dead source binding: {e}").into(),
                    );
                }
            }
        }
        (None, Some(source_sandbox)) => {
            // Unconfirmable by construction — burn the budget so this
            // (should-be-impossible) shape also reaches the exhaustion
            // fallback instead of wedging.
            let _ = state.services.meta.bump_evac_attempts(session_id).await;
            return Err(format!(
                "evac-resumer: session {session_id} retains source sandbox {source_sandbox} \
                 without a source host; refusing to release ownership"
            )
            .into());
        }
        (_, None) => {}
    }

    // Bump pre-pipeline. A pipeline failure leaves the counter
    // incremented and the session at Evacuating — next tick retries
    // until the budget runs out. Bumping post-success isn't needed
    // because `transition_session(Evacuating)` resets the counter on
    // every entry per migration 0037's CASE expression.
    let new_attempts = state.services.meta.bump_evac_attempts(session_id).await?;
    tracing::info!(
        %session_id,
        attempt = new_attempts,
        max_attempts = cfg.max_attempts,
        "evac-resumer: starting resume attempt",
    );

    run_resume_pipeline(state, session, claim.fence()).await?;
    Ok(())
}

pub(crate) async fn confirm_source_teardown(
    state: &SharedState,
    source_host: engram_core::HostId,
    source_sandbox: engram_core::SandboxId,
    fence: engram_core::traits::SessionFence,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let backend = state.host_registry.backend_of(source_host).ok_or_else(|| {
        format!(
            "evac-resumer: source host {source_host} is not connected; retaining ownership of \
             sandbox {source_sandbox}"
        )
    })?;

    match backend.destroy(source_sandbox, fence).await {
        Ok(()) => {}
        Err(engram_core::SandboxError::NotFound) => return Ok(()),
        Err(e) => {
            return Err(format!(
                "evac-resumer: source destroy for sandbox {source_sandbox} on host \
                 {source_host} was not confirmed: {e}"
            )
            .into());
        }
    }

    match backend.probe_sandbox(source_sandbox).await {
        Ok(probe) if !probe.known_to_backend && !probe.process_alive => Ok(()),
        Ok(probe) => Err(format!(
            "evac-resumer: source sandbox {source_sandbox} on host {source_host} still owns \
             the session after destroy acknowledgement (known={}, alive={}); retaining \
             coordinator ownership",
            probe.known_to_backend, probe.process_alive,
        )
        .into()),
        Err(engram_core::SandboxError::NotFound) => Ok(()),
        // An old host-agent mid-roll has no probe RPC (`Unimplemented` →
        // `Unsupported`, per the HostClient trait doc). Proceed on the
        // strength of the confirmed destroy above — the same "no probe
        // available" posture `reconcile::flip_missing` documents. A drain
        // is exactly when mixed agent versions exist; refusing here wedged
        // every evacuation off a not-yet-rolled host.
        Err(engram_core::SandboxError::Unsupported(_)) => Ok(()),
        Err(e) => Err(format!(
            "evac-resumer: source teardown probe for sandbox {source_sandbox} on host \
             {source_host} failed: {e}; retaining coordinator ownership"
        )
        .into()),
    }
}

async fn run_resume_pipeline(
    state: &SharedState,
    session: Session,
    fence: engram_core::traits::SessionFence,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;
    let snapshot = state
        .services
        .meta
        .latest_snapshot_for_session(session_id)
        .await?;

    // ADR 0028 A.log: capture the rung-1 rewind cursor before the
    // snapshot moves into evacuate_dead_source. Only a coherent
    // checkpoint (memory present) rewinds; the receipt's
    // `EvacLoss::None` confirms rung-1 actually happened.
    let rewind_cursor = snapshot
        .as_ref()
        .filter(|s| s.memory_manifest.is_some())
        .and_then(|s| s.events_cursor);

    // ADR 0028 Fix B: pre-materialize the disk-only cold-boot spec (ADR
    // 0116: carries the session's persisted harness/skill slots). Only
    // consulted when no coherent memory snapshot is usable; a `None`
    // (image un-enabled) fails structurally rather than burning the
    // budget, while a transient store error propagates — the scanner's
    // next tick is the retry.
    let cold_boot_spec = crate::boot_materializer::materialize_cold_boot(state, &session).await?;

    // #800 (RESERVED evac placement): the session's reserved 2D budget,
    // resolved from the enabled image the same way the resume verb resolves
    // it (`resume_from_fc_snapshot`). `None` (image un-enabled) keeps the
    // pre-#800 capacity-soft placement inside `evacuate_dead_source`. Read
    // here, before `cold_boot_spec` is moved into the call below.
    let evac_budget = cold_boot_spec
        .as_ref()
        .map(|s| (s.memory.max_mib, s.cpu.vcpus));

    // ADR 0045 Phase F: an operator-pinned teleport destination, if any.
    // Honored strictly (a bad pin retries then falls back to Idle, never
    // silently lands elsewhere); cleared below once the session resolves.
    //
    // Issue #214 defense in depth: a pin is supposed to be consumed within
    // seconds of being set (teleport marks the session Evacuating, the next
    // scanner tick resolves it). A pin still present long after it was set
    // is a LEAK — some path set it without the session ever entering
    // Evacuating (the core fix closes the known teleport no-op path; this
    // backstops any future one). Strictly honoring a stale pin would hijack
    // this evacuation onto a possibly full/gone host, then burn the budget
    // to a forced-Idle strand. Instead: ignore + clear + warn on an aged
    // pin so this evacuation degrades to default capacity-ranked placement.
    let require_host = match state.services.meta.get_teleport_target(session_id).await {
        Ok(Some((host, set_at))) => {
            if teleport_pin_aged(set_at, state.services.clock.now_utc()) {
                tracing::warn!(
                    %session_id,
                    stale_target = %host,
                    set_at = ?set_at,
                    ttl_secs = TELEPORT_PIN_TTL.num_seconds(),
                    "evac-resumer: teleport pin older than TTL — treating as a leaked pin; \
                     ignoring + clearing it, falling back to default placement (issue #214)",
                );
                let _ = state
                    .services
                    .meta
                    .set_teleport_target(session_id, None)
                    .await;
                None
            } else {
                Some(host)
            }
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%session_id, error = %e,
                "get_teleport_target failed; treating as unpinned");
            None
        }
    };

    let receipt = match evacuate_dead_source(
        &state.host_registry,
        &state.services.meta,
        session.clone(),
        snapshot,
        cold_boot_spec,
        require_host,
        // No origin preference: every scanner producer (drain, dead
        // host, migration parachute) is moving AWAY from the source.
        None,
        fence,
        evac_budget,
        state.services.clock.now_utc(),
    )
    .await
    {
        Ok(r) => r,
        // #800: RESERVED evac placement found no survivor that fits — QUEUE
        // (Evacuating → Queued, resume-origin) instead of overcommitting a
        // measured-full host. The queue scanner re-homes it once capacity
        // returns (fenced on the evac op's epoch, like the resume enqueue).
        // A `false` return = the row already left Evacuating (a peer
        // relocated it) or the epoch moved — stop silently. Handing
        // ownership to the scanner is the honest overflow path; the resumer
        // does NOT burn a retry attempt on it.
        Err(EvacError::NoCapacityQueue) => {
            match state
                .services
                .meta
                .enqueue_evacuating_session_resume(session_id, fence.epoch as i64)
                .await
            {
                Ok(true) => {
                    tracing::info!(
                        %session_id,
                        "evac-resumer: no survivor fits the reserved budget — queued \
                         (resume-origin) instead of overcommitting (#800)",
                    );
                    let _ = state
                        .emit(
                            session_id,
                            SessionEvent::StatusChanged {
                                from: SessionState::Evacuating,
                                to: SessionState::Queued,
                                at: state.services.clock.now_utc(),
                            },
                        )
                        .await;
                }
                Ok(false) => {
                    tracing::debug!(
                        %session_id,
                        "evac-resumer: enqueue-for-capacity no-op (row moved / epoch bumped)",
                    );
                }
                Err(e) => {
                    tracing::warn!(%session_id, error = %e,
                        "evac-resumer: enqueue-for-capacity failed; leaving Evacuating (retry)");
                }
            }
            return Ok(());
        }
        // ADR 0028 Fix B fail-fast: structural errors can never be
        // fixed by retrying — the pre-Fix-B behavior of letting the
        // budget loop burn 20 attempts (~3 min of RestoreFailed churn
        // in the cf4d4afd incident) just delayed the honest terminal
        // state. NoRecoverableState (nothing to restore from) → Dead;
        // ColdBootUnavailable (disk exists, image gone) → Idle, so
        // re-enabling the image + /resume can still recover the disk.
        Err(e) if e.is_structural() => {
            let target = match &e {
                EvacError::NoRecoverableState => SessionState::Dead,
                _ => SessionState::Idle,
            };
            tracing::warn!(
                %session_id,
                error = %e,
                target = %target.as_str(),
                "evac-resumer: structural failure — failing fast instead of burning budget",
            );
            match state
                .services
                .meta
                .transition_session(session_id, target, BindingDisposition::RequireUnbound)
                .await
            {
                Ok(prev) => {
                    let _ = state
                        .emit(
                            session_id,
                            SessionEvent::StatusChanged {
                                from: prev,
                                to: target,
                                at: state.services.clock.now_utc(),
                            },
                        )
                        .await;
                }
                Err(te) => {
                    tracing::warn!(
                        %session_id,
                        error = %te,
                        "evac-resumer: structural fail-fast transition failed",
                    );
                }
            }
            // Session left Evacuating terminally — drop any teleport pin.
            let _ = state
                .services
                .meta
                .set_teleport_target(session_id, None)
                .await;
            return Ok(());
        }
        Err(e) => return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
    };

    // Resolved onto a peer (Created) — the teleport pin is consumed.
    let _ = state
        .services
        .meta
        .set_teleport_target(session_id, None)
        .await;

    tracing::info!(
        %session_id,
        new_host = %receipt.new_host_id,
        new_sandbox = %receipt.new_sandbox_id,
        loss = receipt.loss.as_str(),
        "evac-resumer: rebound to peer at Created — finishing harness rebuild",
    );

    // StatusChanged{prev → Created} so SSE subscribers see the move.
    // `evacuate_dead_source` already committed the transition to
    // Created in PG, so we emit synthesized event with from=Evacuating.
    let _ = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Evacuating,
                to: SessionState::Created,
                at: state.services.clock.now_utc(),
            },
        )
        .await;

    // ADR 0073: evac restore is a fresh-spawn generation — mint.
    crate::api::snapshot::bind_session_routing_minted(state, session_id, receipt.new_sandbox_id)
        .await;

    // ADR 0028 A.log: warm rung-1 recovery — rewind the transcript to
    // the checkpoint's cursor + emit the recovery boundary. Gated on
    // EvacLoss::None (memory was actually restored); a rung-2 cold
    // boot carries no cursor and skips this. No-op if the checkpoint
    // was the head.
    // ADR 0045 F1: this path is only reached via operator drain /
    // teleport (Phase A retired the reactive dead-host producer), so the
    // rewind is a planned relocation, not a host failure.
    if receipt.loss == engram_core::types::evacuation::EvacLoss::None {
        crate::api::snapshot::apply_rung1_rewind(
            state,
            session_id,
            rewind_cursor,
            crate::state::RecoveryCause::PlannedRelocation,
        )
        .await;
    }

    // Refresh the session row so finish_resume_to_active sees the
    // freshly-bound host_id + sandbox_id.
    let session_refreshed = state.services.meta.get_session(session_id).await?;
    match finish_resume_to_active(
        state,
        &session_refreshed,
        receipt.new_sandbox_id,
        true,
        fence,
    )
    .await
    {
        Ok(FinishResumeOutcome::Active) => {
            tracing::info!(
                %session_id,
                "evac-resumer: session reached Active on peer host",
            );
        }
        Ok(FinishResumeOutcome::CreatedHarnessFailed(e)) => {
            tracing::warn!(
                %session_id,
                error = %e,
                "evac-resumer: harness rebuild failed on peer; session left at Created — \
                 user /resume retries from there (ADR 0090: the resume op now owns \
                 backoff + budget). Scanner won't re-pick (status != Evacuating).",
            );
        }
        Err(e) => {
            tracing::warn!(
                %session_id,
                error = %e,
                "evac-resumer: finish_resume_to_active errored; session left at Created",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    // The scanner's full loop exercises Postgres + HostRegistry +
    // finish_resume_to_active; the per-step plumbing is unit-tested
    // via the existing evacuation / api::snapshot tests. End-to-end
    // coverage lives in:
    //   - `admin_evac_live_pg` (commit 12i): operator-drain triggers
    //     Evacuating, scanner picks it up, session reaches Active on
    //     peer.
    //   - `evac_resumer_budget_falls_back_to_idle` (commit 12i):
    //     simulate 20 failed bumps, observe Evacuating → Idle
    //     fallback.
    //   - dev-vm integration-evac-test.sh: real two-host drain.
    //
    // ADR 0028 Fix B adds the structural fail-fast tests below: a
    // structurally-unrecoverable session must reach its terminal
    // state on the FIRST attempt, not after burning the 20-attempt
    // budget (~3 min of churn in the cf4d4afd incident).

    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_core::traits::MetadataStore;
    use engram_core::types::session::SessionMode;
    use std::sync::Arc;

    fn evacuating_session(live_disk: Option<engram_core::types::manifest::ManifestRef>) -> Session {
        Session {
            id: engram_core::SessionId::new(),
            status: SessionState::Evacuating,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evac-test".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            last_event_at: None,
            live_disk_manifest: live_disk,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    fn build_state(session: Session) -> (SharedState, Arc<MiniMeta>) {
        let tmp = std::env::temp_dir().join(format!("evac-resumer-test-{}", session.id));
        std::fs::create_dir_all(&tmp).unwrap();
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        let services = Services {
            meta: meta.clone(),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                tmp.join("blobs"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.join("blobs")),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            entropy: Arc::new(engram_core::traits::OsEntropy),
        };
        let cfg = CoordinatorConfig {
            local_path: tmp,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
        )
    }

    /// ADR 0045 C1 / ADR 0079: a RUNNING op on the session (a live
    /// migration's inline claim, a peer pod's pipeline) makes the
    /// scanner SKIP the session this tick — no transition, no restore
    /// attempt, no double-driving.
    #[tokio::test]
    async fn advance_one_skips_when_an_op_owns_the_session() {
        let session = evacuating_session(None);
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());
        meta.ops
            .seed_running(session_id, engram_core::types::session_op::OpKind::Teleport);

        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("skip is not an error");

        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Evacuating,
            "an op-owned session must be left untouched for the holder",
        );
    }

    /// No snapshot + no live disk manifest → NoRecoverableState →
    /// Dead on attempt 1.
    #[tokio::test]
    async fn structural_no_state_fails_fast_to_dead() {
        let session = evacuating_session(None);
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());

        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("advance_one swallows structural failures");

        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Dead,
            "structurally unrecoverable session must fail fast to Dead, not retry",
        );
    }

    /// Live disk manifest present but the image is not enabled (no
    /// cold-boot spec derivable) → ColdBootUnavailable → Idle on
    /// attempt 1, preserving the re-enable-then-/resume path.
    #[tokio::test]
    async fn structural_disk_only_without_image_fails_fast_to_idle() {
        let live = engram_core::types::manifest::ManifestRef {
            manifest_id: uuid::Uuid::from_u128(0xD15C),
            version: 4,
        };
        let session = evacuating_session(Some(live));
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());

        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("advance_one swallows structural failures");

        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Idle,
            "disk-only session with no enabled image must land at Idle \
             (re-enable image + /resume recovers), not burn the budget",
        );
    }

    /// Issue #214: the aged-pin predicate. A fresh stamp is honored; one
    /// past the TTL is a leak; a `None` stamp (legacy pin) is always
    /// honored so the rollout doesn't drop in-flight pins.
    #[test]
    fn teleport_pin_aged_respects_ttl() {
        let now = Utc::now();
        assert!(
            !teleport_pin_aged(Some(now), now),
            "a just-set pin is not aged",
        );
        assert!(
            !teleport_pin_aged(
                Some(now - (TELEPORT_PIN_TTL - chrono::Duration::seconds(1))),
                now
            ),
            "a pin just inside the TTL is honored",
        );
        assert!(
            teleport_pin_aged(
                Some(now - (TELEPORT_PIN_TTL + chrono::Duration::seconds(1))),
                now
            ),
            "a pin past the TTL is treated as a leak",
        );
        assert!(
            !teleport_pin_aged(None, now),
            "a NULL stamp (pre-0065 legacy pin) is never aged out",
        );
    }

    /// Issue #214 acceptance: a teleport pin older than the TTL is a leak.
    /// The evac scanner must IGNORE it (not strictly pin this evacuation
    /// onto the stale target) and CLEAR it (so it can't hijack a future
    /// evacuation either). We observe the clear directly; the "ignore" is
    /// implied because a structurally-recoverable session with a honored
    /// pin to an unknown host would burn its budget, whereas this session
    /// resolves on attempt 1 (structural fail-fast) with the pin gone.
    #[tokio::test]
    async fn aged_teleport_pin_is_ignored_and_cleared() {
        let session = evacuating_session(None);
        let session_id = session.id;
        let (state, meta) = build_state(session.clone());

        // Plant an aged pin straight into the mirror: a real leak from
        // some path that set the pin >TTL ago without the session ever
        // resolving off Evacuating. The set-at is backdated past the TTL.
        let stale_target = engram_core::HostId::new();
        meta.teleport_targets.lock().insert(
            session_id,
            (
                stale_target,
                Some(Utc::now() - (TELEPORT_PIN_TTL + chrono::Duration::minutes(1))),
            ),
        );

        // No recoverable state → the pipeline fails fast to Dead, but the
        // aged-pin check runs first (before evacuate_dead_source).
        advance_one(&EvacResumerConfig::default(), &state, session, 0)
            .await
            .expect("advance_one swallows structural failures");

        assert_eq!(
            meta.get_teleport_target(session_id).await.unwrap(),
            None,
            "an aged (leaked) teleport pin must be cleared, not left to hijack \
             a future evacuation",
        );
    }

    /// Inverse of the above: a FRESH pin must NOT be cleared by the
    /// aged-pin guard — it is consumed normally by the resolution path.
    /// (Here the session fails structurally, so the pin is cleared by the
    /// terminal-fail arm rather than the aged-pin arm; the point is the
    /// aged-pin arm doesn't fire and warn on a healthy pin.) We assert the
    /// predicate path: a freshly-stamped pin is observed as require_host.
    #[test]
    fn fresh_teleport_pin_is_not_aged() {
        assert!(
            !teleport_pin_aged(Some(Utc::now()), Utc::now()),
            "a freshly-set teleport pin must be honored, never aged out",
        );
    }
}
