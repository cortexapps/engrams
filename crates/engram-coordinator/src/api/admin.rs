//! Admin endpoints — explicit triggers for primitives whose
//! production driver is implicit (cron, background sweeps).
//! "Explicit triggers for testability": every detector that fires
//! on a schedule also has a hand-callable endpoint here so
//! integration tests can drive the same primitive without waiting
//! for the timer.
//!
//! All endpoints sit behind the same bearer-auth middleware as the
//! other protected routes.
//!
//! ADR 0007 / Phase 7: the cold-tier flush endpoints (`flush_one`
//! and `flush_idle`) are retired. Durability now flows through the
//! chunk store; `reap_materialize_dir` is the ongoing admin
//! surface. The chunk-store GC endpoint was removed 2026-05-23
//! (see ADR 0015 M5 "Known regression — chunk-store GC deleted").

use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;

use engram_core::traits::SessionFence;
use engram_core::SessionId;

use crate::error::ApiError;
use crate::state::SharedState;

// ---------------------------------------------------------------------
// ADR 0007 — materialize-dir orphan reap
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// ADR 0016 Phase B commit 4a — flush-now admin trigger
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct FlushNowResult {
    /// `applied`: the host drained dirty chunks and coord wrote the
    /// new manifest_ref. `idle`: host returned `None` (sandbox not
    /// chunk-tracked OR no dirty bytes); no PG write. `stale`: host
    /// returned a manifest_ref but coord's UPDATE was guarded out
    /// because `sessions.sandbox_id` no longer matches (rebind /
    /// destroy raced).
    pub outcome: FlushNowOutcome,
    /// New manifest version coord persisted, or `None` on `idle`/`stale`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_version: Option<u64>,
}

#[derive(Serialize, PartialEq, Eq, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum FlushNowOutcome {
    Applied,
    Idle,
    Stale,
}

/// Transport-agnostic core for the flush-now primitive (ADR 0016 Phase B,
/// ADR 0051) — the explicit trigger for the FlushScheduler. Forces an
/// immediate flush on the session's bound sandbox + writes the new
/// `sessions.live_disk_manifest_*` row + bumps `chunk_generation`, so e2e
/// tests and operators can drive the publish-to-PG round-trip without
/// sleeping the scheduler's cadence. 404 if the session is unknown; 409 if
/// it has no bound sandbox; otherwise an `outcome` where `idle` means the
/// host had no dirty bytes and `stale` means coord's row-update was guarded
/// out by sandbox_id drift. The gRPC `FleetService::flush_session` calls this.
pub(crate) async fn flush_now_core(
    state: &SharedState,
    session_id: SessionId,
) -> Result<FlushNowResult, ApiError> {
    // Look up the session's bound sandbox. NotFound on the session
    // row bubbles as 404; bound=None is a domain-level conflict.
    let session = state.services.meta.get_session(session_id).await?;
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(ApiError::Conflict(format!(
            "session {session_id} has no bound sandbox (status={})",
            session.status.as_str(),
        )));
    };
    // Dispatch to the host that owns this sandbox.
    let host_client = state.services.host.clone();
    let flush_outcome = host_client.flush_sandbox(sandbox_id).await?;
    let Some(manifest_ref) = flush_outcome else {
        return Ok(FlushNowResult {
            outcome: FlushNowOutcome::Idle,
            manifest_version: None,
        });
    };
    // Funnel directly into the same MetadataStore method the
    // publisher's drain task uses. The sandbox_id guard inside the
    // UPDATE catches the (rare) destroy-or-rebind race; coord just
    // surfaces the outcome to the caller.
    match state
        .services
        .meta
        .update_live_disk_manifest(session_id, sandbox_id, manifest_ref)
        .await?
    {
        engram_core::traits::UpdateOutcome::Applied => {
            tracing::info!(
                %session_id,
                %sandbox_id,
                manifest_id = %manifest_ref.manifest_id,
                manifest_version = manifest_ref.version,
                "admin flush_now: applied",
            );
            Ok(FlushNowResult {
                outcome: FlushNowOutcome::Applied,
                manifest_version: Some(manifest_ref.version),
            })
        }
        engram_core::traits::UpdateOutcome::DroppedStale => {
            tracing::warn!(
                %session_id,
                %sandbox_id,
                manifest_id = %manifest_ref.manifest_id,
                manifest_version = manifest_ref.version,
                "admin flush_now: stale (sandbox_id mismatch on UPDATE)",
            );
            Ok(FlushNowResult {
                outcome: FlushNowOutcome::Stale,
                manifest_version: None,
            })
        }
    }
}

// ---------------------------------------------------------------------
// ADR 0018 — session evacuation admin endpoints (async shape, commit 12)
// ---------------------------------------------------------------------
//
// Commit 12 rewrote evac from a synchronous
// snapshot→restore→rebind→harness-rebuild RPC into a state-machine
// transition + background scanner. The admin surface mirrors that
// split:
//
// - `POST /api/admin/sessions/:id/evacuate` — pause + flush + snapshot
//   the source sandbox, mark the session `Evacuating`. Returns 202
//   immediately. The `evac_resumer` scanner picks the session up on
//   its next tick (≤10s default) and drives it to Active on a peer.
// - `POST /api/admin/hosts/:id/cordon` / `uncordon` — flip the
//   in-memory `HostState.draining` flag + PG `hosts.status` so the
//   picker excludes the host.
// - `POST /api/admin/hosts/:id/drain` — cordon + fire Evacuating on
//   every Active session on the host in parallel. Returns 202 with
//   the list of session_ids being evacuated.
//
// The pause-before-flush ordering is what unblocks cross-host disk
// fidelity: the evict pipeline runs Pause → Flush → Snapshot
// → Destroy → transition_session, so the on-disk manifest the
// scanner restores from is bit-identical to what the source saw at
// pause time (no flush-vs-pause race; see ADR 0018 §"Commit 12
// rework").

#[derive(Serialize)]
pub struct EvacuateSessionResponse {
    pub session_id: SessionId,
    /// "evacuating" — the session is paused, snapshotted, and the
    /// `evac_resumer` scanner will resume it on a peer within the
    /// next sweep interval (≤10s default). Operators can subscribe
    /// to `GET /sessions/:id/events` to watch the
    /// `Evacuating → Created → Active` chain land.
    pub status: &'static str,
}

/// `POST /api/admin/sessions/:id/evacuate` — mark the session
/// `Evacuating`. Pre: Active session with a bound sandbox. Post: the
/// source sandbox is paused, flushed, snapshotted, and destroyed;
/// PG row is at `Evacuating`; `evac_resumer` will resume on a peer.
///
/// Returns 202 Accepted; the scanner is the actual deliverable. Use
/// the session events stream to observe the resume completing.
/// Transport-agnostic core for the evacuate primitive (ADR 0051). Marks
/// an Active session `Evacuating` via the shared eviction pipeline; the
/// `evac_resumer` scanner resumes it on a peer. The axum handler wraps
/// this in `(202, Json<_>)`; the gRPC `FleetService::evacuate_session`
/// reads `.session_id` + `.status` off the bare struct.
pub(crate) async fn evacuate_session_core(
    state: &SharedState,
    session_id: SessionId,
) -> Result<EvacuateSessionResponse, ApiError> {
    let session = state.services.meta.get_session(session_id).await?;
    if !matches!(session.status, engram_core::types::SessionState::Active) {
        return Err(ApiError::Conflict(format!(
            "evacuate only supported for Active sessions (got {})",
            session.status.as_str(),
        )));
    }
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(ApiError::Conflict(format!(
            "session {session_id} has no bound sandbox",
        )));
    };

    // Fire the shared eviction pipeline with `target_state =
    // Evacuating` under an inline op claim (ADR 0079 — the claim is the
    // per-session exclusion; the pipeline is the SAME evict-verb body,
    // step-recorded, so a coordinator death mid-drive is re-driven by
    // the executor's reclaim sweep from the recorded step). The only
    // difference from an idle eviction is the terminal state, so both
    // flows inherit the same recoverability invariants (snapshot durable
    // in BlobStorage before destroy, PG state flips before host-side
    // destroy).
    let claim = crate::session_ops::OpClaim::try_acquire(
        state,
        session_id,
        engram_core::types::session_op::OpKind::Evict,
        serde_json::json!({ "target": "evacuating", "allow_park": false, "nominated": false }),
    )
    .await
    .map_err(|e| ApiError::Internal(format!("op claim acquire failed: {e}")))?
    .ok_or_else(|| {
        ApiError::Conflict(format!(
            "session {session_id} is busy (an op is in flight); retry shortly",
        ))
    })?;
    let _ = sandbox_id; // the pipeline re-reads the binding under the claim
    let result = crate::idle_evictor::run_evict_pipeline(
        &claim.as_ctx(),
        engram_core::types::SessionState::Evacuating,
        false,
        false,
    )
    .await;
    match &result {
        Ok(_) => {
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
    result.map_err(|e| ApiError::Internal(format!("evac pipeline: {e}")))?;

    tracing::info!(
        %session_id,
        %sandbox_id,
        "admin evacuate: session marked Evacuating; scanner will resume on peer",
    );

    Ok(EvacuateSessionResponse {
        session_id,
        status: "evacuating",
    })
}

#[derive(Serialize)]
pub struct EvictIdleResponse {
    pub session_id: SessionId,
    /// The evict op's observed outcome: "idle" (full suspend — paused,
    /// flushed, snapshotted, sandbox destroyed, resume rebinds) or the
    /// ADR 0074 rung-2 "evicting (parked-paused, rung 2)" (VM paused in
    /// place, un-parked by the next prompt). A busy op lane / no-op
    /// completion surfaces as a retryable Conflict instead.
    pub status: &'static str,
}

/// Transport-agnostic core for the idle-eviction primitive (ADR 0051,
/// restored on the gRPC surface as `SessionService::EvictIdle`). The
/// explicit admin trigger for the idle-eviction pipeline: enqueues the
/// exact same evict verb (`idle_evictor::run_evict_pipeline`) the idle
/// detector's nomination enqueues on a timeout,
/// so it is a faithful stand-in for "the session went idle" — without waiting
/// out (or globally lowering) the idle TTL. Pre: Active session with a bound
/// sandbox. Post: session at `Idle`, memory snapshot durable in BlobStorage,
/// resumable. Synchronous (unlike `evacuate`, which hands off to the resumer
/// scanner): the pipeline runs inline and the session is `Idle` by the time
/// this returns.
pub(crate) async fn evict_idle_core(
    state: &SharedState,
    session_id: SessionId,
) -> Result<EvictIdleResponse, ApiError> {
    let session = state.services.meta.get_session(session_id).await?;
    if !matches!(session.status, engram_core::types::SessionState::Active) {
        return Err(ApiError::Conflict(format!(
            "evict-idle only supported for Active sessions (got {})",
            session.status.as_str(),
        )));
    }
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(ApiError::Conflict(format!(
            "session {session_id} has no bound sandbox",
        )));
    };

    // ADR 0079: enqueue the evict verb (parking allowed — this is the
    // faithful stand-in for "the session went idle") and observe the op.
    let observed = crate::api::snapshot::enqueue_and_observe_evict(state, session_id, true).await?;
    let status = match observed {
        crate::api::snapshot::ObservedEvict::ParkedPaused => "parked (paused in place)",
        crate::api::snapshot::ObservedEvict::EvictedSettling => {
            "evicting (capture landed; settling to idle via the heartbeat reconcile)"
        }
        crate::api::snapshot::ObservedEvict::Idle => "idle",
    };
    tracing::info!(
        %session_id,
        %sandbox_id,
        ?observed,
        "admin evict-idle: evict op completed",
    );

    Ok(EvictIdleResponse { session_id, status })
}

// ---------------------------------------------------------------------
// ADR 0045 Phase F — pause / resume a microVM in place (the freeze/flush
// test surface). Thin admin passthroughs to the FC pause/resume
// primitive: freeze or unfreeze the running guest WITHOUT snapshotting,
// destroying, or changing session state. The session stays `Active`; a
// paused guest simply stops executing until resumed. Distinct from
// teleport/evac (which relocate) and from idle-eviction (which
// snapshots + suspends to `Idle`).
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct PauseResumeResponse {
    pub session_id: SessionId,
    pub note: &'static str,
}

/// `POST /api/admin/sessions/:id/pause` — freeze the session's running
/// microVM in place. 409 if the session has no live sandbox (idle / not
/// yet started).
pub async fn pause_session(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<PauseResumeResponse>, ApiError> {
    let sandbox_id = state.resolve_sandbox(session_id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to pause — it is idle or not yet started".into(),
        )
    })?;
    // ADR 0079: epoch threaded by the op executor; 0 until the verb migrates.
    state
        .services
        .host
        .pause(sandbox_id, SessionFence::unfenced())
        .await
        .map_err(|e| ApiError::Internal(format!("pause sandbox: {e}")))?;
    tracing::info!(%session_id, %sandbox_id, "admin: froze microVM in place");
    Ok(Json(PauseResumeResponse {
        session_id,
        note: "paused",
    }))
}

/// `POST /api/admin/sessions/:id/resume` — unfreeze a `pause`d microVM in
/// place. Note: this is the *in-place* unfreeze, NOT `/sessions/:id/resume`
/// (which rehydrates an `Idle` session from a snapshot). 409 if the
/// session has no live sandbox.
pub async fn resume_session(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<PauseResumeResponse>, ApiError> {
    let sandbox_id = state.resolve_sandbox(session_id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to resume in place — it is idle or not yet started".into(),
        )
    })?;
    // ADR 0079: epoch threaded by the op executor; 0 until the verb migrates.
    state
        .services
        .host
        .resume(sandbox_id, SessionFence::unfenced())
        .await
        .map_err(|e| ApiError::Internal(format!("resume sandbox: {e}")))?;
    tracing::info!(%session_id, %sandbox_id, "admin: unfroze microVM in place");
    Ok(Json(PauseResumeResponse {
        session_id,
        note: "resumed",
    }))
}

// ---------------------------------------------------------------------
// ADR 0018 commit 12e — host cordon / uncordon / drain
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct CordonResponse {
    pub host_id: engram_core::HostId,
    pub status: &'static str,
}

/// `POST /api/admin/hosts/:id/cordon` — mark a host non-schedulable.
/// ADR 0047: writes the coordinator-owned `hosts.cordoned` bit, which
/// heartbeats never touch — the cordon is DURABLE until an explicit
/// uncordon, across coord restarts and replicas. A PG failure is a 500
/// (durability is the point; there is no in-memory fallback to lie
/// behind). Succeeds for a host that has a row but no live connection
/// on this pod (a wave-cordon during pod churn must stick). No effect
/// on already-bound sessions — for that, the operator calls `/drain`.
/// Transport-agnostic core for the cordon primitive (ADR 0051). Writes
/// the durable, coordinator-owned `hosts.cordoned` bit (ADR 0047); a PG
/// failure is a 500 (durability is the point — no in-memory fallback).
/// The axum `cordon_host` handler and the gRPC `FleetService::cordon_host`
/// both call this.
pub(crate) async fn cordon_host_core(
    state: &SharedState,
    host_id: engram_core::HostId,
) -> Result<CordonResponse, ApiError> {
    match state.services.meta.set_host_cordoned(host_id, true).await {
        Ok(()) => {}
        Err(engram_core::MetaError::NotFound) => {
            return Err(ApiError::NotFound(format!("host {host_id} has no row")));
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "cordon: set_host_cordoned failed: {e}"
            )));
        }
    }
    tracing::info!(%host_id, "admin cordon: host marked non-schedulable (durable)");
    Ok(CordonResponse {
        host_id,
        status: "cordoned",
    })
}

/// `POST /api/admin/hosts/:id/uncordon` — inverse of cordon. The
/// host returns to the picker's view immediately.
/// Transport-agnostic core for the uncordon primitive (ADR 0051). Clears
/// the durable `hosts.cordoned` bit (ADR 0047). The axum `uncordon_host`
/// handler and the gRPC `FleetService::uncordon_host` both call this.
pub(crate) async fn uncordon_host_core(
    state: &SharedState,
    host_id: engram_core::HostId,
) -> Result<CordonResponse, ApiError> {
    match state.services.meta.set_host_cordoned(host_id, false).await {
        Ok(()) => {}
        Err(engram_core::MetaError::NotFound) => {
            return Err(ApiError::NotFound(format!("host {host_id} has no row")));
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "uncordon: set_host_cordoned failed: {e}"
            )));
        }
    }
    tracing::info!(%host_id, "admin uncordon: host returned to scheduling");
    Ok(CordonResponse {
        host_id,
        status: "ready",
    })
}

#[derive(Serialize)]
pub struct DrainHostResponse {
    pub host_id: engram_core::HostId,
    pub evacuating: Vec<SessionId>,
    pub failures: Vec<DrainFailure>,
}

#[derive(Serialize)]
pub struct DrainFailure {
    pub session_id: SessionId,
    pub error: String,
}

/// `POST /api/admin/hosts/:id/drain` — cordon the host, then run the
/// evict pipeline (target `Evacuating`) for every Active session on
/// it. Returns 202 with the per-session outcomes; the `evac_resumer`
/// scanner is responsible for completing each transition to Active
/// on a peer host.
///
/// Concurrency: each session's eviction is independent and runs in
/// parallel — the source sandbox is paused on the source host
/// concurrently across sessions. The eviction lease serialises
/// per-session retries; two coord pods both running /drain on the
/// same host will see one win per session, the other no-op via the
/// lease guard.
/// Transport-agnostic core for the drain primitive (ADR 0051). Cordons
/// the host (durable `hosts.cordoned` bit), then fans out a live-first
/// move (snapshot-rehome fallback) for every Active session on it. The
/// axum `drain_host` handler wraps this in `(202, Json<_>)`; the gRPC
/// `FleetService::admin_drain_host` reads `.host_id`, `.evacuating`, and
/// `.failures` (each with `.session_id` + `.error`) off the bare struct.
///
/// Issue #208: the per-session live-teleport verbs hold the session lease
/// across multi-second pause/capture/restore blackouts and a `JoinSet`
/// aborts in-flight tasks on drop — so the whole JoinSet is driven on its
/// own DETACHED task (not the caller's future). HTTP/gRPC cancellation
/// then only stops us OBSERVING; the per-session verbs still run to their
/// terminal commit/abort/parachute arms. This guard is preserved exactly.
pub(crate) async fn admin_drain_host_core(
    state: &SharedState,
    host_id: engram_core::HostId,
) -> Result<DrainHostResponse, ApiError> {
    // ADR 0047: the durable cordon — heartbeats can't clobber it, every
    // replica's picker reads it. A PG failure fails the drain (no
    // in-memory fallback to half-drain behind).
    match state.services.meta.set_host_cordoned(host_id, true).await {
        Ok(()) => {}
        Err(engram_core::MetaError::NotFound) => {
            return Err(ApiError::NotFound(format!("host {host_id} has no row")));
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "drain: set_host_cordoned failed: {e}"
            )));
        }
    }

    // PG-authoritative list of sessions bound here (with budgets — ADR
    // 0048 C8 needs them for the don't-strand guard). The in-memory
    // `sandboxes_on_host` map is faster but can lag (post-restart
    // rehydration window). For drain we use PG so a fresh coord pod
    // can complete a drain initiated against a sibling.
    let assignments = state
        .services
        .meta
        .list_active_assignments_with_budgets_on_host(host_id)
        .await
        .map_err(|e| ApiError::Internal(format!("drain: list sessions on host: {e}")))?;

    if assignments.is_empty() {
        tracing::info!(%host_id, "admin drain: host cordoned; no Active sessions to evacuate");
        return Ok(DrainHostResponse {
            host_id,
            evacuating: Vec::new(),
            failures: Vec::new(),
        });
    }

    // Fan out per-session moves. JoinSet so we collect outcomes
    // without giving up on the first error.
    //
    // ADR 0045 C1: LIVE-FIRST. A drain is the teleport's marquee
    // use-case — move each session losslessly to a peer; only fall
    // back to the evict-to-Evacuating snapshot-rehome (the pre-C1
    // behavior, loses post-checkpoint state) when the live move
    // can't run (flag off, no peer capacity, pre-C1 host) or fails
    // back to the source. A Parachute failure already left the
    // session Evacuating with the scanner armed — same end state as
    // the fallback, so it counts as evacuating.
    //
    // Issue #208: the per-session bodies each run a live-teleport verb
    // that holds the session lease across a multi-second pause/capture/
    // restore blackout. A `JoinSet` ABORTS all of its in-flight tasks
    // when it is dropped — so if we held the JoinSet directly on this
    // axum handler future, a client disconnect (drains run for minutes;
    // LB timeouts and operator Ctrl-C are routine) would drop the
    // handler, drop the JoinSet, and abort every in-flight migration
    // mid-blackout. That strands frozen sources and forks sessions
    // exactly like the inline teleport bug. Drive the whole JoinSet on
    // its own detached task and merely await its JoinHandle here: HTTP
    // cancellation then only stops us observing — the per-session verbs
    // still run to their terminal arms.
    let total = assignments.len();
    // Own a clone for the detached driver (the core borrows `state`; the
    // spawned task needs a `'static` owned `SharedState`).
    let driver_state: SharedState = (*state).clone();
    let driver = tokio::spawn(async move {
        let state = driver_state;
        let mut tasks = tokio::task::JoinSet::new();
        for a in &assignments {
            let st = state.clone();
            let sid = a.session_id;
            let mem_budget = a.mem_budget_mib;
            let cpu_budget = a.cpu_budget_vcpus;
            tasks.spawn(async move {
                // ADR 0048 C8 don't-strand guard: before starting ANY move,
                // confirm some SURVIVOR (a non-victim host) fits this session's
                // budgets. If none does, do NOT begin the move — an Active
                // session must never be parked Idle just because the fleet is
                // full. Surface it as a failure so the operator aborts the wave.
                let (repo, tag) = match st.services.meta.get_session(sid).await {
                    Ok(s) => {
                        let (r, t) = engram_core::types::session::split_image_ref(&s.image);
                        (r.to_string(), t.to_string())
                    }
                    Err(e) => return (sid, Err(format!("get_session: {e}"))),
                };
                // ADR 0068: a capacity-fit PREVIEW, not the move itself —
                // no snapshot/manifest is loaded here, so no substrate
                // requirement is derivable (or needed: the actual move,
                // `evacuate_dead_source` or `migrate_session_live` below,
                // re-derives `caps` from the real snapshot it restores and
                // is the authoritative gate). The base capability gate
                // (`host_meets_capabilities` with `Default` requirements)
                // still applies through `placement_preview`.
                let fit_ctx = crate::placement::ScheduleContext {
                    repo: &repo,
                    image_version: &tag,
                    snapshot_host: None,
                    memory_mib: Some(mem_budget.max(0) as u32),
                    cpu_budget_vcpus: Some(cpu_budget.max(0) as u32),
                    required_image_digest: None,
                    exclude_host: Some(host_id),
                    prefer_host: None,
                    caps: crate::placement::CapabilityRequirements::default(),
                    prefer_bundles: &[],
                };
                match crate::placement::placement_preview(
                    st.services.meta.as_ref(),
                    &fit_ctx,
                    mem_budget,
                    cpu_budget as i64,
                    st.services.clock.now_utc(),
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        return (
                            sid,
                            Err("no surviving host has capacity for this session — \
                             drain would strand it (scale up, then retry)"
                                .to_string()),
                        );
                    }
                    Err(e) => return (sid, Err(format!("placement_preview: {e:?}"))),
                }

                if crate::live_migration::live_teleport_enabled() {
                    let ctx = crate::placement::ScheduleContext {
                        repo: &repo,
                        image_version: &tag,
                        snapshot_host: None,
                        memory_mib: Some(mem_budget.max(0) as u32),
                        cpu_budget_vcpus: Some(cpu_budget.max(0) as u32),
                        required_image_digest: None,
                        exclude_host: Some(host_id),
                        prefer_host: None,
                        // ADR 0068: same preview posture as `fit_ctx` above —
                        // `migrate_session_live`'s own capture path is the
                        // authoritative gate for the live-teleport target.
                        caps: crate::placement::CapabilityRequirements::default(),
                        prefer_bundles: &[],
                    };
                    let target = crate::placement::pick_for_session(
                        st.services.meta.as_ref(),
                        &st.host_registry,
                        &ctx,
                        st.services.clock.now_utc(),
                    )
                    .await
                    .ok()
                    .map(|(h, _)| h);
                    if let Some(target) = target {
                        match crate::live_migration::migrate_session_live(&st, sid, target).await {
                            Ok(()) => return (sid, Ok(())),
                            Err(crate::live_migration::MigrateError::Parachute(e)) => {
                                tracing::warn!(
                                    %sid, %host_id, error = %e,
                                    "drain: live move parachuted; scanner rehome armed",
                                );
                                return (sid, Ok(()));
                            }
                            Err(e) => {
                                tracing::warn!(
                                    %sid, %host_id, error = %e,
                                    "drain: live move failed; falling back to snapshot-rehome",
                                );
                            }
                        }
                    }
                }
                // ADR 0079: inline op claim + the shared evict-verb
                // pipeline (target Evacuating). A busy op lane (a
                // concurrent eviction/resume owns the session) is not a
                // drain failure: the session is being moved / handled by
                // the other actor — fold to success, like `Skipped`.
                let claim = match crate::session_ops::OpClaim::try_acquire(
                    &st,
                    sid,
                    engram_core::types::session_op::OpKind::Evict,
                    serde_json::json!({
                        "target": "evacuating", "allow_park": false, "nominated": false
                    }),
                )
                .await
                {
                    Ok(Some(c)) => c,
                    Ok(None) => return (sid, Ok(())),
                    Err(e) => return (sid, Err(format!("op claim acquire: {e}"))),
                };
                let outcome = crate::idle_evictor::run_evict_pipeline(
                    &claim.as_ctx(),
                    engram_core::types::SessionState::Evacuating,
                    false,
                    false,
                )
                .await;
                match &outcome {
                    Ok(_) => {
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
                // A `Skipped` (the session was already relocated, etc.)
                // is folded to success — drain doesn't pin a destination,
                // so unlike teleport (issue #214) there is no stale pin
                // to unwind. Errors still surface as failures.
                (sid, outcome.map(|_| ()).map_err(|e| e.to_string()))
            });
        }

        let mut evacuating: Vec<SessionId> = Vec::new();
        let mut failures: Vec<DrainFailure> = Vec::new();
        while let Some(join) = tasks.join_next().await {
            match join {
                Ok((sid, Ok(()))) => evacuating.push(sid),
                Ok((sid, Err(e))) => {
                    tracing::warn!(%sid, %host_id, error = %e, "drain: per-session evict/guard failed");
                    failures.push(DrainFailure {
                        session_id: sid,
                        error: e,
                    });
                }
                Err(e) => {
                    tracing::warn!(%host_id, error = %e, "drain: join error in per-session task");
                }
            }
        }
        (evacuating, failures)
    });

    // Await the detached driver for the normal (connected) response. If
    // the HTTP request is cancelled, this future is dropped — but the
    // spawned `driver` (and therefore its JoinSet of per-session verbs)
    // keeps running to completion, so no migration is aborted mid-move.
    let (evacuating, failures) = driver.await.map_err(|join_err| {
        ApiError::Internal(format!("drain: driver task panicked: {join_err}"))
    })?;

    tracing::info!(
        %host_id,
        evacuating = evacuating.len(),
        failures = failures.len(),
        total,
        "admin drain: per-session evac pipeline dispatched; scanner will resume each on a peer",
    );

    Ok(DrainHostResponse {
        host_id,
        evacuating,
        failures,
    })
}

// ---------------------------------------------------------------------
// ADR 0016 Phase C — chunk-GC admin endpoints
// ---------------------------------------------------------------------
//
// Three routes mirror the `reap_materialize_dir` shape: explicit
// triggers for the primitives the background sweep loop in
// `chunk_gc.rs` drives implicitly. Tests + ops fire these without
// waiting on the hourly cadence.

#[derive(serde::Deserialize, Default)]
pub struct ChunkGcSweepParams {
    /// Override the grace period (seconds) for this sweep only.
    /// Used by tests to knock 24h → 0 so candidates promote
    /// immediately. Operators typically leave unset and trust the
    /// configured default.
    pub grace_secs: Option<u64>,
}

#[derive(Serialize)]
pub struct ChunkGcSweepResult {
    pub listed_chunks: usize,
    pub malformed_keys: usize,
    pub pin_set_size: usize,
    pub candidates_marked: usize,
    pub restart_count: u32,
    pub restart_budget_exhausted: bool,
    pub promoted_deletes: usize,
    pub promote_delete_errors: usize,
    /// Echoes the grace_secs used (whether from query override or
    /// the config default). Diagnostic for the
    /// "promoted_deletes=0, why?" investigation path.
    pub grace_secs: u64,
}

impl From<(crate::chunk_gc::SweepReport, u64)> for ChunkGcSweepResult {
    fn from(value: (crate::chunk_gc::SweepReport, u64)) -> Self {
        let (r, grace_secs) = value;
        Self {
            listed_chunks: r.listed_chunks,
            malformed_keys: r.malformed_keys,
            pin_set_size: r.pin_set_size,
            candidates_marked: r.candidates_marked,
            restart_count: r.restart_count,
            restart_budget_exhausted: r.restart_budget_exhausted,
            promoted_deletes: r.promoted_deletes,
            promote_delete_errors: r.promote_delete_errors,
            grace_secs,
        }
    }
}

// ----------------------------------------------------------------
// ADR 0051: transport-agnostic GC cores for the app-gRPC FleetService.
// The REST surface keeps its dry-run/sweep route split; the gRPC RPCs
// fold both into one call discriminated by `dry_run`. `grace_secs`
// overrides the configured grace period (test seam); `None` uses the
// config default. Same primitives the background sweep loops fire.
// ----------------------------------------------------------------

/// gRPC `ChunkGc` core. `dry_run=true` classifies + counts (no writes);
/// `false` runs the full candidate-upsert + promote pass.
pub(crate) async fn chunk_gc_core(
    state: &SharedState,
    dry_run: bool,
    grace_secs: Option<u64>,
) -> Result<ChunkGcSweepResult, ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let grace = cfg.grace_period.as_secs();
    let mode = if dry_run {
        crate::chunk_gc::SweepMode::DryRun
    } else {
        crate::chunk_gc::SweepMode::Full
    };
    let report = crate::chunk_gc::run_one_sweep(state, &cfg, mode)
        .await
        .map_err(|e| ApiError::Internal(format!("chunk-gc: {e}")))?;
    Ok((report, grace).into())
}

/// gRPC `BundleGc` core (ADR 0035 §5). Same dry-run/full split.
pub(crate) async fn bundle_gc_core(
    state: &SharedState,
    dry_run: bool,
    grace_secs: Option<u64>,
) -> Result<crate::bundle_gc::BundleSweepReport, ApiError> {
    let mode = if dry_run {
        crate::chunk_gc::SweepMode::DryRun
    } else {
        crate::chunk_gc::SweepMode::Full
    };
    let params = ChunkGcSweepParams { grace_secs };
    let Json(report) = bundle_gc_run((*state).clone(), params, mode).await?;
    Ok(report)
}

/// gRPC `SnapshotBlobGc` core (ADR 0028 addendum). Same dry-run/full split.
pub(crate) async fn snapshot_blob_gc_core(
    state: &SharedState,
    dry_run: bool,
    grace_secs: Option<u64>,
) -> Result<crate::snapshot_blob_gc::SnapshotBlobSweepReport, ApiError> {
    let mode = if dry_run {
        crate::chunk_gc::SweepMode::DryRun
    } else {
        crate::chunk_gc::SweepMode::Full
    };
    let params = ChunkGcSweepParams { grace_secs };
    let Json(report) = snapshot_blob_gc_run((*state).clone(), params, mode).await?;
    Ok(report)
}

/// ADR 0035 §5: the bundle-generation flavor of the chunk-GC sweep,
/// shared by the `BundleGc` gRPC core. `grace_secs=0` drains immediately.
async fn bundle_gc_run(
    state: SharedState,
    params: ChunkGcSweepParams,
    mode: crate::chunk_gc::SweepMode,
) -> Result<Json<crate::bundle_gc::BundleSweepReport>, ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = params.grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let report = crate::bundle_gc::run_one_bundle_sweep(
        state.services.meta.clone(),
        state.services.blob.clone(),
        &cfg,
        mode,
        &state.services.clock,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("bundle-gc: {e}")))?;
    Ok(Json(report))
}

/// ADR 0028 addendum: snapshot-blob GC sweep, shared by the
/// `SnapshotBlobGc` gRPC core. `grace_secs=0` drains immediately.
async fn snapshot_blob_gc_run(
    state: SharedState,
    params: ChunkGcSweepParams,
    mode: crate::chunk_gc::SweepMode,
) -> Result<Json<crate::snapshot_blob_gc::SnapshotBlobSweepReport>, ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = params.grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let report = crate::snapshot_blob_gc::run_one_snapshot_blob_sweep(
        state.services.meta.clone(),
        state.services.blob.clone(),
        &cfg,
        mode,
        &state.services.clock,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("snapshot-blob-gc: {e}")))?;
    Ok(Json(report))
}

// ---------------------------------------------------------------------
// ADR 0044 K4 — fleet-demand signal for the node-pool autoscaler
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct FleetDemandResponse {
    /// Hosts with a live heartbeat.
    pub ready_hosts: u32,
    /// Non-draining hosts the scheduler can place on.
    pub schedulable_hosts: u32,
    /// ADR 0046: Σ max(0, allocatable_mib − reserved) over schedulable hosts.
    pub free_mib: u64,
    /// Σ allocatable_mib over schedulable hosts.
    pub total_mib: u64,
    /// ADR 0048: Σ spare vCPU over schedulable hosts, and the total budget.
    pub free_vcpus: u64,
    pub total_vcpus: u64,
    /// ADR 0048: hosts cordoned for scale-down.
    pub cordoned_hosts: u32,
    /// ADR 0048: the queue. `queued_*` is the demand to scale UP to fit;
    /// scale-DOWN is hard-gated on `queued_sessions == 0`.
    pub queued_sessions: u64,
    pub queued_mib: u64,
    pub queued_vcpus: u64,
}

/// Transport-agnostic core for the fleet-demand signal (ADR 0051, restored on
/// the gRPC surface as `FleetService::GetFleetDemand`). The autoscaler's
/// input. ADR 0047/0048: counts, headroom (both dims), the cordoned footprint,
/// AND the queue all come from PG, so every coordinator replica reports
/// identical demand. Infallible: a transient PG query failure falls back to
/// "no pressure" (free = total) so scale-down hysteresis rides out a single
/// tick rather than surfacing a 5xx to the autoscaler.
pub(crate) async fn fleet_demand_core(state: &SharedState) -> FleetDemandResponse {
    let m = crate::placement::fleet_snapshot(
        state.services.meta.as_ref(),
        state.services.clock.now_utc(),
    )
    .await
    .unwrap_or_default();
    // free_mib rides the snapshot now: same capability/TTL-gated host set
    // as schedulable_hosts, so the autoscaler can't see capacity placement
    // won't use (the retired SQL `fleet_free_mib` did — the 2026-07-11
    // under-scaling mechanism).
    let free_mib = m.free_mib;
    let queued = state
        .services
        .meta
        .queued_demand()
        .await
        .unwrap_or_default();
    FleetDemandResponse {
        ready_hosts: m.ready_hosts,
        schedulable_hosts: m.schedulable_hosts,
        free_mib,
        total_mib: m.total_mib,
        free_vcpus: m.free_vcpus,
        total_vcpus: m.total_vcpus,
        cordoned_hosts: m.cordoned_hosts,
        queued_sessions: queued.sessions,
        queued_mib: queued.mem_mib,
        queued_vcpus: queued.vcpus,
    }
}
