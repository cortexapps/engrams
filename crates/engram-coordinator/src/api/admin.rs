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

use engram_core::SessionId;

use crate::error::ApiError;
use crate::state::SharedState;

// ---------------------------------------------------------------------
// ADR 0007 — materialize-dir orphan reap
// ---------------------------------------------------------------------

// ADR 0039 Task 32: `reap_materialize_dir` axum shim removed. See the
// gRPC FleetService host-agent RPC for the entry point.
// (Supporting types ReapMaterializeDirParams, ReapMaterializeDirResult,
// PerHostReap, PerHostReapStats also removed — not referenced by any
// core function.)

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

/// Transport-agnostic core for FlushSession (POST /admin/sessions/:id/flush-now).
pub(crate) async fn flush_now_core(
    state: &crate::state::SharedState,
    session_id: engram_core::SessionId,
) -> Result<FlushNowResult, crate::error::ApiError> {
    let session = state.services.meta.get_session(session_id).await?;
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(crate::error::ApiError::Conflict(format!(
            "session {session_id} has no bound sandbox (status={})",
            session.status.as_str(),
        )));
    };
    let host_client = state.services.host.clone();
    let flush_outcome = host_client.flush_sandbox(sandbox_id).await?;
    let Some(manifest_ref) = flush_outcome else {
        return Ok(FlushNowResult {
            outcome: FlushNowOutcome::Idle,
            manifest_version: None,
        });
    };
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
                manifest_version = manifest_ref.version,
                "flush_now: applied — new manifest persisted",
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
                "flush_now: stale — sandbox_id drift between flush and publish; row not updated",
            );
            Ok(FlushNowResult {
                outcome: FlushNowOutcome::Stale,
                manifest_version: None,
            })
        }
    }
}

// ADR 0039 Task 32: `flush_now` axum shim removed. See `flush_now_core` for the gRPC entry point.

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
// fidelity: `evict_session_to_state` runs Pause → Flush → Snapshot
// → Destroy → transition_session, so the on-disk manifest the
// scanner restores from is bit-identical to what the source saw at
// pause time (no flush-vs-pause race; see ADR 0018 §"Commit 12
// rework").

// ADR 0039 Task 32: `EvacuateSessionRequest` struct removed (only used by the `evacuate_session` axum shim).

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

/// Transport-agnostic core for EvacuateSession (POST /admin/sessions/:id/evacuate).
pub(crate) async fn evacuate_session_core(
    state: &crate::state::SharedState,
    session_id: engram_core::SessionId,
) -> Result<EvacuateSessionResponse, crate::error::ApiError> {
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
    crate::idle_evictor::evict_session_to_state(
        state,
        session_id,
        sandbox_id,
        engram_core::types::SessionState::Evacuating,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("evac pipeline: {e}")))?;
    Ok(EvacuateSessionResponse {
        session_id,
        status: "evacuating",
    })
}

// ADR 0039 Task 32: `evacuate_session` axum shim removed. See `evacuate_session_core` for the gRPC entry point.

/// Transport-agnostic core for ChunkGc.
pub(crate) async fn chunk_gc_core(
    state: &crate::state::SharedState,
    dry_run: bool,
    grace_secs: Option<u64>,
) -> Result<ChunkGcSweepResult, crate::error::ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let grace_secs_used = cfg.grace_period.as_secs();
    let mode = if dry_run {
        crate::chunk_gc::SweepMode::DryRun
    } else {
        crate::chunk_gc::SweepMode::Full
    };
    let report = crate::chunk_gc::run_one_sweep(state, &cfg, mode)
        .await
        .map_err(|e| crate::error::ApiError::Internal(format!("chunk-gc: {e}")))?;
    Ok((report, grace_secs_used).into())
}

/// Transport-agnostic core for BundleGc.
pub(crate) async fn bundle_gc_core(
    state: &crate::state::SharedState,
    dry_run: bool,
    grace_secs: Option<u64>,
) -> Result<crate::bundle_gc::BundleSweepReport, crate::error::ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let mode = if dry_run {
        crate::chunk_gc::SweepMode::DryRun
    } else {
        crate::chunk_gc::SweepMode::Full
    };
    crate::bundle_gc::run_one_bundle_sweep(
        state.services.meta.clone(),
        state.services.blob.clone(),
        &cfg,
        mode,
    )
    .await
    .map_err(|e| crate::error::ApiError::Internal(format!("bundle-gc: {e}")))
}

/// Transport-agnostic core for SnapshotBlobGc.
pub(crate) async fn snapshot_blob_gc_core(
    state: &crate::state::SharedState,
    dry_run: bool,
    grace_secs: Option<u64>,
) -> Result<crate::snapshot_blob_gc::SnapshotBlobSweepReport, crate::error::ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let mode = if dry_run {
        crate::chunk_gc::SweepMode::DryRun
    } else {
        crate::chunk_gc::SweepMode::Full
    };
    crate::snapshot_blob_gc::run_one_snapshot_blob_sweep(
        state.services.meta.clone(),
        state.services.blob.clone(),
        &cfg,
        mode,
    )
    .await
    .map_err(|e| crate::error::ApiError::Internal(format!("snapshot-blob-gc: {e}")))
}

// ADR 0039 Task 32: `TeleportSessionRequest` struct and `teleport_session` axum shim removed.
// See the gRPC FleetService TeleportSession RPC for the entry point.

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
    let sandbox_id = state.registry.get(session_id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to pause — it is idle or not yet started".into(),
        )
    })?;
    state
        .services
        .host
        .pause(sandbox_id)
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
    let sandbox_id = state.registry.get(session_id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to resume in place — it is idle or not yet started".into(),
        )
    })?;
    state
        .services
        .host
        .resume(sandbox_id)
        .await
        .map_err(|e| ApiError::Internal(format!("resume sandbox: {e}")))?;
    tracing::info!(%session_id, %sandbox_id, "admin: unfroze microVM in place");
    Ok(Json(PauseResumeResponse {
        session_id,
        note: "resumed",
    }))
}

// ADR 0039 Task 32: `fleet_demand` axum shim and `FleetDemandResponse` removed.
// See the gRPC FleetService for the entry point.

// ADR 0018 commit 12e — host cordon / uncordon / drain
// ---------------------------------------------------------------------

// ADR 0039 Task 32: `cordon_host` axum shim removed. See `cordon_host` (gRPC FleetService) for the entry point.
// ADR 0039 Task 32: `uncordon_host` axum shim removed. See `uncordon_host` (gRPC FleetService) for the entry point.
// (Supporting type `CordonResponse` also removed — only used by the two axum shims above.)

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

/// Transport-agnostic core for DrainHost (POST /admin/hosts/:id/drain and
/// gRPC FleetService::AdminDrainHost). Cordons the host, queries PG for
/// Active sessions, and fans out `evict_session_to_state(Evacuating)` in
/// parallel. Returns the per-session outcomes so each transport layer can
/// encode them in its own wire type.
///
/// Both callers bridge the SessionId-vs-String difference at their own
/// edge:
///  - axum `drain_host`: SessionId is already the wire type → direct.
///  - gRPC `admin_drain_host`: maps SessionId → `.to_string()` before
///    building the proto response.
pub(crate) async fn admin_drain_host_core(
    state: &crate::state::SharedState,
    host_id: engram_core::HostId,
) -> Result<DrainHostResponse, crate::error::ApiError> {
    // ADR 0047: the durable cordon — heartbeats can't clobber it, every
    // replica's picker reads it from PG. A PG failure fails the drain (no
    // in-memory fallback to half-drain behind — ADR 0047 S2 removed the
    // in-memory host scheduler mirror).
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
    // 0048 C8 needs them for the don't-strand guard). For drain we use PG
    // so a fresh coord pod can complete a drain initiated against a sibling.
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
    let mut tasks = tokio::task::JoinSet::new();
    for a in &assignments {
        let st = state.clone();
        let sid = a.session_id;
        let sb = a.sandbox_id;
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
            let fit_ctx = crate::placement::ScheduleContext {
                repo: &repo,
                image_version: &tag,
                prefer_snapshot_id: None,
                memory_mib: Some(mem_budget.max(0) as u32),
                cpu_budget_vcpus: Some(cpu_budget.max(0) as u32),
                required_image_digest: None,
                exclude_host: Some(host_id),
                prefer_host: None,
            };
            match crate::placement::placement_preview(
                st.services.meta.as_ref(),
                &fit_ctx,
                mem_budget,
                cpu_budget as i64,
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
                    prefer_snapshot_id: None,
                    memory_mib: Some(mem_budget.max(0) as u32),
                    cpu_budget_vcpus: Some(cpu_budget.max(0) as u32),
                    required_image_digest: None,
                    exclude_host: Some(host_id),
                    prefer_host: None,
                };
                let target = crate::placement::pick_for_session(
                    st.services.meta.as_ref(),
                    &st.host_registry,
                    &ctx,
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
            let outcome = crate::idle_evictor::evict_session_to_state(
                &st,
                sid,
                sb,
                engram_core::types::SessionState::Evacuating,
            )
            .await;
            (sid, outcome.map_err(|e| e.to_string()))
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

    tracing::info!(
        %host_id,
        evacuating = evacuating.len(),
        failures = failures.len(),
        total = assignments.len(),
        "admin drain: per-session evac pipeline dispatched; scanner will resume each on a peer",
    );

    Ok(DrainHostResponse {
        host_id,
        evacuating,
        failures,
    })
}

// ADR 0039 Task 32: `drain_host` axum shim removed. See `admin_drain_host_core` for the gRPC entry point.

/// Transport-agnostic core for CordonHost / UncordonHost. Writes the
/// durable PG `hosts.cordoned` bit (ADR 0047 — heartbeats never touch it,
/// no in-memory mirror). `NotFound` if the host has no row.
pub(crate) async fn set_cordon_core(
    state: &crate::state::SharedState,
    host_id: engram_core::HostId,
    cordoned: bool,
) -> Result<(), crate::error::ApiError> {
    match state.services.meta.set_host_cordoned(host_id, cordoned).await {
        Ok(()) => {
            tracing::info!(
                %host_id, cordoned,
                "admin cordon: host schedulability updated (durable)",
            );
            Ok(())
        }
        Err(engram_core::MetaError::NotFound) => {
            Err(ApiError::NotFound(format!("host {host_id} has no row")))
        }
        Err(e) => Err(ApiError::Internal(format!(
            "set_host_cordoned failed: {e}"
        ))),
    }
}

/// Transport-agnostic core for DeleteHost (ADR 0048) — deregister a drained
/// host immediately so scale-down doesn't wait ~30-40s for the dead-host
/// detector. Returns the [`DeleteHostOutcome`] so the caller encodes
/// 200/409 itself. Also drops the in-memory registry routing entry.
pub(crate) async fn delete_host_core(
    state: &crate::state::SharedState,
    host_id: engram_core::HostId,
) -> Result<engram_core::types::session::DeleteHostOutcome, crate::error::ApiError> {
    use engram_core::types::session::DeleteHostOutcome;
    match state.services.meta.delete_host(host_id).await {
        Ok(DeleteHostOutcome::Deleted) => {
            // Drop the in-memory routing entry (best-effort; a sibling
            // replica clears its own on the host_dead notify / TTL).
            state.host_registry.unregister(host_id);
            tracing::info!(%host_id, "admin: host deregistered (row deleted)");
            Ok(DeleteHostOutcome::Deleted)
        }
        Ok(other) => Ok(other),
        Err(e) => Err(ApiError::Internal(format!("delete_host: {e}"))),
    }
}

/// The autoscaler's demand signal (ADR 0047/0048). Counts, headroom (both
/// dims), the cordoned footprint, AND the queue all come from PG, so every
/// coordinator replica reports identical demand.
pub(crate) struct FleetDemand {
    pub ready_hosts: u32,
    pub schedulable_hosts: u32,
    pub free_mib: u64,
    pub total_mib: u64,
    pub free_vcpus: u64,
    pub total_vcpus: u64,
    pub cordoned_hosts: u32,
    pub queued_sessions: u64,
    pub queued_mib: u64,
    pub queued_vcpus: u64,
}

/// Transport-agnostic core for FleetDemand (gRPC FleetService::FleetDemand).
pub(crate) async fn fleet_demand_core(state: &crate::state::SharedState) -> FleetDemand {
    let m = crate::placement::fleet_snapshot(state.services.meta.as_ref())
        .await
        .unwrap_or_default();
    // On a transient query failure, fall back to total (treat as "no pressure";
    // scale-down hysteresis rides out a single tick).
    let free_mib = state
        .services
        .meta
        .fleet_free_mib()
        .await
        .map(|f| f.max(0) as u64)
        .unwrap_or(m.total_mib);
    let queued = state
        .services
        .meta
        .queued_demand()
        .await
        .unwrap_or_default();
    FleetDemand {
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

/// Transport-agnostic core for EvictIdle (gRPC FleetService::EvictIdle) — the
/// explicit admin trigger for the idle-eviction pipeline. Fires the exact
/// same primitive (`idle_evictor::evict_idle_session`) the host-side idle
/// detector and the coord `idle_detect_backstop` scanner drive on a timeout.
/// Pre: Active session with a bound sandbox. Post: session at `Idle`,
/// memory snapshot durable, resumable. Synchronous — `Idle` by return.
pub(crate) async fn evict_idle_core(
    state: &crate::state::SharedState,
    session_id: SessionId,
) -> Result<(), crate::error::ApiError> {
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
    crate::idle_evictor::evict_idle_session(state, session_id, sandbox_id)
        .await
        .map_err(|e| ApiError::Internal(format!("idle-evict pipeline: {e}")))?;
    tracing::info!(
        %session_id, %sandbox_id,
        "admin evict-idle: session suspended to Idle via the idle-eviction primitive",
    );
    Ok(())
}

/// Transport-agnostic core for Teleport (gRPC FleetService::Teleport) —
/// relocate an Active session to a chosen host (ADR 0045 Phase F). Tries the
/// LIVE move first (behind `ENGRAM_LIVE_TELEPORT=1`); falls back to the
/// snapshot-rehome evac pipeline (durable target pin on the session row, the
/// `evac_resumer` scanner resumes on that exact host). Returns `"migrated"`
/// (live) or `"evacuating"` (rehome) so the caller encodes its own status.
pub(crate) async fn teleport_core(
    state: &crate::state::SharedState,
    session_id: SessionId,
    target_host_id: engram_core::HostId,
) -> Result<&'static str, crate::error::ApiError> {
    let session = state.services.meta.get_session(session_id).await?;
    if !matches!(session.status, engram_core::types::SessionState::Active) {
        return Err(ApiError::Conflict(format!(
            "teleport only supported for Active sessions (got {})",
            session.status.as_str(),
        )));
    }
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(ApiError::Conflict(format!(
            "session {session_id} has no bound sandbox",
        )));
    };
    // Fail fast if the chosen target can't take the session right now
    // (unknown / draining / full / is the source) — better than marking
    // the session Evacuating and letting the scanner retry-then-Idle
    // against an impossible pin.
    crate::placement::pick_specific_host(
        state.services.meta.as_ref(),
        &state.host_registry,
        target_host_id,
        session.host_id,
    )
    .await
    .map_err(|e| {
        ApiError::Conflict(format!(
            "target host {target_host_id} can't take this session: {e:?}",
        ))
    })?;

    // ADR 0045 C1: behind ENGRAM_LIVE_TELEPORT=1, try the LIVE move first.
    // `Unsupported` falls through to the snapshot-rehome path; other
    // failures surface (their recovery posture is in the error).
    if crate::live_migration::live_teleport_enabled() {
        match crate::live_migration::migrate_session_live(state, session_id, target_host_id).await {
            Ok(()) => return Ok("migrated"),
            Err(crate::live_migration::MigrateError::Unsupported(reason)) => {
                ::metrics::counter!(crate::metrics::MIGRATION_TOTAL,
                    "outcome" => "unsupported_fallback")
                .increment(1);
                tracing::info!(%session_id, %reason,
                    "live teleport unsupported; falling back to snapshot-rehome");
            }
            Err(e) => {
                let outcome = match &e {
                    crate::live_migration::MigrateError::AbortedToSource(_) => "aborted_to_source",
                    crate::live_migration::MigrateError::Parachute(_) => "parachute",
                    _ => "fatal",
                };
                ::metrics::counter!(crate::metrics::MIGRATION_TOTAL, "outcome" => outcome)
                    .increment(1);
                return Err(ApiError::Internal(format!("live teleport: {e}")));
            }
        }
    }

    // Pin the destination durably on the session row (ADR 0047 — the
    // scanner on ANY replica honors it), then fire the same evac pipeline
    // `evacuate` uses. Unwind the pin if the pipeline fails.
    state
        .services
        .meta
        .set_teleport_target(session_id, Some(target_host_id))
        .await
        .map_err(|e| ApiError::Internal(format!("teleport: pin target: {e}")))?;
    if let Err(e) = crate::idle_evictor::evict_session_to_state(
        state,
        session_id,
        sandbox_id,
        engram_core::types::SessionState::Evacuating,
    )
    .await
    {
        let _ = state
            .services
            .meta
            .set_teleport_target(session_id, None)
            .await;
        return Err(ApiError::Internal(format!("teleport pipeline: {e}")));
    }
    tracing::info!(
        %session_id, %sandbox_id, target_host = %target_host_id,
        "admin teleport: session marked Evacuating, pinned to target; scanner will resume there",
    );
    Ok("evacuating")
}

// ---------------------------------------------------------------------
// ADR 0016 Phase C — chunk-GC admin endpoints
// ---------------------------------------------------------------------
//
// ADR 0039 Task 32: `ChunkGcSweepParams` struct removed (only used by the axum shims below).

// ADR 0039 Task 32: `chunk_gc_dry_run`, `chunk_gc_sweep`, `bundle_gc_dry_run`, `bundle_gc_sweep`,
// `snapshot_blob_gc_dry_run`, `snapshot_blob_gc_sweep` axum shims removed.
// See `chunk_gc_core`, `bundle_gc_core`, `snapshot_blob_gc_core` for the gRPC entry points.

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

// ADR 0039 Task 32: `chunk_gc_candidates` axum shim removed. See `chunk_gc_core` for the gRPC entry point.
// (Supporting types ChunkGcCandidatesParams, ChunkGcCandidateView, ChunkGcCandidatesResult also removed.)
