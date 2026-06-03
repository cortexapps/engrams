//! ADR 0016 Phase C: chunk-GC sweep orchestrator.
//!
//! [`run_one_sweep`] is the work-loop body:
//!
//! 1. Read `chunk_generation` (gen_before).
//! 2. Collect the pin set via [`engram_chunk_store::PinSet::collect`].
//! 3. List `BlobStorage` under `chunks/sha256/`; for each key, parse
//!    back to a `ChunkHash` and check pin-set membership.
//! 4. Unpinned chunks: in `Full` mode, upsert into the
//!    `chunk_gc_candidates` PG table (idempotent, sticky
//!    `first_seen_at`); in `DryRun`, just count.
//! 5. Read `chunk_generation` (gen_after). If it ticked AND we have
//!    restart budget left, restart from step 1 (a flush /
//!    enable_image / record_snapshot raced the sweep; pin set is
//!    stale). Bounded to `max_restart_attempts` to prevent
//!    continuous-flush livelock.
//! 6. **Promote pass** (Full only): list candidates with
//!    `first_seen_at < now() - grace_period`, delete each from
//!    BlobStorage, delete the candidate row. Paged so a single
//!    sweep can't OOM the coord pod.
//!
//! [`gc_sweep_loop`] is the background task: spawned at coord
//! startup, gated on `ENGRAM_CHUNK_GC_ENABLED` (default ON in
//! active-development posture — the 24h grace period is the real
//! safety net), wakes every `ENGRAM_CHUNK_GC_INTERVAL_SECS` and
//! calls `run_one_sweep(Full)`.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_chunk_store::{ChunkHash, ChunkStore, GcError, PinSet};
use engram_core::traits::{BlobStorage, MetadataStore};

use crate::state::SharedState;

/// Sweep configuration. Defaults match the active-development
/// posture: GC enabled, hourly cadence, 24h candidate grace.
#[derive(Clone, Debug)]
pub struct ChunkGcConfig {
    /// Whether `gc_sweep_loop` runs at all. `false` skips the
    /// background loop entirely (admin endpoints still work).
    /// Defaults to `true`. Override via `ENGRAM_CHUNK_GC_ENABLED`
    /// (`0`/`false`/`off` → disable).
    pub enabled: bool,
    /// Interval between background sweeps. Defaults to 3600s (1h).
    /// Override via `ENGRAM_CHUNK_GC_INTERVAL_SECS`.
    pub interval: Duration,
    /// How long a candidate must live in the table before promote-
    /// pass deletes it from BlobStorage. Defaults to 86400s (24h).
    /// Override via `ENGRAM_CHUNK_GC_GRACE_SECS`.
    pub grace_period: Duration,
    /// Max barrier-restart attempts inside a single sweep. Prevents
    /// continuous-flush livelock where every sweep collects a stale
    /// pin set because `chunk_generation` keeps ticking. Defaults
    /// to 3. After exhausting restarts, accept the partial result
    /// — the 24h grace absorbs the racy candidate (next sweep will
    /// either re-pin it or sustain it as candidate).
    pub max_restart_attempts: u32,
    /// Promote-pass batch size — max candidates fetched and
    /// deleted per sweep. Defaults to 10_000.
    pub promote_batch_size: i64,
}

impl Default for ChunkGcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(3600),
            grace_period: Duration::from_secs(86400),
            max_restart_attempts: 3,
            promote_batch_size: 10_000,
        }
    }
}

impl ChunkGcConfig {
    /// Read overrides from env vars. Unset / unparseable values
    /// fall back to defaults.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_ENABLED") {
            cfg.enabled = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "");
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_INTERVAL_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.interval = Duration::from_secs(secs);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_GRACE_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.grace_period = Duration::from_secs(secs);
            }
        }
        cfg
    }
}

/// Whether [`run_one_sweep`] writes anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepMode {
    /// List + classify only. No candidate upserts, no deletes.
    /// Used by `POST /api/admin/chunk-gc/dry-run`.
    DryRun,
    /// Full pipeline: upsert unpinned chunks into
    /// `chunk_gc_candidates`, then run the promote pass.
    Full,
}

/// Outcome of a single sweep. Returned by the admin endpoints +
/// logged at the end of each background-loop tick.
#[derive(Clone, Debug, Default)]
pub struct SweepReport {
    /// How many chunks `list_prefix` returned under
    /// `chunks/sha256/`.
    pub listed_chunks: usize,
    /// Subset of `listed_chunks` whose parse-back to `ChunkHash`
    /// failed. Indicates stray non-chunk keys at the prefix
    /// (shouldn't happen; surfaced for diagnostic visibility).
    pub malformed_keys: usize,
    /// Size of the pin set at the time the sweep completed.
    pub pin_set_size: usize,
    /// Number of unpinned chunks observed. In `Full` mode each
    /// was upserted as a candidate; in `DryRun` this is what
    /// *would* be marked.
    pub candidates_marked: usize,
    /// How many sweep iterations ran inside this call. `>1` means
    /// the chunk_generation barrier triggered restarts.
    pub restart_count: u32,
    /// Whether the sweep hit `max_restart_attempts` and accepted
    /// the partial result anyway. `true` is benign but worth a
    /// warn-log — it signals continuous-flush pressure outpacing
    /// sweep duration.
    pub restart_budget_exhausted: bool,
    /// Promote-pass: candidates whose first_seen_at predated the
    /// grace cutoff and were deleted from BlobStorage + the
    /// candidate table. Always 0 in `DryRun`.
    pub promoted_deletes: usize,
    /// Promote-pass: BlobStorage delete attempts that errored.
    /// The candidate row stays in the table so the next sweep
    /// retries; surfaced for visibility.
    pub promote_delete_errors: usize,
}

/// Convenience wrapper that pulls `meta` / `blob` / `chunk_store`
/// off the shared state. Background loop + admin handlers call this;
/// integration tests in commit 6 call [`run_one_sweep_inner`]
/// directly with their own service handles (no SharedState fixture
/// needed).
pub async fn run_one_sweep(
    state: &SharedState,
    cfg: &ChunkGcConfig,
    mode: SweepMode,
) -> Result<SweepReport, GcError> {
    run_one_sweep_inner(
        state.services.meta.clone(),
        state.services.blob.clone(),
        &state.services.chunk_store,
        cfg,
        mode,
    )
    .await
}

/// Run one sweep cycle against a given set of services. The
/// SharedState-taking [`run_one_sweep`] above wraps this for the
/// production callers; tests bypass the wrapper to build the
/// services they need from a real PG pool + LocalBlobStorage.
pub async fn run_one_sweep_inner(
    meta: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStorage>,
    chunk_store: &ChunkStore,
    cfg: &ChunkGcConfig,
    mode: SweepMode,
) -> Result<SweepReport, GcError> {
    let mut report = SweepReport::default();

    // -------- barrier-bounded classification loop --------
    loop {
        let gen_before = meta.chunk_generation().await?;
        let pin_set = PinSet::collect(meta.as_ref(), chunk_store).await?;

        // Reset per-iteration counters so a restart doesn't double-
        // count from the previous iteration. The barrier means only
        // the LAST iteration's classification is authoritative.
        let keys = blob
            .list_prefix("chunks/sha256/")
            .await
            .map_err(|e| GcError::ChunkStore(engram_chunk_store::ChunkStoreError::Blob(e)))?;
        let listed = keys.len();
        let mut malformed = 0usize;
        let mut candidates_marked = 0usize;

        for key in &keys {
            let Some(hash) = ChunkHash::from_storage_key(key) else {
                malformed += 1;
                continue;
            };
            if pin_set.contains(&hash) {
                continue;
            }
            candidates_marked += 1;
            if mode == SweepMode::Full {
                meta.upsert_chunk_gc_candidate(*hash.as_bytes()).await?;
            }
        }

        // Barrier check: if chunk_generation ticked, our pin set is
        // stale — restart with a fresh collection. Bounded retries
        // prevent continuous-flush livelock.
        let gen_after = meta.chunk_generation().await?;
        if gen_after == gen_before {
            report.listed_chunks = listed;
            report.malformed_keys = malformed;
            report.pin_set_size = pin_set.len();
            report.candidates_marked = candidates_marked;
            break;
        }

        report.restart_count += 1;
        if report.restart_count >= cfg.max_restart_attempts {
            // Accept the partial result. The 24h grace + next
            // sweep's re-classification cover any racy candidates.
            tracing::warn!(
                gen_before,
                gen_after,
                restart_count = report.restart_count,
                "chunk-gc sweep exhausted restart budget; accepting partial result"
            );
            report.restart_budget_exhausted = true;
            report.listed_chunks = listed;
            report.malformed_keys = malformed;
            report.pin_set_size = pin_set.len();
            report.candidates_marked = candidates_marked;
            break;
        }
        tracing::info!(
            gen_before,
            gen_after,
            restart_count = report.restart_count,
            "chunk-gc sweep restarting on barrier bump"
        );
    }

    // -------- promote pass (Full only) --------
    if mode == SweepMode::Full {
        let (deletes, errors) = promote_expired(meta.as_ref(), blob.as_ref(), cfg).await?;
        report.promoted_deletes = deletes;
        report.promote_delete_errors = errors;
    }

    Ok(report)
}

/// Promote-pass: delete candidates older than `grace_period` from
/// BlobStorage + the candidate table. Paged via
/// `cfg.promote_batch_size` so a backlog can't lock the loop.
async fn promote_expired(
    meta: &dyn MetadataStore,
    blob: &dyn BlobStorage,
    cfg: &ChunkGcConfig,
) -> Result<(usize, usize), GcError> {
    let cutoff = Utc::now()
        - chrono::Duration::from_std(cfg.grace_period)
            .unwrap_or_else(|_| chrono::Duration::seconds(86_400));

    let expired = meta
        .list_expired_gc_candidates(cutoff, cfg.promote_batch_size)
        .await?;

    let mut delete_ok: Vec<[u8; 32]> = Vec::with_capacity(expired.len());
    let mut errors = 0usize;
    for hash_bytes in expired {
        // Reconstruct the storage key. We're using the raw bytes
        // from PG, so build a ChunkHash and use its storage_key().
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&hash_bytes);
        let hex: String = arr.iter().map(|b| format!("{b:02x}")).collect();
        let key = format!("chunks/sha256/{}/{}", &hex[..2], &hex[2..]);
        match blob.delete(&key).await {
            Ok(()) => {
                delete_ok.push(hash_bytes);
            }
            Err(e) => {
                tracing::warn!(
                    chunk_hash = %hex,
                    error = %e,
                    "chunk-gc promote: BlobStorage delete failed; candidate row stays for retry"
                );
                errors += 1;
            }
        }
    }

    // Only drop candidate rows for the chunks we actually deleted —
    // failed deletes keep their row so the next sweep retries.
    meta.delete_gc_candidates(&delete_ok).await?;

    Ok((delete_ok.len(), errors))
}

/// Background sweep loop. Spawned from coord startup; ticks every
/// `cfg.interval` and runs a `Full` sweep. Errors log but don't
/// abort the loop — a transient PG or BlobStorage hiccup shouldn't
/// take down GC permanently.
pub async fn gc_sweep_loop(state: SharedState, cfg: ChunkGcConfig) {
    if !cfg.enabled {
        tracing::info!("chunk-gc disabled by config; sweep loop will not run");
        return;
    }
    tracing::info!(
        interval_secs = cfg.interval.as_secs(),
        grace_secs = cfg.grace_period.as_secs(),
        max_restarts = cfg.max_restart_attempts,
        "chunk-gc sweep loop starting"
    );
    let mut ticker = tokio::time::interval(cfg.interval);
    // Skip the immediate tick — let coord finish boot before the
    // first sweep fires.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match run_one_sweep(&state, &cfg, SweepMode::Full).await {
            Ok(report) => {
                tracing::info!(
                    listed = report.listed_chunks,
                    malformed = report.malformed_keys,
                    pinned = report.pin_set_size,
                    candidates = report.candidates_marked,
                    promoted = report.promoted_deletes,
                    promote_errors = report.promote_delete_errors,
                    restart_count = report.restart_count,
                    restart_budget_exhausted = report.restart_budget_exhausted,
                    "chunk-gc sweep done"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "chunk-gc sweep failed; will retry on next interval");
            }
        }
        // ADR 0035 §5: the bundle-generation sweep rides the same tick,
        // barrier, and grace config. Tiny key space (handfuls of
        // generations), so no separate cadence.
        match crate::bundle_gc::run_one_bundle_sweep(
            state.services.meta.clone(),
            state.services.blob.clone(),
            &cfg,
            SweepMode::Full,
        )
        .await
        {
            Ok(report) => {
                tracing::info!(
                    listed = report.listed,
                    pinned = report.pin_set_size,
                    candidates = report.candidates_marked,
                    promoted = report.promoted_deletes,
                    promote_errors = report.promote_delete_errors,
                    restart_count = report.restart_count,
                    "bundle-gc sweep done"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "bundle-gc sweep failed; will retry on next interval");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_gc_on() {
        let cfg = ChunkGcConfig::default();
        assert!(cfg.enabled, "GC defaults ON in active-development posture");
        assert_eq!(cfg.interval, Duration::from_secs(3600));
        assert_eq!(cfg.grace_period, Duration::from_secs(86_400));
        assert_eq!(cfg.max_restart_attempts, 3);
    }

    #[test]
    fn from_env_off_keywords_disable() {
        for v in ["0", "false", "off", ""] {
            // SAFETY: serial test on a process-global env var.
            std::env::set_var("ENGRAM_CHUNK_GC_ENABLED", v);
            let cfg = ChunkGcConfig::from_env();
            assert!(!cfg.enabled, "value {v:?} should disable GC");
        }
        std::env::remove_var("ENGRAM_CHUNK_GC_ENABLED");
    }

    #[test]
    fn from_env_on_or_unset_enables() {
        std::env::remove_var("ENGRAM_CHUNK_GC_ENABLED");
        let cfg = ChunkGcConfig::from_env();
        assert!(cfg.enabled, "unset env var should keep default ON");

        std::env::set_var("ENGRAM_CHUNK_GC_ENABLED", "1");
        let cfg = ChunkGcConfig::from_env();
        assert!(cfg.enabled);
        std::env::remove_var("ENGRAM_CHUNK_GC_ENABLED");
    }

    #[test]
    fn from_env_interval_override() {
        std::env::set_var("ENGRAM_CHUNK_GC_INTERVAL_SECS", "120");
        let cfg = ChunkGcConfig::from_env();
        assert_eq!(cfg.interval, Duration::from_secs(120));
        std::env::remove_var("ENGRAM_CHUNK_GC_INTERVAL_SECS");
    }
}
