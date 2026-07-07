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

use chrono::Utc;
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
    // ADR 0079 (review finding #6): FENCED — a fenced-out predecessor must
    // not stamp park_rung=2 over a successor's fresh state. Ok(false) =
    // fenced; surface it as the `fenced:` Conflict the caller already maps
    // to `EvictOutcome::Fenced`.
    if !state
        .services
        .meta
        .fenced_set_session_park_rung(session_id, ctx.epoch, 2, Some(Utc::now()))
        .await?
    {
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
                    at: Utc::now(),
                },
            )
            .await;
    }
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
    if !entry_legal {
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
                                at: Utc::now(),
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
                        let _ = state
                            .services
                            .meta
                            .fenced_set_session_park_rung(session_id, ctx.epoch, 0, None)
                            .await;
                    }
                },
                Err(engram_core::SandboxError::InvalidSpec(_)) => {
                    // Backend can't pause (VZ/Process) — fall through to
                    // the full eviction below.
                }
                Err(e) => {
                    tracing::warn!(session_id = %session_id, error = %e,
                        "rung-2 park: pause failed; falling through to full eviction");
                }
            }
        }
        // ADR 0065: reap the ephemeral in-guest browser stack before the eviction
        // snapshot so a live Chrome is never frozen into it (re-lazy-started on
        // the next EnsureBrowser after resume). Best-effort; never blocks eviction.
        let _ = state.services.host.stop_browser(sandbox_id).await;
        match state
            .services
            .host
            .snapshot_begin(sandbox_id, ctx.fence())
            .await
        {
            Ok(snapshot_id) => {
                return finish_eviction_d5(ctx, session_id, sandbox_id, snapshot_id, &session)
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

    let metadata = state
        .services
        .host
        .snapshot(sandbox_id, ctx.fence())
        .await
        .map_err(EvictError::Sandbox)?;

    let host_id = state.host_registry.host_of(sandbox_id);
    let now = Utc::now();
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
        Ok(false) => return Ok(EvictOutcome::Fenced),
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
        let _ = state
            .services
            .meta
            .fenced_set_session_park_rung(session_id, ctx.epoch, 0, None)
            .await;
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

/// ADR 0045 D5 (rewritten for issue #529, then ADR 0079): the fast-path
/// tail of an idle eviction. The capture has landed (`snapshot_begin`
/// returned) and the finalize is a HOST-OWNED job: the host durably
/// persisted its inputs before `snapshot_begin` returned, and it lands
/// the snapshot row itself via the heartbeat reconcile
/// (`api/host_http.rs::heartbeat`) — surviving this coordinator dying,
/// restarting, or never seeing the upload complete.
///
/// This function marks the session Idle NOW (user-visible teardown ends
/// here) and FINISHES THE EVICT OP immediately — it does NOT hold the
/// session's one-running op lane for the upload (ADR 0079 review finding
/// #11: an earlier draft watched the finalize row and blocked a queued
/// resume/deliver for up to the upload's duration, reintroducing the
/// evict-then-resume stall this epic exists to kill). The finalize is
/// host-owned: the upload proceeds on the host, and the coordinator
/// learns the durable snapshot row via the heartbeat reconcile
/// (`api/host_http.rs::heartbeat`), independent of any op. The instant
/// the session is Idle it is resumable; a resume that arrives before the
/// row lands falls back to the prior periodic checkpoint (ADR 0028's
/// documented-acceptable bounded loss), never waits on the upload.
async fn finish_eviction_d5(
    ctx: &OpCtx<'_>,
    session_id: SessionId,
    sandbox_id: SandboxId,
    snapshot_id: engram_core::types::SnapshotId,
    session: &engram_core::types::Session,
) -> Result<EvictOutcome, EvictError> {
    let state = ctx.state;
    let now = Utc::now();

    if !ctx.step("mark_idle").await {
        return Ok(EvictOutcome::Fenced);
    }
    // Idle-before-durable: PG sandbox detach (the authoritative unbind,
    // ADR 0047 — no in-memory registry), then state flip. Fenced;
    // `host_id` is re-written unchanged to preserve resume affinity.
    match state
        .services
        .meta
        .fenced_assign_sandbox(session_id, ctx.epoch, None, session.host_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => return Ok(EvictOutcome::Fenced),
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e,
                "D5 eviction: fenced_assign_sandbox(None) failed");
        }
    }
    let prev = match crate::session_ops::transition_with_fence(
        state,
        session_id,
        ctx.fence(),
        SessionState::Idle,
    )
    .await
    {
        Ok(prev) => prev,
        Err(engram_core::MetaError::Conflict(msg)) if msg.starts_with("fenced:") => {
            return Ok(EvictOutcome::Fenced);
        }
        Err(e) => {
            // Issue #529: the host's finalize artifacts are ALREADY
            // durable (snapshot_begin returned) — there is nothing to
            // abort or destroy here. The op's retry hits the idempotent
            // `snapshot_begin` and re-observes the same pending job
            // rather than re-capturing.
            return Err(EvictError::Meta(e.to_string()));
        }
    };
    if session.park_rung != 0 {
        // Fenced (review finding #6).
        let _ = state
            .services
            .meta
            .fenced_set_session_park_rung(session_id, ctx.epoch, 0, None)
            .await;
    }
    let _ = state
        .emit_fenced(session_id, ctx.fence(), SessionEvent::Evicted { at: now })
        .await;
    let _ = state
        .emit_fenced(
            session_id,
            ctx.fence(),
            SessionEvent::StatusChanged {
                from: prev,
                to: SessionState::Idle,
                at: now,
            },
        )
        .await;
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        snapshot_id = %snapshot_id,
        "idle eviction: session Idle after capture; finalize is now a host-owned \
         job (issue #529) — the row lands via the heartbeat reconcile",
    );

    // ADR 0079 (review finding #11): the evict op FINISHES here — the
    // instant the session is durably Idle. The 900s inline finalize
    // row-watch that used to live here HELD the session's one-running op
    // lane for the whole upload, blocking any queued resume/deliver behind
    // it (the very "evict-then-resume collision gone" property the op log
    // is meant to deliver would fail on the full-evict path). The finalize
    // is genuinely host-owned: the host durably persisted its inputs
    // before `snapshot_begin` returned and lands the snapshot row itself
    // via the heartbeat reconcile, surviving this coordinator dying — so
    // there is nothing the coordinator must hold the lane to watch. A
    // resume that arrives before the row lands falls back to the prior
    // checkpoint (ADR 0028 — "the same blast radius as an active host
    // death"), which is the accepted tradeoff for making the session
    // resumable the INSTANT it's Idle.
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
pub(crate) async fn scanner_run_once(
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state.services.meta.list_evicting_sessions().await?;
    // Queue-depth gauge even when 0 — a flatline at 0 is the healthy
    // signal; a climbing value means evictions arrive faster than
    // pipelines complete.
    ::metrics::gauge!(crate::metrics::EVICTION_SCANNER_QUEUE).set(candidates.len() as f64);
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "eviction scanner found Evicting sessions"
    );
    for (session, _attempts) in candidates {
        // ADR 0074 rungs 2-3: a PARKED session (park_rung >= 2) is not a
        // fresh eviction to enqueue — the reaper owns its dwell/pressure
        // descent. Route it there.
        if session.park_rung >= 2 {
            if let Err(e) = park_reaper_advance_one(state, session).await {
                tracing::warn!(error = %e, "park reaper per-session advance failed");
            }
            continue;
        }
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
    // than enqueue a guaranteed no-op. The existing HostLost machinery
    // (dead-host second stage, host-side orphan reap, manual /resume)
    // owns recovery from there, and HostLost is not Active so neither
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
                            at: Utc::now(),
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
    let outcome = crate::session_ops::enqueue(
        state,
        session_id,
        engram_core::types::session_op::OpKind::Evict,
        serde_json::json!({ "target": "idle", "allow_park": true, "nominated": true }),
        Some(&key),
    )
    .await?;
    if let engram_core::types::session_op::EnqueueOutcome::Claimed(_) = outcome {
        tracing::info!(%session_id, "eviction scanner: enqueued evict op (claimed)");
    }
    Ok(())
}

/// ADR 0074 rung reaper: dwell cap (seconds) after which a parked VM is
/// descended to a full eviction even without memory pressure — its RAM
/// isn't worth holding if the user hasn't returned in this long, and a
/// paused guest's TCP connections go stale past this window anyway.
/// Env-tunable; default 15 minutes (ADR 0074 rung-2 dwell).
fn park_dwell_cap() -> chrono::Duration {
    let secs = std::env::var("ENGRAM_PARK_DWELL_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(900);
    chrono::Duration::seconds(secs.max(1))
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
        // implies a live VM). Clear the rung and let the normal scanner
        // path fall it back to HostLost on the next tick.
        tracing::warn!(%session_id, "park reaper: parked row has no sandbox; clearing rung");
        let _ = state
            .services
            .meta
            .set_session_park_rung(session_id, 0, None)
            .await;
        return Ok(());
    };

    // Rung 2 (parked-paused) is the only rung this reaper descends today;
    // rung 3 (parked-local) is handled by the data-plane retention path.
    if session.park_rung != 2 {
        return Ok(());
    }

    let dwell_exceeded = session
        .parked_at
        .map(|at| Utc::now().signed_duration_since(at) >= park_dwell_cap())
        .unwrap_or(true); // no stamp → treat as long-parked (descend)
    let has_headroom = host_has_memory_headroom(state, session_id).await;
    let reason = if !has_headroom {
        "pressure"
    } else if dwell_exceeded {
        "dwell"
    } else {
        // Still within dwell and the host has room — keep the VM parked.
        return Ok(());
    };

    // Key the descent to the park instant: at most one descent op per
    // park, dedup'd across ticks and replicas.
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
        tracing::info!(%session_id, reason, "park reaper: enqueued descent op (parked-paused → full eviction)");
    }
    Ok(())
}

/// Marker that this module exists so unused-arg checkers don't
/// flag the `Arc<dyn SandboxBackend>` we explicitly take below.
#[allow(dead_code)]
fn _backend_unused_check<B: SandboxBackend>(_: std::sync::Arc<B>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
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
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
            blob: std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
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
            "the evict op finishes the instant the session is Idle, not after finalize",
        );
        assert_eq!(meta.session.lock().status, SessionState::Idle);
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
            cloud: Arc::new(MockCloud::new()),
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
            cloud: Arc::new(MockCloud::new()),
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
            cloud: Arc::new(MockCloud::new()),
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
            cloud: Arc::new(MockCloud::new()),
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

    /// ADR 0074 rung 2 (parked-paused): with host memory headroom, an
    /// idle eviction PAUSES the VM in place instead of capturing —
    /// session holds at Evicting, park_rung=2, sandbox alive, nothing
    /// snapshotted. When the user returns, the cancel path un-pauses and
    /// flips back to Active with the same live sandbox (no rebuild).
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
        assert_eq!(s.status, SessionState::Evicting, "parked holds at Evicting");
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

    /// ADR 0074 rung 2 reaper: a parked-paused VM whose dwell cap has
    /// elapsed is DESCENDED to a full eviction (un-pause → capture →
    /// destroy → Idle), reclaiming its RAM. The parking left no trace in
    /// the terminal state.
    #[tokio::test]
    async fn parked_paused_descends_to_full_eviction_on_dwell() {
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
        meta.session.lock().host_id = Some(host_id); // headroom resolves via PG now
        seed_host_with_free_ram(&meta, host_id, 60_000);

        // Park first.
        let op = drive_evict(&state, session_id, true, false).await;
        assert_eq!(op.state, OpState::Done);
        assert_eq!(meta.session.lock().park_rung, 2, "parked-paused");

        // Backdate the park entry beyond the dwell cap (default 600s).
        let stale = chrono::Utc::now() - chrono::Duration::seconds(3600);
        state
            .services
            .meta
            .set_session_park_rung(session_id, 2, Some(stale))
            .await
            .unwrap();

        // A scanner tick routes the parked row to the reaper, which
        // ENQUEUES the descent op (host still has headroom, so the
        // trigger is dwell); the claimed op drives on a detached task.
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
            "dwell-expired park descends to a full eviction"
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
}
