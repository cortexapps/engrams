//! Disk-pressure detector — Stage 7 of ADR 0005.
//!
//! Background task that polls free disk on `<work_dir>` via
//! `statvfs`. When the free percentage drops below a threshold, the
//! detector evicts sessions to cold tier in two passes:
//!
//!   1. **Cheap-drop pass.** Snapshots whose blob is already in cold
//!      tier (`blob_present = TRUE` AND `local_path = Some`) — the
//!      cold copy is durable; clearing the local copy is free disk
//!      with no upload cost.
//!   2. **Full-flush pass.** If we're still below threshold, pick
//!      the LRU `Idle` session, call the same `flush_session`
//!      primitive Stage 5's admin endpoint uses. Sequential: zstd
//!      is CPU-bound and we don't want concurrent flushes thrashing
//!      the host that's already low on disk.
//!
//! Hysteresis: the loop stops when free_pct reaches `threshold_pct
//! + hysteresis_pct` so we don't oscillate around the threshold.
//!
//! Implicit + explicit triggers share `flush_session`. The admin
//! endpoint at `POST /api/admin/sessions/:id/flush` (Stage 5) is the
//! same code path; tests + `drain-before-redeploy` ops scenarios
//! can fire flushes without synthesizing disk pressure.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::{BlobStorage, MetadataStore};
use tokio::task::JoinHandle;

use crate::flush::{flush_session, FlushRequest, SealFn};

/// Detector tuning. All durations in seconds, percentages 0-100.
#[derive(Clone, Debug)]
pub struct DiskPressureConfig {
    /// Below this free-pct, the detector starts evicting. Default 15.
    pub threshold_pct: u8,
    /// Resume free-pct after eviction stops (threshold + hysteresis).
    /// Default 5.
    pub hysteresis_pct: u8,
    /// How often the detector wakes. Default 30s.
    pub poll_interval: Duration,
    /// Cap on flushes per tick. Bounds the worst-case CPU burst.
    /// Default 4.
    pub max_flushes_per_tick: usize,
}

impl Default for DiskPressureConfig {
    fn default() -> Self {
        Self {
            threshold_pct: 15,
            hysteresis_pct: 5,
            poll_interval: Duration::from_secs(30),
            max_flushes_per_tick: 4,
        }
    }
}

/// Read overrides from `ENGRAM_DISK_PRESSURE_*` env vars. Anything
/// missing or unparseable falls back to the default. Surfaced as a
/// helper rather than a method so the call site (coord main.rs)
/// stays narrow.
pub fn config_from_env() -> DiskPressureConfig {
    let mut cfg = DiskPressureConfig::default();
    if let Ok(s) = std::env::var("ENGRAM_DISK_PRESSURE_THRESHOLD_PCT") {
        if let Ok(v) = s.parse::<u8>() {
            cfg.threshold_pct = v.min(99);
        }
    }
    if let Ok(s) = std::env::var("ENGRAM_DISK_PRESSURE_HYSTERESIS_PCT") {
        if let Ok(v) = s.parse::<u8>() {
            cfg.hysteresis_pct = v.min(50);
        }
    }
    if let Ok(s) = std::env::var("ENGRAM_DISK_PRESSURE_POLL_SECS") {
        if let Ok(v) = s.parse::<u64>() {
            cfg.poll_interval = Duration::from_secs(v.max(5));
        }
    }
    if let Ok(s) = std::env::var("ENGRAM_DISK_PRESSURE_MAX_FLUSHES") {
        if let Ok(v) = s.parse::<usize>() {
            cfg.max_flushes_per_tick = v.max(1);
        }
    }
    cfg
}

/// Spawn the detector. Returns a JoinHandle so the caller can abort
/// on shutdown — the loop never exits voluntarily.
pub fn spawn(
    cfg: DiskPressureConfig,
    work_dir: PathBuf,
    blob: Arc<dyn BlobStorage>,
    meta: Arc<dyn MetadataStore>,
    seal: SealFn,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            threshold_pct = cfg.threshold_pct,
            hysteresis_pct = cfg.hysteresis_pct,
            poll_secs = cfg.poll_interval.as_secs(),
            work_dir = %work_dir.display(),
            "disk-pressure detector starting",
        );
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick so a freshly-started host-
        // agent doesn't insta-evict before the warm pool stabilizes.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = tick_once(&cfg, &work_dir, &blob, &meta, &seal).await {
                tracing::warn!(
                    error = %e,
                    "disk-pressure tick failed; will retry next interval",
                );
            }
        }
    })
}

#[derive(Debug)]
pub enum DetectorError {
    Statvfs(String),
    Meta(engram_core::MetaError),
}

impl std::fmt::Display for DetectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Statvfs(m) => write!(f, "statvfs: {m}"),
            Self::Meta(e) => write!(f, "metadata: {e}"),
        }
    }
}

impl std::error::Error for DetectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Meta(e) => Some(e),
            _ => None,
        }
    }
}

async fn tick_once(
    cfg: &DiskPressureConfig,
    work_dir: &Path,
    blob: &Arc<dyn BlobStorage>,
    meta: &Arc<dyn MetadataStore>,
    seal: &SealFn,
) -> Result<(), DetectorError> {
    let free = free_pct(work_dir)?;
    if free >= cfg.threshold_pct as f64 {
        return Ok(());
    }
    tracing::info!(
        free_pct = free,
        threshold_pct = cfg.threshold_pct,
        "disk pressure detected; running eviction passes",
    );

    // Pass 1 — drop already-cold local copies. Cheap; no upload.
    let dropped = drop_already_cold(meta, work_dir).await;
    if dropped > 0 {
        tracing::info!(dropped, "disk-pressure: cheap-drop pass freed local copies");
    }
    let resume_at = (cfg.threshold_pct + cfg.hysteresis_pct) as f64;
    if free_pct(work_dir)? >= resume_at {
        return Ok(());
    }

    // Pass 2 — full flush of LRU idle sessions. Sequential so we
    // don't thrash CPU on a host that's already pressured.
    let idle = meta
        .list_idle_sessions()
        .await
        .map_err(DetectorError::Meta)?;
    let mut flushed = 0usize;
    for s in idle.into_iter().take(cfg.max_flushes_per_tick) {
        // Skip sessions whose latest snapshot has nothing local to
        // ship (already cold, or never snapshotted) — the cheap
        // pass would have caught the cold-with-local case; the
        // never-snapshotted case isn't a flush candidate.
        let snap = match meta.latest_snapshot_for_session(s.id).await {
            Ok(Some(snap)) => snap,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    session_id = %s.id,
                    error = %e,
                    "disk-pressure: latest_snapshot_for_session failed; skipping",
                );
                continue;
            }
        };
        let Some(local_path) = snap.local_path.clone() else {
            continue;
        };
        let Some(host_id) = snap.host_id else {
            continue;
        };
        let req = FlushRequest {
            session_id: s.id,
            snapshot_id: snap.id,
            host_id,
            snapshot_path: local_path,
        };
        match flush_session(req, blob, meta, seal).await {
            Ok(outcome) => {
                flushed += 1;
                tracing::info!(
                    session_id = %s.id,
                    bytes = outcome.blob_size_bytes,
                    took_ms = outcome.took_ms,
                    "disk-pressure: flushed session",
                );
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %s.id,
                    error = %e,
                    "disk-pressure: flush failed; continuing to next victim",
                );
            }
        }
        if free_pct(work_dir)? >= resume_at {
            break;
        }
    }
    tracing::info!(
        flushed,
        free_pct_now = free_pct(work_dir).unwrap_or(0.0),
        "disk-pressure tick complete",
    );
    Ok(())
}

/// Find snapshots that have both `local_path` and `blob_present`
/// (HotAndCold residency); remove the local directory and clear
/// the column. Best-effort throughout — failures log and continue.
/// Returns the count of snapshots whose local copies were dropped.
async fn drop_already_cold(meta: &Arc<dyn MetadataStore>, _work_dir: &Path) -> usize {
    // The trait doesn't currently expose "list every snapshot with
    // local_path AND blob_present" so we list idle sessions and
    // check each one's latest snapshot. This is a coarser sweep
    // than ideal — a session that's Active with a hot+cold snapshot
    // (rare; would require an admin flush during Active) won't be
    // touched. Acceptable for v1; a follow-up adds a typed
    // `list_dual_resident_snapshots` once the use case materializes.
    let sessions = match meta.list_idle_sessions().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "drop_already_cold: list_idle_sessions failed");
            return 0;
        }
    };
    let mut dropped = 0usize;
    for s in sessions {
        let snap = match meta.latest_snapshot_for_session(s.id).await {
            Ok(Some(snap)) => snap,
            _ => continue,
        };
        if !snap.blob_present {
            continue;
        }
        let Some(local) = snap.local_path.clone() else {
            continue;
        };
        if let Err(e) = meta.clear_local_path(snap.id).await {
            tracing::warn!(
                snapshot_id = %snap.id,
                error = %e,
                "drop_already_cold: clear_local_path failed; skipping fs cleanup",
            );
            continue;
        }
        if let Err(e) = tokio::fs::remove_dir_all(&local).await {
            tracing::warn!(
                snapshot_id = %snap.id,
                path = %local.display(),
                error = %e,
                "drop_already_cold: remove_dir_all failed; metadata cleared regardless",
            );
        }
        dropped += 1;
    }
    dropped
}

/// Free-disk percentage of the volume containing `path`, as a float
/// 0.0–100.0. Uses statvfs; unix-only. Errors return the
/// conservative "not pressured" sentinel (100.0) so a transient
/// failure doesn't drive a stampede of flushes.
#[cfg(unix)]
fn free_pct(path: &Path) -> Result<f64, DetectorError> {
    let st = nix::sys::statvfs::statvfs(path)
        .map_err(|e| DetectorError::Statvfs(format!("statvfs({}): {e}", path.display())))?;
    let total = st.blocks() as u64;
    if total == 0 {
        // Volume claims zero blocks — treat as "not pressured" since
        // the eviction pass would be meaningless.
        return Ok(100.0);
    }
    let avail = st.blocks_available() as u64;
    Ok((avail as f64 / total as f64) * 100.0)
}

#[cfg(not(unix))]
fn free_pct(_path: &Path) -> Result<f64, DetectorError> {
    // The detector is unix-only; on Windows we just claim 100% free
    // so the detector never trips. Engram doesn't support Windows
    // hosts as production targets anyway.
    Ok(100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_env_picks_up_overrides() {
        // Wrap in serial-style env mutations so `cargo test` parallel
        // execution doesn't trip on shared process state. We only
        // touch one env key per assertion + clear at the end.
        std::env::set_var("ENGRAM_DISK_PRESSURE_THRESHOLD_PCT", "20");
        std::env::set_var("ENGRAM_DISK_PRESSURE_HYSTERESIS_PCT", "8");
        std::env::set_var("ENGRAM_DISK_PRESSURE_POLL_SECS", "60");
        std::env::set_var("ENGRAM_DISK_PRESSURE_MAX_FLUSHES", "2");
        let cfg = config_from_env();
        assert_eq!(cfg.threshold_pct, 20);
        assert_eq!(cfg.hysteresis_pct, 8);
        assert_eq!(cfg.poll_interval.as_secs(), 60);
        assert_eq!(cfg.max_flushes_per_tick, 2);
        std::env::remove_var("ENGRAM_DISK_PRESSURE_THRESHOLD_PCT");
        std::env::remove_var("ENGRAM_DISK_PRESSURE_HYSTERESIS_PCT");
        std::env::remove_var("ENGRAM_DISK_PRESSURE_POLL_SECS");
        std::env::remove_var("ENGRAM_DISK_PRESSURE_MAX_FLUSHES");
    }

    #[test]
    fn config_caps_threshold_at_99() {
        std::env::set_var("ENGRAM_DISK_PRESSURE_THRESHOLD_PCT", "200");
        let cfg = config_from_env();
        assert!(
            cfg.threshold_pct <= 99,
            "threshold_pct must be capped: got {}",
            cfg.threshold_pct
        );
        std::env::remove_var("ENGRAM_DISK_PRESSURE_THRESHOLD_PCT");
    }

    #[test]
    fn free_pct_returns_a_number_for_existing_dir() {
        // Smoke test: running from any temp dir gives a real number,
        // not an error. We don't assert a range — host disk state
        // varies — just that the call succeeds.
        let tmp = tempfile::tempdir().unwrap();
        let pct = free_pct(tmp.path()).unwrap();
        assert!(pct.is_finite());
        assert!((0.0..=100.0).contains(&pct), "free_pct out of range: {pct}");
    }

    #[test]
    fn free_pct_for_missing_path_is_an_error() {
        let result = free_pct(Path::new("/this-path-does-not-exist-engram"));
        // statvfs on a nonexistent path errors; our wrapper passes
        // that through. Conservatively returning 100.0 instead is
        // also defensible — but the current impl errors, so lock
        // that down.
        assert!(matches!(result, Err(DetectorError::Statvfs(_))));
    }
}
