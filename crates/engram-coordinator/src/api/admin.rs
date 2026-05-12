//! Admin endpoints — explicit triggers for primitives whose
//! production driver is implicit (background loops, threshold
//! detectors). ADR 0005 / Stage 5.
//!
//! `flush_session` is the canonical example: in production it fires
//! when the disk-pressure detector (Stage 7) crosses a threshold,
//! but for testability + drain-before-redeploy ops scenarios we
//! expose it explicitly here. Both paths share one code path —
//! `engram_host_agent::flush::flush_session`.
//!
//! All endpoints sit behind the same bearer-auth middleware as the
//! other protected routes.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use chrono::Utc;
use engram_core::types::SessionStatus;
use engram_core::SessionId;
use serde::Serialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Wire shape returned by both admin endpoints. Mirrors
/// `engram_host_agent::flush::FlushOutcome` but keeps the dependency
/// one-way (admin.rs imports from host-agent, not the other way
/// around).
#[derive(Serialize)]
pub struct FlushResult {
    pub session_id: SessionId,
    pub snapshot_id: engram_core::SnapshotId,
    pub blob_size_bytes: u64,
    pub took_ms: u64,
}

impl From<engram_host_agent::flush::FlushOutcome> for FlushResult {
    fn from(o: engram_host_agent::flush::FlushOutcome) -> Self {
        Self {
            session_id: o.session_id,
            snapshot_id: o.snapshot_id,
            blob_size_bytes: o.blob_size_bytes,
            took_ms: o.took_ms,
        }
    }
}

/// `POST /api/admin/sessions/:id/flush`. Forces a cold-tier flush of
/// `:id`'s latest snapshot. Returns 404 if the session is unknown,
/// 409 if it isn't currently `Idle` (no snapshot to flush, or
/// already cold-evicted, or actively running).
pub async fn flush_one(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<FlushResult>, ApiError> {
    let outcome = flush_inner(&state, id).await?;
    Ok(Json(outcome.into()))
}

/// `POST /api/admin/flush-idle`. Enumerate every Idle session, fan
/// out the same primitive in parallel (bounded). Returns one entry
/// per session — success or per-session error. Useful for "drain
/// before redeploy" + Stage 5's integration tests.
pub async fn flush_idle(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state
        .services
        .meta
        .list_idle_sessions()
        .await
        .map_err(|e| ApiError::Internal(format!("list_idle_sessions: {e}")))?;

    // Cap the per-host parallelism to avoid CPU/disk thrash from
    // many concurrent tar+zstd pipelines. 4 is a reasonable default
    // — flushes are CPU-bound on zstd compression; 4 cores' worth
    // is enough headroom on commodity hardware while leaving CPU
    // for the rest of the host's traffic.
    let concurrency = 4;
    let state = state.clone();
    let results = futures::stream::iter(sessions)
        .map(|s| {
            let state = state.clone();
            async move {
                let id = s.id;
                match flush_inner(&state, id).await {
                    Ok(o) => json!({
                        "session_id": id,
                        "ok": true,
                        "outcome": FlushResult::from(o),
                    }),
                    Err(e) => json!({
                        "session_id": id,
                        "ok": false,
                        "error": e.to_string(),
                    }),
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    Ok(Json(json!({ "results": results })))
}

/// Shared core: validate state, look up snapshot, drive the host-
/// agent's flush primitive, emit the SSE event. Returns the typed
/// outcome so both single + bulk endpoints can map it.
async fn flush_inner(
    state: &SharedState,
    id: SessionId,
) -> Result<engram_host_agent::flush::FlushOutcome, ApiError> {
    use engram_host_agent::flush::{flush_session, FlushRequest};

    let session = state.services.meta.get_session(id).await?;
    if session.status != SessionStatus::Idle {
        return Err(ApiError::Conflict(format!(
            "session {id} is not Idle (current: {:?}); flush only applies to Idle snapshots",
            session.status
        )));
    }
    let snap = state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "session {id} has no snapshot to flush (cold-evict already, or never snapshotted)"
            ))
        })?;
    let snapshot_path = snap
        .local_path
        .clone()
        .ok_or_else(|| ApiError::Conflict(format!("snapshot {} has no local_path", snap.id)))?;
    let host_id = snap
        .host_id
        .ok_or_else(|| ApiError::Conflict(format!("snapshot {} has no host_id", snap.id)))?;

    let req = FlushRequest {
        session_id: id,
        snapshot_id: snap.id,
        host_id,
        snapshot_path,
    };

    // Bridge the coord's seal helper into the host-agent's pluggable
    // SealFn. The closure captures a clone of state so the host-
    // agent crate doesn't have to know about SharedState.
    let seal_state = state.clone();
    let seal: engram_host_agent::flush::SealFn = Arc::new(move |url: String| {
        let state = seal_state.clone();
        Box::pin(async move {
            let sealed = crate::blob::seal_blob_ref(&state, &url)
                .await
                .map_err(|e| e.to_string())?;
            Ok(sealed)
        })
    });

    let outcome = flush_session(req, &state.services.blob, &state.services.meta, &seal)
        .await
        .map_err(|e| ApiError::Internal(format!("flush_session: {e}")))?;

    // SSE: surface the cold-evict so dashboard subscribers can
    // re-render the row.
    let _ = state
        .emit(
            id,
            SessionEvent::ColdEvicted {
                snapshot_id: outcome.snapshot_id,
                blob_size_bytes: outcome.blob_size_bytes,
                took_ms: outcome.took_ms,
                at: Utc::now(),
            },
        )
        .await;

    Ok(outcome)
}

// `.buffer_unordered` is on `futures::stream::StreamExt`; the bulk
// endpoint uses it via `futures::stream::iter(...).map(...).buffer_unordered`.
use futures::stream::StreamExt;

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

    let live_ids = state
        .services
        .meta
        .list_live_disk_manifest_ids()
        .await
        .map_err(|e| ApiError::Internal(format!("list_live_disk_manifest_ids: {e}")))?;
    let live_count = live_ids.len();

    tracing::info!(
        live_manifest_count = live_count,
        retain_secs,
        "starting chunk-store GC sweep",
    );

    let stats = engram_chunk_store::gc::run(&state.services.chunk_store, retain_for, live_ids)
        .await
        .map_err(|e| ApiError::Internal(format!("chunk gc: {e}")))?;

    tracing::info!(
        chunks_deleted = stats.chunks_deleted,
        bytes_freed = stats.bytes_freed,
        chunks_retained_age = stats.chunks_retained_age,
        elapsed_ms = stats.elapsed.as_millis() as u64,
        "chunk-store GC sweep complete",
    );

    Ok(Json(GcChunksResult {
        chunks_deleted: stats.chunks_deleted,
        bytes_freed: stats.bytes_freed,
        chunks_retained_age: stats.chunks_retained_age,
        elapsed_ms: stats.elapsed.as_millis() as u64,
        live_manifest_count: live_count,
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
