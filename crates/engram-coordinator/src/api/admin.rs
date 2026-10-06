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

// Administrative eviction and durable drain planning.

#[derive(Serialize)]
pub struct EvictIdleResponse {
    pub session_id: SessionId,
    pub status: &'static str,
}

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
    owner: engram_core::types::host::CordonOwner,
) -> Result<CordonResponse, ApiError> {
    match state
        .services
        .meta
        .set_host_cordon(host_id, Some(owner), None)
        .await
    {
        Ok(()) => {}
        Err(engram_core::MetaError::NotFound) => {
            return Err(ApiError::NotFound(format!("host {host_id} has no row")));
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "cordon: set_host_cordon failed: {e}"
            )));
        }
    }
    tracing::info!(%host_id, "admin cordon: host marked non-schedulable (durable)");
    Ok(CordonResponse {
        host_id,
        status: "cordoned",
    })
}

/// ADR 0116 A-D2: transport-agnostic core for the planned-handoff
/// declaration. Computes the deadline from the injected clock
/// (`now + ttl`) and extends the host's binding lease via
/// `begin_host_handoff` (GREATEST semantics — repeated/racing
/// declarations keep the max). Called by the gRPC
/// `FleetService::begin_host_handoff` (the operator, right after
/// cordon) and the HTTP `POST /api/hosts/:id/handoff` (the host's
/// SIGTERM-ladder belt). `accepted = false` (unknown/dead host) is a
/// non-error: the caller proceeds — the roll continues under whatever
/// shield remains, and enforcement (A3) treats an absent lease with the
/// legacy fallback.
pub(crate) async fn begin_host_handoff_core(
    state: &SharedState,
    host_id: engram_core::HostId,
    ttl_secs: u64,
) -> Result<bool, ApiError> {
    // Clamp: a zero/absurd TTL is a caller bug, not a lease we want to
    // honor. The ceiling is the SHARED `MAX_HANDOFF_TTL_SECS` (24 h) the
    // operator also sizes against, so a legitimate roll budget is never
    // silently truncated (#1218 review: a 1 h ceiling here undercut the
    // operator's default ~102 min sizing). Clamping is LOUD — a
    // truncated deadline written silently is exactly how a shield
    // quietly stops covering the roll it exists for.
    let requested = ttl_secs;
    let ttl_secs = ttl_secs.clamp(1, engram_core::types::host::MAX_HANDOFF_TTL_SECS);
    if ttl_secs != requested {
        tracing::warn!(%host_id, requested, applied = ttl_secs,
            "handoff TTL clamped — the declared deadline will NOT match the caller's sizing");
    }
    let until = state.services.clock.now_utc() + chrono::Duration::seconds(ttl_secs as i64);
    let accepted = state
        .services
        .meta
        .begin_host_handoff(host_id, until)
        .await
        .map_err(|e| ApiError::Internal(format!("begin_host_handoff: {e}")))?;
    if accepted {
        tracing::info!(%host_id, ttl_secs, %until,
            "handoff declared: binding lease extended for a planned operation");
    } else {
        tracing::warn!(%host_id, ttl_secs,
            "handoff declaration ignored: host row unknown or dead");
    }
    Ok(accepted)
}

/// `POST /api/admin/hosts/:id/uncordon` — inverse of cordon. The
/// host returns to the picker's view immediately.
/// Transport-agnostic core for the uncordon primitive (ADR 0051). Clears
/// the durable `hosts.cordoned` bit (ADR 0047). The axum `uncordon_host`
/// handler and the gRPC `FleetService::uncordon_host` both call this.
pub(crate) async fn uncordon_host_core(
    state: &SharedState,
    host_id: engram_core::HostId,
    owner: engram_core::types::host::CordonOwner,
) -> Result<CordonResponse, ApiError> {
    match state
        .services
        .meta
        .cancel_host_retirement(host_id, owner)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return Err(ApiError::Conflict(
                "cordon owner differs or host is retired".into(),
            ))
        }
        Err(engram_core::MetaError::NotFound) => {
            return Err(ApiError::NotFound(format!("host {host_id} has no row")));
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "uncordon: set_host_cordon failed: {e}"
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
    pub planned: Vec<SessionId>,
    pub descended: Vec<SessionId>,
    pub skipped: u32,
}

/// ADR 0123 A/B4: record the retirement request for `owner` and plan one
/// teleport per Active resident. The request is durable, so the teleport
/// scanner re-plans the host every tick: a resident that does not fit
/// today is tried again, and until it leaves it blocks the grant as a
/// visible `bound_sessions` blocker. `Conflict` when another owner holds
/// the cordon or the host is dead; idempotent on a retired host.
pub(crate) async fn request_host_retirement_core(
    state: &SharedState,
    host_id: engram_core::HostId,
    owner: engram_core::types::host::CordonOwner,
    reason: engram_core::types::teleport::TeleportReason,
) -> Result<crate::teleport::PlanReport, ApiError> {
    let meta = &state.services.meta;
    let now = state.services.clock.now_utc();
    let requested = meta
        .request_host_retirement(host_id, owner, reason.as_str(), now)
        .await?;
    if !requested {
        let row = meta
            .get_host(host_id)
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("host {host_id} has no row")))?;
        if row.status != engram_core::types::host::HostStatus::Retired {
            let why = match row.cordon_owner {
                Some(other) if other != owner => {
                    format!("host is cordoned by {}", other.as_str())
                }
                _ => format!("host is {}", row.status.as_str()),
            };
            return Err(ApiError::Conflict(why));
        }
    }
    Ok(crate::teleport::plan_host_teleports(state, host_id, reason).await)
}

/// `AdminDrainHost` is the retirement request with `owner = admin`. The
/// host is retired once it is empty; `UncordonHost{owner: admin}` cancels
/// the request before then.
pub(crate) async fn admin_drain_host_core(
    state: &SharedState,
    host_id: engram_core::HostId,
) -> Result<DrainHostResponse, ApiError> {
    let plan = request_host_retirement_core(
        state,
        host_id,
        engram_core::types::host::CordonOwner::Admin,
        engram_core::types::teleport::TeleportReason::AdminDrain,
    )
    .await?;
    Ok(DrainHostResponse {
        host_id,
        planned: plan.planned,
        descended: plan.descended,
        skipped: plan.skipped,
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
    /// `chunk_generation` moved while the mark pass walked the chunk
    /// space. Benign — promote re-verifies the live pin set before it
    /// deletes. Diagnostic for pin-churn pressure.
    pub generation_moved: bool,
    pub promoted_deletes: usize,
    pub promote_delete_errors: usize,
    /// Present when the mark pass failed. The promote pass still ran,
    /// so `promoted_deletes` is meaningful even here; `listed_chunks`
    /// and `candidates_marked` are not.
    pub mark_error: Option<String>,
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
            generation_moved: r.generation_moved,
            promoted_deletes: r.promoted_deletes,
            promote_delete_errors: r.promote_delete_errors,
            mark_error: r.mark_error,
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
    // Admin sweeps start at shard 0 and never touch the cursor or the
    // lease — they are a diagnostic, not the scheduled sweep.
    // A sweep always yields a report; per-pass failures ride in
    // `mark_error` / `promote_error` so a diagnostic run tells the
    // operator WHICH pass broke instead of collapsing to a 500.
    let report = crate::chunk_gc::run_one_sweep(state, &cfg, mode, 0).await;
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
