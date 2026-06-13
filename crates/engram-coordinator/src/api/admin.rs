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
        return Ok(Json(FlushNowResult {
            outcome: FlushNowOutcome::Idle,
            manifest_version: None,
        }));
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
            Ok(Json(FlushNowResult {
                outcome: FlushNowOutcome::Applied,
                manifest_version: Some(manifest_ref.version),
            }))
        }
        engram_core::traits::UpdateOutcome::DroppedStale => {
            tracing::warn!(
                %session_id,
                %sandbox_id,
                manifest_id = %manifest_ref.manifest_id,
                manifest_version = manifest_ref.version,
                "admin flush_now: stale (sandbox_id mismatch on UPDATE)",
            );
            Ok(Json(FlushNowResult {
                outcome: FlushNowOutcome::Stale,
                manifest_version: None,
            }))
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
}

/// `POST /api/admin/sessions/:id/evacuate` — mark the session
/// `Evacuating`. Pre: Active session with a bound sandbox. Post: the
/// source sandbox is paused, flushed, snapshotted, and destroyed;
/// PG row is at `Evacuating`; `evac_resumer` will resume on a peer.
///
/// Returns 202 Accepted; the scanner is the actual deliverable. Use
/// the session events stream to observe the resume completing.
pub async fn evacuate_session(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
    Json(_req): Json<EvacuateSessionRequest>,
) -> Result<(StatusCode, Json<EvacuateSessionResponse>), ApiError> {
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
    // Evacuating`. Same pause → flush → memory-snapshot → destroy →
    // PG transition the legacy `evict_idle_session` uses for Idle
    // suspends — the *only* difference is the terminal state, so
    // both flows inherit the same recoverability invariants (snapshot
    // durable in BlobStorage before destroy, PG state flips before
    // host-side destroy).
    crate::idle_evictor::evict_session_to_state(
        &state,
        session_id,
        sandbox_id,
        engram_core::types::SessionState::Evacuating,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("evac pipeline: {e}")))?;

    tracing::info!(
        %session_id,
        %sandbox_id,
        "admin evacuate: session marked Evacuating; scanner will resume on peer",
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(EvacuateSessionResponse {
            session_id,
            status: "evacuating",
        }),
    ))
}

#[derive(Serialize)]
pub struct EvictIdleResponse {
    pub session_id: SessionId,
    /// "idle" — the session was paused, flushed, snapshotted, and its
    /// local sandbox destroyed; the PG row is at `Idle` and a
    /// subsequent `POST /sessions/:id/resume` rebinds it.
    pub status: &'static str,
}

/// `POST /api/admin/sessions/:id/evict-idle` — the explicit admin
/// trigger for the idle-eviction pipeline. Fires the exact same
/// primitive (`idle_evictor::evict_idle_session`) the host-side idle
/// detector and the coord `idle_detect_backstop` scanner drive on a
/// timeout, so it's a faithful stand-in for "the session went idle" —
/// without waiting out (or globally lowering) the idle TTL. Pre:
/// Active session with a bound sandbox. Post: session at `Idle`,
/// memory snapshot durable in BlobStorage, resumable.
///
/// Synchronous (unlike `evacuate`, which hands off to the resumer
/// scanner): the pipeline runs inline and the session is `Idle` by the
/// time this returns 200.
pub async fn evict_idle(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
) -> Result<Json<EvictIdleResponse>, ApiError> {
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

    crate::idle_evictor::evict_idle_session(&state, session_id, sandbox_id)
        .await
        .map_err(|e| ApiError::Internal(format!("idle-evict pipeline: {e}")))?;

    tracing::info!(
        %session_id,
        %sandbox_id,
        "admin evict-idle: session suspended to Idle via the idle-eviction primitive",
    );

    Ok(Json(EvictIdleResponse {
        session_id,
        status: "idle",
    }))
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
/// marked `Evacuating` with the target pinned on the session row,
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
    crate::placement::pick_specific_host(
        state.services.meta.as_ref(),
        &state.host_registry,
        req.target_host_id,
        session.host_id,
    )
    .await
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

    // Pin the destination (ADR 0047: durably, on the session row — the
    // scanner on ANY replica honors it), then fire the same evac
    // pipeline `evacuate` uses. Unwind the pin if the pipeline fails.
    state
        .services
        .meta
        .set_teleport_target(session_id, Some(req.target_host_id))
        .await
        .map_err(|e| ApiError::Internal(format!("teleport: pin target: {e}")))?;
    if let Err(e) = crate::idle_evictor::evict_session_to_state(
        &state,
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
    /// Σ allocatable_mib over schedulable hosts — lets the caller derive the
    /// average per-host capacity (how much one node adds).
    pub total_mib: u64,
    /// ADR 0048: Σ spare vCPU (`Σ cpu_budget − reserved`) over schedulable
    /// hosts, and the total CPU budget — the CPU dimension of scale-up.
    pub free_vcpus: u64,
    pub total_vcpus: u64,
    /// ADR 0048: hosts cordoned for scale-down (operator visibility; the
    /// wave driver also reads this to know its in-flight footprint).
    pub cordoned_hosts: u32,
    /// ADR 0048: the queue. `queued_*` is the demand the operator scales
    /// UP to fit; scale-DOWN is hard-gated on `queued_sessions == 0`.
    pub queued_sessions: u64,
    pub queued_mib: u64,
    pub queued_vcpus: u64,
}

/// `GET /api/admin/fleet/demand` — the autoscaler's input. ADR 0047/0048:
/// counts, headroom (both dims), the cordoned footprint, AND the queue all
/// come from PG, so every coordinator replica reports identical demand.
pub async fn fleet_demand(State(state): State<SharedState>) -> Json<FleetDemandResponse> {
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
    Json(FleetDemandResponse {
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
/// ADR 0047: writes the coordinator-owned `hosts.cordoned` bit, which
/// heartbeats never touch — the cordon is DURABLE until an explicit
/// uncordon, across coord restarts and replicas. A PG failure is a 500
/// (durability is the point; there is no in-memory fallback to lie
/// behind). Succeeds for a host that has a row but no live connection
/// on this pod (a wave-cordon during pod churn must stick). No effect
/// on already-bound sessions — for that, the operator calls `/drain`.
pub async fn cordon_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<Json<CordonResponse>, ApiError> {
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
    Ok(Json(CordonResponse {
        host_id,
        status: "cordoned",
    }))
}

/// `POST /api/admin/hosts/:id/uncordon` — inverse of cordon. The
/// host returns to the picker's view immediately.
pub async fn uncordon_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<Json<CordonResponse>, ApiError> {
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
pub async fn drain_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<(StatusCode, Json<DrainHostResponse>), ApiError> {
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
        return Ok((
            StatusCode::ACCEPTED,
            Json(DrainHostResponse {
                host_id,
                evacuating: Vec::new(),
                failures: Vec::new(),
            }),
        ));
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

    Ok((
        StatusCode::ACCEPTED,
        Json(DrainHostResponse {
            host_id,
            evacuating,
            failures,
        }),
    ))
}

/// `DELETE /api/admin/hosts/:id` (ADR 0048) — deregister a drained host
/// immediately, so the operator's scale-down doesn't wait ~30-40s for the
/// dead-host detector. 409 if any session is still bound (the operator
/// must finish draining first); 200 + idempotent if the row is already
/// gone. Also drops the in-memory registry entry + gRPC pool channel.
pub async fn delete_host(
    State(state): State<SharedState>,
    Path(host_id): Path<engram_core::HostId>,
) -> Result<StatusCode, ApiError> {
    use engram_core::types::session::DeleteHostOutcome;
    match state.services.meta.delete_host(host_id).await {
        Ok(DeleteHostOutcome::Deleted) => {
            // Drop the in-memory routing entry (best-effort; a sibling
            // replica clears its own on the host_dead notify / TTL).
            state.host_registry.unregister(host_id);
            tracing::info!(%host_id, "admin: host deregistered (row deleted)");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(DeleteHostOutcome::SessionsBound(n)) => Err(ApiError::Conflict(format!(
            "host {host_id} still has {n} bound session(s); drain it before deleting"
        ))),
        Err(e) => Err(ApiError::Internal(format!("delete_host: {e}"))),
    }
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
