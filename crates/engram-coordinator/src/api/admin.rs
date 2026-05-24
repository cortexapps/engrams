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
