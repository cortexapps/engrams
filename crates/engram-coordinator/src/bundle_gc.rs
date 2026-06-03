//! ADR 0035 §5: bundle-generation GC sweep — the blob-storage half of
//! "the bundle path is pinned by GC" (the host-staging half is the
//! host-agent's heartbeat-driven sweep).
//!
//! Mirrors the chunk-GC shape (`chunk_gc.rs`) at a fraction of the
//! scale: bundle generations number in the handfuls, not millions, so
//! there's no pagination pressure and no separate config — the sweep
//! rides the same `gc_sweep_loop` tick, the same `chunk_generation`
//! barrier (it ticks on `record_snapshot`, which is exactly when the
//! bundle pin set changes), and the same candidates-with-grace pattern
//! (`bundle_gc_candidates`, migration 0051).
//!
//! Liveness: `bundles/sha256/<sha>` is pinned iff some
//! `snapshots.aux_bundles` row references `<sha>`. Publish-on-first-
//! reference (the host's `BundleStore::publish`) means everything
//! under the prefix was pinned at upload time; a key becomes garbage
//! only when every snapshot referencing it has been deleted.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use engram_chunk_store::GcError;
use engram_core::traits::{BlobStorage, MetadataStore};

use crate::chunk_gc::{ChunkGcConfig, SweepMode};

const BUNDLE_PREFIX: &str = "bundles/sha256/";

/// Outcome of one bundle sweep, for the loop log + admin dry-run.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BundleSweepReport {
    pub listed: usize,
    pub pin_set_size: usize,
    pub candidates_marked: usize,
    pub promoted_deletes: usize,
    pub promote_delete_errors: usize,
    pub restart_count: u32,
}

/// One bundle-GC sweep. Shares `ChunkGcConfig` (grace / restart
/// budget) with the chunk sweep so operators tune one knob.
pub async fn run_one_bundle_sweep(
    meta: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStorage>,
    cfg: &ChunkGcConfig,
    mode: SweepMode,
) -> Result<BundleSweepReport, GcError> {
    let mut report = BundleSweepReport::default();

    // -------- barrier-bounded classification --------
    loop {
        let gen_before = meta.chunk_generation().await?;
        let pins: HashSet<String> = meta
            .bundle_pin_set()
            .await?
            .into_iter()
            .map(|r| r.sha256)
            .collect();
        let keys = blob
            .list_prefix(BUNDLE_PREFIX)
            .await
            .map_err(|e| GcError::ChunkStore(engram_chunk_store::ChunkStoreError::Blob(e)))?;
        let mut candidates_marked = 0usize;
        for key in &keys {
            let sha = key.trim_start_matches(BUNDLE_PREFIX);
            if pins.contains(sha) {
                continue;
            }
            candidates_marked += 1;
            if mode == SweepMode::Full {
                meta.upsert_bundle_gc_candidate(sha).await?;
            }
        }
        let gen_after = meta.chunk_generation().await?;
        if gen_after == gen_before {
            report.listed = keys.len();
            report.pin_set_size = pins.len();
            report.candidates_marked = candidates_marked;
            break;
        }
        report.restart_count += 1;
        if report.restart_count >= cfg.max_restart_attempts {
            tracing::warn!(
                restart_count = report.restart_count,
                "bundle-gc sweep exhausted restart budget; accepting partial result"
            );
            report.listed = keys.len();
            report.pin_set_size = pins.len();
            report.candidates_marked = candidates_marked;
            break;
        }
    }

    // -------- promote pass (Full only) --------
    if mode == SweepMode::Full {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(cfg.grace_period)
                .unwrap_or_else(|_| chrono::Duration::seconds(86_400));
        let expired = meta
            .list_expired_bundle_gc_candidates(cutoff, cfg.promote_batch_size)
            .await?;
        let mut delete_ok: Vec<String> = Vec::with_capacity(expired.len());
        for sha in expired {
            let key = format!("{BUNDLE_PREFIX}{sha}");
            match blob.delete(&key).await {
                Ok(()) => delete_ok.push(sha),
                Err(e) => {
                    tracing::warn!(
                        sha256 = %sha,
                        error = %e,
                        "bundle-gc promote: BlobStorage delete failed; candidate stays for retry"
                    );
                    report.promote_delete_errors += 1;
                }
            }
        }
        for sha in &delete_ok {
            tracing::info!(sha256 = %sha, "bundle generation deleted (unpinned past grace)");
        }
        meta.delete_bundle_gc_candidates(&delete_ok).await?;
        report.promoted_deletes = delete_ok.len();
    }

    Ok(report)
}
