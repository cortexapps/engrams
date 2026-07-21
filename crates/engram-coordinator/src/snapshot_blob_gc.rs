//! ADR 0028 addendum: portable snapshot-blob GC sweep — the durability
//! half of "snapshot blobs are pinned by GC."
//!
//! Brings `snapshots/<id>/{state.bin,sidecar.json}`
//! under the same pin-set + grace + barrier model as chunks
//! (`chunk_gc`) and bundles (`bundle_gc`). This replaces the host's
//! inline `abort_prior_inflight_snapshot` blob deletion, which deleted
//! these blobs with no PG check — so a concurrent producer (a periodic
//! checkpoint vs an eviction sharing the one per-sandbox inflight slot)
//! could delete a snapshot whose row was already recorded
//! `recoverable = true`, bricking resume.
//!
//! Liveness: `snapshots/<id>/...` is pinned iff a row with `<id>` exists
//! in `snapshots` (`snapshot_blob_pin_set` = `SELECT id FROM snapshots`,
//! NO `recoverable`/`session_id` filter — see the trait doc). Snapshot
//! ids are fresh UUIDs, never reused, so the ONLY "unpinned → pinned"
//! transition is the upload-before-record window every capture has (the
//! host uploads the blob, then the coord records the row). Unlike
//! bundles, that window is on the normal path of every snapshot, so the
//! promote pass RE-VERIFIES the pin set at delete time: a candidate
//! whose row has since landed is dropped from the table, never deleted.
//! Re-pin can only mean "the row was recorded after we marked it" — it
//! cannot be a recycled id — so the re-check is exact.

use std::collections::HashSet;
use std::sync::Arc;

use engram_chunk_store::GcError;
use engram_core::traits::{BlobStorage, MetadataStore};
use engram_core::types::SnapshotId;

use crate::chunk_gc::{ChunkGcConfig, SweepMode};

const SNAPSHOT_PREFIX: &str = "snapshots/";

/// Outcome of one sweep, for the loop log + admin dry-run.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SnapshotBlobSweepReport {
    pub listed: usize,
    pub malformed: usize,
    pub pin_set_size: usize,
    pub candidates_marked: usize,
    pub promoted_deletes: usize,
    /// Candidates that became re-pinned (row recorded) between marking
    /// and promotion — cleared from the table, blobs kept.
    pub promote_repinned_skips: usize,
    pub promote_delete_errors: usize,
    pub restart_count: u32,
}

/// Parse the snapshot id out of a `snapshots/<id>/<suffix>` blob key.
/// `None` for keys that don't match the layout (counted `malformed` and
/// never touched).
fn snapshot_id_from_key(key: &str) -> Option<SnapshotId> {
    let rest = key.strip_prefix(SNAPSHOT_PREFIX)?;
    let id_seg = rest.split('/').next()?;
    id_seg.parse::<SnapshotId>().ok()
}

/// One snapshot-blob GC sweep. Shares `ChunkGcConfig` (grace / restart
/// budget) with the chunk + bundle sweeps so operators tune one knob.
///
/// `clock` is the sweep's time source (ADR 0098 D1: time is an
/// injected input). The promote cutoff reads it FRESH after the mark
/// pass: candidates are stamped `first_seen_at DEFAULT now()` by PG
/// during this same call, so a cutoff captured at sweep start would
/// never see a same-sweep candidate as expired under zero grace.
pub async fn run_one_snapshot_blob_sweep(
    meta: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStorage>,
    cfg: &ChunkGcConfig,
    mode: SweepMode,
    clock: &Arc<dyn engram_core::traits::Clock>,
) -> Result<SnapshotBlobSweepReport, GcError> {
    let mut report = SnapshotBlobSweepReport::default();

    // -------- barrier-bounded classification --------
    loop {
        let gen_before = meta.chunk_generation().await?;
        let mut pins: HashSet<SnapshotId> =
            meta.snapshot_blob_pin_set().await?.into_iter().collect();
        // ADR 0084 §B6: a cold base's own Full snapshot has no
        // `snapshots` row (only the warm overlay it seeds gets one) —
        // its state.bin/sidecar blobs would otherwise look unpinned to
        // this sweep.
        pins.extend(meta.cold_base_snapshot_ids().await?);
        let keys = blob
            .list_prefix(SNAPSHOT_PREFIX)
            .await
            .map_err(|e| GcError::ChunkStore(engram_chunk_store::ChunkStoreError::Blob(e)))?;

        // Dedup the ≤2 keys per snapshot (state.bin / sidecar.json)
        // down to one id so each candidate is upserted once.
        let mut malformed = 0usize;
        let mut unpinned: HashSet<SnapshotId> = HashSet::new();
        for key in &keys {
            match snapshot_id_from_key(key) {
                Some(id) if !pins.contains(&id) => {
                    unpinned.insert(id);
                }
                Some(_) => {} // pinned — leave it
                None => malformed += 1,
            }
        }
        let mut candidates_marked = 0usize;
        if mode == SweepMode::Full {
            for id in &unpinned {
                meta.upsert_snapshot_blob_gc_candidate(*id).await?;
                candidates_marked += 1;
            }
        }

        let gen_after = meta.chunk_generation().await?;
        if gen_after == gen_before {
            report.listed = keys.len();
            report.malformed = malformed;
            report.pin_set_size = pins.len();
            report.candidates_marked = candidates_marked;
            break;
        }
        report.restart_count += 1;
        if report.restart_count >= cfg.max_restart_attempts {
            tracing::warn!(
                restart_count = report.restart_count,
                "snapshot-blob-gc sweep exhausted restart budget; accepting partial result"
            );
            report.listed = keys.len();
            report.malformed = malformed;
            report.pin_set_size = pins.len();
            report.candidates_marked = candidates_marked;
            break;
        }
    }

    // -------- promote pass (Full only) --------
    if mode == SweepMode::Full {
        let cutoff = clock.now_utc()
            - chrono::Duration::from_std(cfg.grace_period)
                .unwrap_or_else(|_| chrono::Duration::seconds(86_400));
        let expired = meta
            .list_expired_snapshot_blob_gc_candidates(cutoff, cfg.promote_batch_size)
            .await?;

        // Re-verify the pin set at delete time — the load-bearing
        // safety step for the upload-before-record window (see the
        // module doc). A candidate whose row has since landed is dropped
        // from the candidate table (its blob stays, pinned); a still-
        // unpinned candidate has its blobs deleted.
        let mut pins: HashSet<SnapshotId> =
            meta.snapshot_blob_pin_set().await?.into_iter().collect();
        // ADR 0084 §B6: a cold base's own Full snapshot has no
        // `snapshots` row (only the warm overlay it seeds gets one) —
        // its state.bin/sidecar blobs would otherwise look unpinned to
        // this sweep.
        pins.extend(meta.cold_base_snapshot_ids().await?);
        let mut resolved: Vec<SnapshotId> = Vec::with_capacity(expired.len());
        for id in expired {
            if pins.contains(&id) {
                report.promote_repinned_skips += 1;
                resolved.push(id);
                continue;
            }
            // delete is idempotent on missing keys (both GCS + Local
            // return Ok), so deleting both is safe even though
            // memory-less / non-FC snapshots only ever uploaded a
            // subset. A real (non-NotFound) error keeps the candidate
            // row for the next sweep to retry.
            let mut all_ok = true;
            for key in [
                engram_chunk_store::snapshot_blob::state_blob_key(id),
                engram_chunk_store::snapshot_blob::sidecar_blob_key(id),
            ] {
                if let Err(e) = blob.delete(&key).await {
                    tracing::warn!(
                        snapshot_id = %id,
                        key = %key,
                        error = %e,
                        "snapshot-blob-gc promote: delete failed; candidate stays for retry"
                    );
                    report.promote_delete_errors += 1;
                    all_ok = false;
                }
            }
            if all_ok {
                tracing::info!(snapshot_id = %id, "snapshot blobs deleted (unpinned past grace)");
                resolved.push(id);
                report.promoted_deletes += 1;
            }
        }
        meta.delete_snapshot_blob_gc_candidates(&resolved).await?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_id_from_each_suffix() {
        let id = SnapshotId::new();
        for suffix in ["state.bin", "sidecar.json"] {
            let key = format!("snapshots/{id}/{suffix}");
            assert_eq!(snapshot_id_from_key(&key), Some(id), "key={key}");
        }
    }

    #[test]
    fn rejects_malformed_keys() {
        assert_eq!(snapshot_id_from_key("snapshots/not-a-uuid/state.bin"), None);
        assert_eq!(snapshot_id_from_key("chunks/sha256/ab/cd"), None);
        assert_eq!(snapshot_id_from_key("snapshots/"), None);
    }
}
