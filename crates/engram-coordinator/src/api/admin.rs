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
// ADR 0018 Phase C — session evacuation admin endpoint
// ---------------------------------------------------------------------
//
// Explicit operator + test seam for the evacuation primitive. The
// auto-triggers (dead_host.rs second-stage, nbd_loss_trigger) fire the
// same primitive on heartbeat-loss / NBD-loss; this endpoint exposes
// the alive-source variant for operator drains + e2e tests.

#[derive(serde::Deserialize, Default)]
pub struct EvacuateSessionRequest {
    /// Caller-specified target host. `None` lets the scheduler pick
    /// via `pick_for_session` with `exclude_host = session.host_id`.
    #[serde(default)]
    pub target_host: Option<engram_core::HostId>,
}

#[derive(Serialize)]
pub struct EvacuateSessionResponse {
    pub session_id: SessionId,
    pub new_host_id: engram_core::HostId,
    pub new_sandbox_id: engram_core::SandboxId,
    pub loss: engram_core::types::evacuation::EvacLoss,
}

/// `POST /api/admin/sessions/:id/evacuate` — relocate `session_id`
/// onto a peer host. Only the alive-source path (Active session,
/// reachable backend) is supported here in commit 7; HostLost / Idle
/// sessions return 409 with a pointer to the auto-trigger gate and
/// the deferred /resume-from-Created path.
///
/// Request body: `EvacuateSessionRequest`. Omit `target_host` to let
/// the scheduler pick (always excludes the source host).
///
/// Status codes:
/// - 404 if the session row doesn't exist.
/// - 409 if the session is not Active OR has no bound sandbox OR if
///   the operator-supplied `target_host` is the source host.
/// - 503 if no host can accept the relocate (no capacity or image
///   not ready on any peer).
/// - 200 with `EvacuateSessionResponse` on success. The session is
///   at `Created` on the new host_id with new_sandbox_id bound;
///   start_agent rebuild is the caller's next step (mirrors the
///   resume_from_fc_snapshot dance) or the operator can `/resume`
///   once the resume-from-Created path lands.
pub async fn evacuate_session(
    State(state): State<SharedState>,
    Path(session_id): Path<SessionId>,
    Json(req): Json<EvacuateSessionRequest>,
) -> Result<Json<EvacuateSessionResponse>, ApiError> {
    let session = state.services.meta.get_session(session_id).await?;

    // Alive-source preconditions: session must be Active with a
    // bound sandbox + host.
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
    let Some(source_host) = session.host_id else {
        return Err(ApiError::Conflict(format!(
            "session {session_id} has no bound host",
        )));
    };

    // Resolve target. Operator override wins (with same-host guard).
    // Scheduler fallback picks via the Phase C policy: exclude source
    // host, otherwise capacity-fit. Same image-ready filter as
    // create_session so we never relocate onto an image-cold host.
    let target_host = if let Some(t) = req.target_host {
        if t == source_host {
            return Err(ApiError::Conflict(format!(
                "target_host {t} is the source host — no-op evac",
            )));
        }
        t
    } else {
        let (image_repo, image_tag) = engram_core::types::session::split_image_ref(&session.image);
        // Image-ready filter: only consider hosts that have prefetched
        // the session's image. The session was created with a digest
        // gate; we re-resolve it here from enabled_images so the
        // scheduler can match it against ready_images sets.
        let digest = match state.services.meta.get_enabled_image(&session.image).await {
            Ok(Some(row)) => Some(engram_protocol::heartbeat::ManifestDigest::new(
                row.manifest_digest,
            )),
            // If the image isn't enabled (deleted post-create), fall
            // through with no readiness filter — picking on capacity
            // alone is the operator's best-effort. Same fallback the
            // resume path uses for non-enabled-images.
            _ => None,
        };
        let ctx = crate::host_registry::ScheduleContext {
            repo: image_repo,
            image_version: image_tag,
            prefer_snapshot_id: None,
            memory_mib: None,
            required_image_digest: digest,
            exclude_host: Some(source_host),
        };
        let (picked, _backend) =
            state
                .host_registry
                .pick_for_session(&ctx)
                .map_err(|e| match e {
                    crate::host_registry::PickError::ImageNotReady(d) => {
                        ApiError::Internal(format!("evac: image not ready on any peer: {d}"))
                    }
                    crate::host_registry::PickError::NoCapacity => {
                        ApiError::Internal("evac: no peer host has capacity".into())
                    }
                })?;
        picked
    };

    // Fire the alive-source primitive.
    let receipt = crate::evacuation::evacuate_to(
        &state.host_registry,
        &state.services.meta,
        session_id,
        sandbox_id,
        target_host,
    )
    .await
    .map_err(|e| match e {
        crate::evacuation::EvacError::TargetIsSource { .. }
        | crate::evacuation::EvacError::TargetNotRegistered { .. } => {
            ApiError::Conflict(e.to_string())
        }
        crate::evacuation::EvacError::SourceLookup(_) => ApiError::Conflict(e.to_string()),
        _ => ApiError::Internal(e.to_string()),
    })?;

    tracing::info!(
        %session_id,
        source_host = %source_host,
        new_host = %receipt.new_host_id,
        new_sandbox = %receipt.new_sandbox_id,
        loss = receipt.loss.as_str(),
        "admin evacuate: session relocated to peer; awaiting start_agent finish",
    );

    Ok(Json(EvacuateSessionResponse {
        session_id,
        new_host_id: receipt.new_host_id,
        new_sandbox_id: receipt.new_sandbox_id,
        loss: receipt.loss,
    }))
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
