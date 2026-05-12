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
//! chunk store; `gc_chunks` + `reap_materialize_dir` are the
//! ongoing admin surface.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::error::ApiError;
use crate::state::SharedState;

// ---------------------------------------------------------------------
// ADR 0007 — chunk-store GC
// ---------------------------------------------------------------------

/// Query params for `POST /api/admin/gc-chunks`.
#[derive(serde::Deserialize, Default)]
pub struct GcChunksParams {
    /// Minimum age (seconds) an unreferenced chunk must reach
    /// before it's eligible for deletion. Defaults to 24h — long
    /// enough that a session committing a new manifest version
    /// isn't racing the sweep, short enough that storage cost
    /// catches up reasonably fast.
    ///
    /// Operators bump this when the GC's live-set source is
    /// incomplete (we currently only see manifest_ids referenced
    /// by `snapshots` rows; enabled_images' canonical chunks
    /// would otherwise be eligible for sweep until someone takes
    /// a snapshot using them).
    pub retain_secs: Option<u64>,
}

/// Wire shape of `POST /api/admin/gc-chunks` response.
/// Mirrors `engram_chunk_store::gc::GcStats` with concrete types
/// the JSON serializer can render directly.
#[derive(Serialize)]
pub struct GcChunksResult {
    pub chunks_deleted: u64,
    pub bytes_freed: u64,
    pub chunks_retained_age: u64,
    pub elapsed_ms: u64,
    pub live_manifest_count: usize,
    pub retain_secs: u64,
}

/// `POST /api/admin/gc-chunks` — fire a single GC pass against
/// the chunk store. Matches the project's "explicit-trigger
/// admin endpoints for testability" pattern: there's a future
/// cron alongside, but the admin route is also what tests +
/// drain-before-redeploy ops use directly.
///
/// Returns 500 on internal failures (chunk-store I/O, DB
/// unreachable). Always safe to retry — the sweep is idempotent
/// w.r.t. its own output (deleting an already-deleted chunk is
/// a no-op).
pub async fn gc_chunks(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<GcChunksParams>,
) -> Result<Json<GcChunksResult>, ApiError> {
    let retain_secs = params.retain_secs.unwrap_or(24 * 3600);
    let retain_for = std::time::Duration::from_secs(retain_secs);

    // Delegate to `chunk_gc::run_once` so the admin trigger and the
    // background cron exercise the exact same pipeline — including
    // the disk+memory live-set union (forgetting one would sweep
    // the other side's chunks prematurely).
    let result = crate::chunk_gc::run_once(&state, retain_for)
        .await
        .map_err(|e| ApiError::Internal(format!("chunk gc: {e}")))?;

    Ok(Json(GcChunksResult {
        chunks_deleted: result.stats.chunks_deleted,
        bytes_freed: result.stats.bytes_freed,
        chunks_retained_age: result.stats.chunks_retained_age,
        elapsed_ms: result.stats.elapsed.as_millis() as u64,
        live_manifest_count: result.live_manifest_count,
        retain_secs,
    }))
}

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
    pub materialize_dir: String,
}

/// `POST /api/admin/reap-materialize-dir` — drop materialized
/// `<manifest_id>-vN.ext4` files whose manifest_id is no longer
/// referenced by any live snapshot row. Pairs with the chunk-
/// store GC endpoint above: that one sweeps chunks; this one
/// sweeps the assembled files derived from chunks.
///
/// Returns 409 when running in `--mode=coordinator`: the
/// materialize dir lives on each host's local disk, and the
/// multi-host fanout RPC isn't wired yet (tracked in the rollout
/// doc as a follow-up).
pub async fn reap_materialize_dir(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<ReapMaterializeDirParams>,
) -> Result<Json<ReapMaterializeDirResult>, ApiError> {
    let materialize_dir = match state.services.materialize_dir.as_ref() {
        Some(dir) => dir.clone(),
        None => {
            return Err(ApiError::Conflict(
                "reap-materialize-dir is `--mode=all`-only; multi-host fanout is \
                 a follow-up (see docs/chunked-storage-rollout.md)"
                    .into(),
            ));
        }
    };
    let min_age_secs = params.min_age_secs.unwrap_or(3600);
    let min_age = std::time::Duration::from_secs(min_age_secs);

    // Live-set source = the same DB query the chunk GC uses. The
    // two reapers stay coherent: a manifest that survives the
    // chunk sweep also keeps its materialized files, and
    // vice-versa.
    let live_ids: std::collections::HashSet<uuid::Uuid> = state
        .services
        .meta
        .list_live_disk_manifest_ids()
        .await
        .map_err(|e| ApiError::Internal(format!("list_live_disk_manifest_ids: {e}")))?
        .into_iter()
        .collect();
    let live_manifest_count = live_ids.len();

    tracing::info!(
        materialize_dir = %materialize_dir.display(),
        live_manifest_count,
        min_age_secs,
        "starting materialize-dir orphan reap",
    );

    let stats =
        engram_host_agent::orphan_reap::reap_materialize_dir(&materialize_dir, &live_ids, min_age)
            .await
            .map_err(|e| ApiError::Internal(format!("reap_materialize_dir: {e}")))?;

    tracing::info!(
        files_scanned = stats.files_scanned,
        files_deleted = stats.files_deleted,
        bytes_freed = stats.bytes_freed,
        "materialize-dir orphan reap complete",
    );

    Ok(Json(ReapMaterializeDirResult {
        files_scanned: stats.files_scanned,
        files_deleted: stats.files_deleted,
        bytes_freed: stats.bytes_freed,
        files_skipped_unparseable: stats.files_skipped_unparseable,
        files_skipped_too_young: stats.files_skipped_too_young,
        live_manifest_count,
        min_age_secs,
        materialize_dir: materialize_dir.display().to_string(),
    }))
}
