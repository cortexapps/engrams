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
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use engram_core::SessionId;

use crate::error::ApiError;
use crate::state::SharedState;

// ---------------------------------------------------------------------
// ADR 0007 — materialize-dir orphan reap
// ---------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
pub struct ReapMaterializeDirParams {
    /// Minimum age (seconds) a materialized file must reach
    /// before the reaper considers it for deletion. Guards a
    /// freshly-materialized file from being ripped out from
    /// under an in-flight `create()`'s clonefile step. Default
    /// 1h; tune down to ~5min in high-churn deployments.
    pub min_age_secs: Option<u64>,
}

#[derive(Serialize)]
pub struct ReapMaterializeDirResult {
    pub files_scanned: u64,
    pub files_deleted: u64,
    pub bytes_freed: u64,
    pub files_skipped_unparseable: u64,
    pub files_skipped_too_young: u64,
    pub live_manifest_count: usize,
    pub min_age_secs: u64,
    /// Local materialize_dir in `--mode=all`; empty in
    /// `--mode=coordinator` (each host has its own dir, surfaced
    /// per-host below).
    pub materialize_dir: String,
    /// `--mode=coordinator` fanout: per-host reap outcomes.
    /// Empty in `--mode=all`. Stays present (zero-length) instead
    /// of optional so clients can deserialize one shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub per_host: Vec<PerHostReap>,
}

#[derive(Serialize)]
pub struct PerHostReap {
    pub host_id: String,
    /// Per-host stats. Absent when the host returned an error; in
    /// that case `error` is populated. Mutually exclusive with
    /// `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<PerHostReapStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct PerHostReapStats {
    pub files_scanned: u64,
    pub files_deleted: u64,
    pub bytes_freed: u64,
    pub files_skipped_unparseable: u64,
    pub files_skipped_too_young: u64,
}

/// `POST /api/admin/reap-materialize-dir` — drop materialized
/// `<manifest_id>-vN.ext4` files whose manifest_id is no longer
/// referenced by any live snapshot row. Pairs with the chunk-
/// store GC endpoint above: that one sweeps chunks; this one
/// sweeps the assembled files derived from chunks.
///
/// Dispatch:
/// - `--mode=all`: runs in-proc against the coordinator's local
///   `materialize_dir`. Top-level fields populated; `per_host`
///   empty.
/// - `--mode=coordinator`: fans out across every registered host
///   that exposes a `ConnectedHost`. Top-level totals are summed
///   across hosts; `per_host` carries individual outcomes
///   (including any per-host failures — one host's RPC error
///   doesn't fail the aggregate). Hosts without a wired admin
///   handler return a clean "unsupported" error rather than
///   bringing down the whole sweep.
pub async fn reap_materialize_dir(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ReapMaterializeDirParams>,
) -> Result<Json<ReapMaterializeDirResult>, ApiError> {
    let min_age_secs = params.min_age_secs.unwrap_or(3600);

    // Live-set source = the same DB query the chunk GC uses. The
    // two reapers stay coherent: a manifest that survives the
    // chunk sweep also keeps its materialized files, and
    // vice-versa. Computed once + shared across the in-proc path
    // AND the per-host fanout.
    let live_ids_vec = state
        .services
        .meta
        .list_live_disk_manifest_ids()
        .await
        .map_err(|e| ApiError::Internal(format!("list_live_disk_manifest_ids: {e}")))?;
    let live_manifest_count = live_ids_vec.len();

    // In-proc path (--mode=all): the coord owns a local
    // materialize_dir; skip the fanout entirely.
    if let Some(dir) = state.services.materialize_dir.as_ref() {
        let live: std::collections::HashSet<uuid::Uuid> = live_ids_vec.iter().copied().collect();
        let min_age = std::time::Duration::from_secs(min_age_secs);

        tracing::info!(
            materialize_dir = %dir.display(),
            live_manifest_count,
            min_age_secs,
            "starting materialize-dir orphan reap (in-proc)",
        );

        let stats = engram_host_agent::orphan_reap::reap_materialize_dir(dir, &live, min_age)
            .await
            .map_err(|e| ApiError::Internal(format!("reap_materialize_dir: {e}")))?;

        tracing::info!(
            files_scanned = stats.files_scanned,
            files_deleted = stats.files_deleted,
            bytes_freed = stats.bytes_freed,
            "materialize-dir orphan reap complete",
        );

        return Ok(Json(ReapMaterializeDirResult {
            files_scanned: stats.files_scanned,
            files_deleted: stats.files_deleted,
            bytes_freed: stats.bytes_freed,
            files_skipped_unparseable: stats.files_skipped_unparseable,
            files_skipped_too_young: stats.files_skipped_too_young,
            live_manifest_count,
            min_age_secs,
            materialize_dir: dir.display().to_string(),
            per_host: Vec::new(),
        }));
    }

    // Fanout path (--mode=coordinator): walk every connected host
    // with an admin_client and call the RPC. Aggregate the stats
    // for the top-level response so consumers can use one
    // shape regardless of mode; per-host details land in
    // `per_host`.
    let host_ids = state.host_registry.host_ids();
    tracing::info!(
        host_count = host_ids.len(),
        live_manifest_count,
        min_age_secs,
        "starting materialize-dir orphan reap (multi-host fanout)",
    );

    let mut per_host = Vec::with_capacity(host_ids.len());
    let mut total_scanned = 0u64;
    let mut total_deleted = 0u64;
    let mut total_bytes_freed = 0u64;
    let mut total_skipped_unparseable = 0u64;
    let mut total_skipped_too_young = 0u64;

    for host_id in host_ids {
        // ADR 0013: admin reap fanout goes through the gRPC pool.
        // `get` returns the pool's `GrpcHostClient`; if the host
        // hasn't registered yet (no host_addr persisted) we skip
        // it with a clear per-host error so the aggregate isn't
        // partially silent.
        let client = match state.services.host_pool.get(host_id) {
            Ok(c) => c,
            Err(_) => {
                per_host.push(PerHostReap {
                    host_id: host_id.to_string(),
                    stats: None,
                    error: Some("host not in gRPC pool (not yet registered)".into()),
                });
                continue;
            }
        };
        match client
            .reap_materialize_dir(min_age_secs, live_ids_vec.clone())
            .await
        {
            Ok(stats) => {
                total_scanned += stats.files_scanned;
                total_deleted += stats.files_deleted;
                total_bytes_freed += stats.bytes_freed;
                total_skipped_unparseable += stats.files_skipped_unparseable;
                total_skipped_too_young += stats.files_skipped_too_young;
                per_host.push(PerHostReap {
                    host_id: host_id.to_string(),
                    stats: Some(PerHostReapStats {
                        files_scanned: stats.files_scanned,
                        files_deleted: stats.files_deleted,
                        bytes_freed: stats.bytes_freed,
                        files_skipped_unparseable: stats.files_skipped_unparseable,
                        files_skipped_too_young: stats.files_skipped_too_young,
                    }),
                    error: None,
                });
            }
            Err(e) => {
                tracing::warn!(
                    host_id = %host_id,
                    error = %e,
                    "host failed materialize-dir reap; other hosts unaffected",
                );
                per_host.push(PerHostReap {
                    host_id: host_id.to_string(),
                    stats: None,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    tracing::info!(
        hosts_visited = per_host.len(),
        files_deleted = total_deleted,
        bytes_freed = total_bytes_freed,
        "materialize-dir orphan reap (fanout) complete",
    );

    Ok(Json(ReapMaterializeDirResult {
        files_scanned: total_scanned,
        files_deleted: total_deleted,
        bytes_freed: total_bytes_freed,
        files_skipped_unparseable: total_skipped_unparseable,
        files_skipped_too_young: total_skipped_too_young,
        live_manifest_count,
        min_age_secs,
        materialize_dir: String::new(),
        per_host,
    }))
}

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

/// `POST /api/admin/sessions/:id/flush-now` — explicit trigger for
/// the FlushScheduler primitive (ADR 0016 Phase B). Forces an
/// immediate flush on the session's bound sandbox + writes the new
/// `sessions.live_disk_manifest_*` row + bumps `chunk_generation`.
/// Pairs the scheduler's 30s tick / threshold-notify with an
/// admin trigger so e2e tests and operators can drive the
/// publish-to-PG round-trip without sleeping a cadence.
///
/// Returns:
/// - 404 if the session is unknown
/// - 409 if the session has no bound sandbox (Idle / never-bound)
/// - 200 with `{outcome, manifest_version}` otherwise. `idle` means
///   the host had no dirty bytes to flush; `stale` means the host
///   produced a manifest but coord's row-update was guarded out
///   (sandbox_id drift between flush and publish — same race the
///   scheduler-publish path's sandbox_id guard catches).
pub async fn flush_now(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<FlushNowResult>, ApiError> {
    Ok(Json(flush_now_core(&state, session_id).await?))
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
// fidelity: `evict_session_to_state` runs Pause → Flush → Snapshot
// → Destroy → transition_session, so the on-disk manifest the
// scanner restores from is bit-identical to what the source saw at
// pause time (no flush-vs-pause race; see ADR 0018 §"Commit 12
// rework").

#[derive(serde::Deserialize, Default)]
pub struct EvacuateSessionRequest {
    /// Reserved for a future operator override. Today the scanner
    /// picks any non-source host via the standard policy. Carried in
    /// the type for forward-compat with the pre-rewrite shape; ignored
    /// by the handler.
    #[serde(default)]
    #[allow(dead_code)]
    pub target_host: Option<engram_core::HostId>,
}

#[derive(Serialize)]
pub struct EvacuateSessionResponse {
    pub session_id: SessionId,
    /// "evacuating" — the session is paused, snapshotted, and the
    /// `evac_resumer` scanner will resume it on a peer within the
    /// next sweep interval (≤10s default). Operators can subscribe
    /// to `GET /sessions/:id/events` to watch the
    /// `Evacuating → Created → Active` chain land.
    pub status: &'static str,
    /// The sandbox that was evicted. Returned by the core so the
    /// axum wrapper can log it without a second `get_session` call
    /// (the sandbox is already destroyed by the time we return).
    #[serde(skip)]
    pub sandbox_id: Option<engram_core::SandboxId>,
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
        sandbox_id: Some(sandbox_id),
    })
}

/// `POST /api/admin/sessions/:id/evacuate` — mark the session
/// `Evacuating`. Pre: Active session with a bound sandbox. Post: the
/// source sandbox is paused, flushed, snapshotted, and destroyed;
/// PG row is at `Evacuating`; `evac_resumer` will resume on a peer.
///
/// Returns 202 Accepted; the scanner is the actual deliverable. Use
/// the session events stream to observe the resume completing.
///
/// // Mirrored by evacuate_session() (gRPC FleetService) — see its DRIFT WARNING
pub async fn evacuate_session(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
    Json(_req): Json<EvacuateSessionRequest>,
) -> Result<(StatusCode, Json<EvacuateSessionResponse>), ApiError> {
    let resp = evacuate_session_core(&state, session_id).await?;
    // Log AFTER the core succeeds so the message only appears when the
    // evac pipeline has actually completed (pause+flush+snapshot+destroy).
    // sandbox_id comes from the core — no second get_session needed.
    match resp.sandbox_id {
        Some(sb) => tracing::info!(
            %session_id,
            sandbox_id = %sb,
            "admin evacuate: session marked Evacuating; scanner will resume on peer",
        ),
        None => tracing::info!(
            %session_id,
            "admin evacuate: session marked Evacuating; scanner will resume on peer",
        ),
    }
    Ok((StatusCode::ACCEPTED, Json(resp)))
}

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

#[derive(serde::Deserialize)]
pub struct TeleportSessionRequest {
    /// The destination host to relocate the session onto.
    pub target_host_id: engram_core::HostId,
}

/// `POST /api/admin/sessions/:id/teleport` — relocate an Active session
/// to a **chosen** host. ADR 0045 Phase F: the operator/test surface for
/// live migration. Today it rides the snapshot-rehome evac pipeline
/// (pause → flush → snapshot → restore on the pinned host, ~seconds of
/// downtime); ADR 0045 Phase C swaps the implementation to post-copy
/// live teleport (~10ms) under this same verb, so the surface is stable.
///
/// Pre: Active session with a bound sandbox; `target_host_id` is a
/// schedulable host that isn't the source. Post: 202; the session is
/// marked `Evacuating` with the target pinned in `state.teleport_targets`,
/// and the `evac_resumer` scanner resumes it on that exact host.
pub async fn teleport_session(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<TeleportSessionRequest>,
) -> Result<(StatusCode, Json<EvacuateSessionResponse>), ApiError> {
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

    // Fail fast with a clear error if the chosen target can't take the
    // session right now (unknown / draining / full / is the source) —
    // better than marking the session Evacuating and letting the scanner
    // retry-then-Idle against an impossible pin.
    state
        .host_registry
        .pick_specific_host(req.target_host_id, session.host_id)
        .map_err(|e| {
            ApiError::Conflict(format!(
                "target host {} can't take this session: {e:?}",
                req.target_host_id,
            ))
        })?;

    // ADR 0045 C1: behind ENGRAM_LIVE_TELEPORT=1, try the LIVE move
    // first — the eager dirty-set push (no GCS on the pause path, no
    // post-checkpoint state loss, no scanner tick). `Unsupported`
    // (pre-C1 binaries, no chain, no host_addr) falls through to the
    // snapshot-rehome path below; other failures surface — their
    // recovery posture (aborted-to-source vs scanner parachute) is in
    // the error.
    if crate::live_migration::live_teleport_enabled() {
        match crate::live_migration::migrate_session_live(&state, session_id, req.target_host_id)
            .await
        {
            Ok(()) => {
                return Ok((
                    StatusCode::OK,
                    Json(EvacuateSessionResponse {
                        session_id,
                        status: "migrated",
                        sandbox_id: None,
                    }),
                ));
            }
            Err(crate::live_migration::MigrateError::Unsupported(reason)) => {
                metrics::counter!(crate::metrics::MIGRATION_TOTAL,
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
                metrics::counter!(crate::metrics::MIGRATION_TOTAL, "outcome" => outcome)
                    .increment(1);
                return Err(ApiError::Internal(format!("live teleport: {e}")));
            }
        }
    }

    // Pin the destination, then fire the same evac pipeline `evacuate`
    // uses — the only difference is the scanner reads the pin and places
    // on this exact host. Unwind the pin if the pipeline itself fails.
    state
        .teleport_targets
        .insert(session_id, req.target_host_id);
    if let Err(e) = crate::idle_evictor::evict_session_to_state(
        &state,
        session_id,
        sandbox_id,
        engram_core::types::SessionState::Evacuating,
    )
    .await
    {
        state.teleport_targets.remove(&session_id);
        return Err(ApiError::Internal(format!("teleport pipeline: {e}")));
    }

    tracing::info!(
        %session_id,
        %sandbox_id,
        target_host = %req.target_host_id,
        "admin teleport: session marked Evacuating, pinned to target; scanner will resume there",
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(EvacuateSessionResponse {
            session_id,
            status: "evacuating",
            sandbox_id: None,
        }),
    ))
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

// ---------------------------------------------------------------------
// ADR 0044 K4 — fleet-demand signal for the node-pool autoscaler
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct FleetDemandResponse {
    /// Hosts with a live heartbeat.
    pub ready_hosts: u32,
    /// Non-draining hosts the scheduler can place on.
    pub schedulable_hosts: u32,
    /// ADR 0046: Σ max(0, allocatable_mib − reserved) over schedulable hosts —
    /// the host-measured headroom (nets out the daemon/OS/chunk-cache/mlock
    /// baseline), the autoscaler's true demand signal.
    pub free_mib: u64,
    /// Σ total_mib over schedulable hosts — lets the caller derive the
    /// average per-host capacity (how much one node adds).
    pub total_mib: u64,
}

/// `GET /api/admin/fleet/demand` — the K4 node-pool autoscaler's input. Host
/// counts come from the in-memory registry; `free_mib` is the host-measured
/// allocatable minus reserved session budgets (PG, ADR 0046) so the autoscaler
/// scales on true demand rather than the phantom `total − used(=0)`.
pub async fn fleet_demand(State(state): State<SharedState>) -> Json<FleetDemandResponse> {
    let m = state.host_registry.fleet_metrics();
    // On a transient query failure, fall back to total (treat as "no pressure";
    // scale-down hysteresis rides out a single tick).
    let free_mib = state
        .services
        .meta
        .fleet_free_mib()
        .await
        .map(|f| f.max(0) as u64)
        .unwrap_or(m.total_mib);
    Json(FleetDemandResponse {
        ready_hosts: m.ready_hosts,
        schedulable_hosts: m.schedulable_hosts,
        free_mib,
        total_mib: m.total_mib,
    })
}

// ADR 0018 commit 12e — host cordon / uncordon / drain
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct CordonResponse {
    pub host_id: engram_core::HostId,
    pub status: &'static str,
}

/// `POST /api/admin/hosts/:id/cordon` — mark a host non-schedulable.
/// Flips `HostState.draining` so `pick_for_session` excludes it from
/// new placements + evac targets. PG `hosts.status` is updated to
/// `Draining` in the same call so a coord pod restart (or sibling
/// pod) observes the cordon. No effect on already-bound sessions on
/// this host — for that, the operator calls `/drain` (or evacs each
/// session by hand).
///
/// // Mirrored by cordon_host() (gRPC FleetService) — see its DRIFT WARNING
pub async fn cordon_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<Json<CordonResponse>, ApiError> {
    if !state.host_registry.cordon(host_id) {
        return Err(ApiError::NotFound(format!("host {host_id} not registered")));
    }
    if let Err(e) = state
        .services
        .meta
        .set_host_status(host_id, engram_core::types::HostStatus::Draining)
        .await
    {
        // In-memory flag flipped; PG write failed. Log + return ok —
        // the picker already filters this host out via the in-memory
        // flag. The heartbeat handler on the next tick will rewrite
        // hosts.status from whatever the host reports (typically
        // Ready), which would clobber the cordon. To prevent that
        // requires a PG-anchored cordon (follow-up); for v1 of the
        // drain story we accept the eventual-consistency risk and
        // log loudly.
        tracing::warn!(
            %host_id, error = %e,
            "cordon: in-memory flipped but set_host_status(Draining) failed; \
             a heartbeat may reset hosts.status to Ready before scanner action",
        );
    }
    tracing::info!(%host_id, "admin cordon: host marked non-schedulable");
    Ok(Json(CordonResponse {
        host_id,
        status: "draining",
    }))
}

/// `POST /api/admin/hosts/:id/uncordon` — inverse of cordon. The
/// host returns to the picker's view immediately.
///
/// // Mirrored by uncordon_host() (gRPC FleetService) — see its DRIFT WARNING
pub async fn uncordon_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<Json<CordonResponse>, ApiError> {
    if !state.host_registry.uncordon(host_id) {
        return Err(ApiError::NotFound(format!("host {host_id} not registered")));
    }
    if let Err(e) = state
        .services
        .meta
        .set_host_status(host_id, engram_core::types::HostStatus::Ready)
        .await
    {
        tracing::warn!(
            %host_id, error = %e,
            "uncordon: in-memory flipped but set_host_status(Ready) failed; \
             next heartbeat will reconcile",
        );
    }
    tracing::info!(%host_id, "admin uncordon: host returned to scheduling");
    Ok(Json(CordonResponse {
        host_id,
        status: "ready",
    }))
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
    if !state.host_registry.cordon(host_id) {
        return Err(ApiError::NotFound(format!("host {host_id} not registered")));
    }
    if let Err(e) = state
        .services
        .meta
        .set_host_status(host_id, engram_core::types::HostStatus::Draining)
        .await
    {
        tracing::warn!(
            %host_id, error = %e,
            "drain: cordon PG write failed; continuing — in-memory flag is set",
        );
    }

    // PG-authoritative list of sessions bound here. The in-memory
    // `sandboxes_on_host` map is faster but can lag (post-restart
    // rehydration window). For drain we use PG so a fresh coord pod
    // can complete a drain initiated against a sibling.
    let assignments = state
        .services
        .meta
        .list_active_sandbox_assignments_on_host(host_id)
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

    // Fan out per-session evictions. JoinSet so we collect outcomes
    // without giving up on the first error.
    let mut tasks = tokio::task::JoinSet::new();
    for (session_id, sandbox_id) in &assignments {
        let st = state.clone();
        let sid = *session_id;
        let sb = *sandbox_id;
        tasks.spawn(async move {
            let outcome = crate::idle_evictor::evict_session_to_state(
                &st,
                sid,
                sb,
                engram_core::types::SessionState::Evacuating,
            )
            .await;
            (sid, outcome)
        });
    }

    let mut evacuating: Vec<SessionId> = Vec::new();
    let mut failures: Vec<DrainFailure> = Vec::new();
    while let Some(join) = tasks.join_next().await {
        match join {
            Ok((sid, Ok(()))) => evacuating.push(sid),
            Ok((sid, Err(e))) => {
                tracing::warn!(%sid, %host_id, error = %e, "drain: per-session evict failed");
                failures.push(DrainFailure {
                    session_id: sid,
                    error: e.to_string(),
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

/// `POST /api/admin/hosts/:id/drain` — cordon the host, then fire
/// `evict_session_to_state(Evacuating)` for every Active session on
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
///
/// // Mirrored by admin_drain_host() (gRPC FleetService) — delegates to
/// // admin_drain_host_core; see its DRIFT WARNING for the gRPC side.
pub async fn drain_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<(StatusCode, Json<DrainHostResponse>), ApiError> {
    let resp = admin_drain_host_core(&state, host_id).await?;
    Ok((StatusCode::ACCEPTED, Json(resp)))
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

/// `POST /api/admin/chunk-gc/dry-run` — classify + count, no
/// writes. Operator-safe to fire any time.
pub async fn chunk_gc_dry_run(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcSweepParams>,
) -> Result<Json<ChunkGcSweepResult>, ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = params.grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let grace_secs = cfg.grace_period.as_secs();
    let report = crate::chunk_gc::run_one_sweep(&state, &cfg, crate::chunk_gc::SweepMode::DryRun)
        .await
        .map_err(|e| ApiError::Internal(format!("chunk-gc dry-run: {e}")))?;
    Ok(Json((report, grace_secs).into()))
}

/// `POST /api/admin/chunk-gc/sweep` — full pipeline: candidate
/// upserts + promote pass (BlobStorage deletes). Same primitive
/// the background loop fires; the endpoint is the test seam plus
/// the operator-driven path for "I want this to drain *now*."
pub async fn chunk_gc_sweep(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcSweepParams>,
) -> Result<Json<ChunkGcSweepResult>, ApiError> {
    let mut cfg = crate::chunk_gc::ChunkGcConfig::from_env();
    if let Some(secs) = params.grace_secs {
        cfg.grace_period = std::time::Duration::from_secs(secs);
    }
    let grace_secs = cfg.grace_period.as_secs();
    let report = crate::chunk_gc::run_one_sweep(&state, &cfg, crate::chunk_gc::SweepMode::Full)
        .await
        .map_err(|e| ApiError::Internal(format!("chunk-gc sweep: {e}")))?;
    Ok(Json((report, grace_secs).into()))
}

/// `POST /api/admin/bundle-gc/dry-run` + `/sweep` — ADR 0035 §5:
/// the bundle-generation flavor of the chunk-GC endpoints. Same
/// primitive the background loop fires (explicit admin trigger for
/// testability); `grace_secs=0` lets an operator drain immediately.
pub async fn bundle_gc_dry_run(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcSweepParams>,
) -> Result<Json<crate::bundle_gc::BundleSweepReport>, ApiError> {
    bundle_gc_run(state, params, crate::chunk_gc::SweepMode::DryRun).await
}

pub async fn bundle_gc_sweep(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcSweepParams>,
) -> Result<Json<crate::bundle_gc::BundleSweepReport>, ApiError> {
    bundle_gc_run(state, params, crate::chunk_gc::SweepMode::Full).await
}

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
    )
    .await
    .map_err(|e| ApiError::Internal(format!("bundle-gc: {e}")))?;
    Ok(Json(report))
}

/// ADR 0028 addendum: snapshot-blob GC — dry-run (classify, no delete)
/// and sweep (`grace_secs=0` drains immediately). Mirrors the chunk +
/// bundle gc admin seams.
pub async fn snapshot_blob_gc_dry_run(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcSweepParams>,
) -> Result<Json<crate::snapshot_blob_gc::SnapshotBlobSweepReport>, ApiError> {
    snapshot_blob_gc_run(state, params, crate::chunk_gc::SweepMode::DryRun).await
}

pub async fn snapshot_blob_gc_sweep(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcSweepParams>,
) -> Result<Json<crate::snapshot_blob_gc::SnapshotBlobSweepReport>, ApiError> {
    snapshot_blob_gc_run(state, params, crate::chunk_gc::SweepMode::Full).await
}

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
    )
    .await
    .map_err(|e| ApiError::Internal(format!("snapshot-blob-gc: {e}")))?;
    Ok(Json(report))
}

#[derive(serde::Deserialize, Default)]
pub struct ChunkGcCandidatesParams {
    /// Max rows to return. Default 100, cap 10_000.
    pub limit: Option<i64>,
    /// Optional `first_seen_at` upper bound (RFC3339). When set,
    /// only candidates older than this are returned — useful for
    /// "what would the next promote pass delete?" inspection.
    pub before: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize)]
pub struct ChunkGcCandidateView {
    /// Lowercase hex of the chunk's sha256.
    pub content_hash: String,
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize)]
pub struct ChunkGcCandidatesResult {
    pub candidates: Vec<ChunkGcCandidateView>,
}

/// `GET /api/admin/chunk-gc/candidates` — read the candidate
/// table. Paged; default 100 rows, max 10_000. Oldest
/// `first_seen_at` comes first so operators see the leading edge
/// of the promote-pass queue.
pub async fn chunk_gc_candidates(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ChunkGcCandidatesParams>,
) -> Result<Json<ChunkGcCandidatesResult>, ApiError> {
    let limit = params.limit.unwrap_or(100).clamp(1, 10_000);
    let rows = state
        .services
        .meta
        .list_gc_candidates(limit, params.before)
        .await
        .map_err(|e| ApiError::Internal(format!("list_gc_candidates: {e}")))?;
    let candidates = rows
        .into_iter()
        .map(|r| ChunkGcCandidateView {
            content_hash: r.content_hash.iter().map(|b| format!("{b:02x}")).collect(),
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
        })
        .collect();
    Ok(Json(ChunkGcCandidatesResult { candidates }))
}
