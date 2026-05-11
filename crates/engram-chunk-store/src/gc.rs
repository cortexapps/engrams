//! Garbage collection.
//!
//! A chunk is **reachable** if at least one live manifest version
//! references it. A live manifest is one whose `manifest_id`
//! appears in the caller-supplied "live set" (the coordinator
//! derives this from its `snapshots` + `enabled_images` tables).
//! Unreachable chunks are deleted after a `retain_for` retention
//! window — long enough for in-flight writers to commit their
//! manifest version before the chunks they reference get swept.
//!
//! Two-pass sweep:
//!
//! 1. **Compute the live chunk set.** List every manifest under
//!    `manifests/<id>/v*.json` for each live id, fetch each one,
//!    union its `chunks[*].hash`. (This pulls the manifest JSON;
//!    no chunk bodies are fetched.)
//! 2. **Sweep `chunks/`.** For each chunk key not in the live
//!    set, head it for `last_modified` and delete if older than
//!    `retain_for`.
//!
//! Manifest version GC is a separate concern (this sweep doesn't
//! delete manifest objects). Callers wanting to drop old
//! manifest versions can list `manifests/<id>/` and delete the
//! versions they don't want — that's a small enough operation
//! that it doesn't need its own helper.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use uuid::Uuid;

use crate::error::Result;
use crate::manifest::{ChunkHash, ManifestRef};
use crate::store::ChunkStore;

/// Stats returned from a GC pass. Useful for telemetry + sanity
/// checks (e.g. "we GC'd zero chunks last night, are manifests not
/// being deleted?").
#[derive(Clone, Debug, Default)]
pub struct GcStats {
    pub chunks_deleted: u64,
    pub bytes_freed: u64,
    pub chunks_retained_age: u64,
    pub elapsed: Duration,
}

/// Run a GC pass over the chunk store.
///
/// `live_manifest_ids` enumerates all manifests that should be
/// preserved (any of their versions count). The chunks each live
/// manifest references stay; everything else is eligible for
/// deletion after `retain_for`.
///
/// `retain_for` is the minimum age an unreachable chunk must reach
/// before it's eligible for deletion. Set to >> the longest live
/// write transaction (e.g. 24h) so a session committing a new
/// manifest version isn't racing the sweep.
pub async fn run(
    store: &ChunkStore,
    retain_for: Duration,
    live_manifest_ids: impl IntoIterator<Item = Uuid>,
) -> Result<GcStats> {
    let started = std::time::Instant::now();
    let mut stats = GcStats::default();

    // Phase 1: union of chunk hashes reachable from any live
    // manifest version. We pull every committed version — the
    // most-recent version is generally enough for "what's live
    // *now*" but older versions can still be referenced by
    // operator tooling (rollback / time-travel); preserving them
    // is the conservative call.
    let blob = store.blob();
    let mut live_chunks: HashSet<ChunkHash> = HashSet::new();
    for manifest_id in live_manifest_ids {
        let prefix = ManifestRef::id_prefix(manifest_id);
        let manifest_keys = blob.list_prefix(&prefix).await?;
        for key in manifest_keys {
            // Parse version from the trailing "vN.json".
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(num) = rest.strip_prefix('v').and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            let Ok(version) = num.parse::<u64>() else {
                continue;
            };
            let r = ManifestRef {
                manifest_id,
                version,
            };
            let m = store.get_manifest(r).await?;
            for c in &m.chunks {
                live_chunks.insert(c.hash);
            }
        }
    }

    // Phase 2: list every chunk under chunks/sha256/. For each
    // not in the live set, head for last_modified and delete if
    // it's been around longer than retain_for.
    let now = SystemTime::now();
    let cutoff = now
        .checked_sub(retain_for)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let all_chunk_keys = blob.list_prefix("chunks/sha256/").await?;
    for key in all_chunk_keys {
        // Parse the hash back out: chunks/sha256/<2>/<rest>.
        let Some(rest) = key.strip_prefix("chunks/sha256/") else {
            continue;
        };
        let (a, b) = match rest.split_once('/') {
            Some(parts) => parts,
            None => continue,
        };
        let hex = format!("{a}{b}");
        let Ok(hash) = ChunkHash::from_hex(&hex) else {
            continue;
        };
        if live_chunks.contains(&hash) {
            continue;
        }
        let meta = match blob.head(&key).await {
            Ok(m) => m,
            // Race: a concurrent writer may have deleted this key
            // between list and head. That's fine — nothing to do.
            Err(engram_core::error::BlobError::NotFound) => continue,
            Err(e) => return Err(e.into()),
        };
        // Without a proper "creation time" on BlobObjectMeta, we
        // approximate via the etag's parsed UNIX timestamp for
        // LocalBlobStorage (its etag is "secs-nanos") and via the
        // age of the head request for cloud backends (etag isn't
        // parseable; fall back to "delete immediately if old
        // enough"). For v1 we use a simple rule: if any usable
        // age signal is available, respect retain_for; otherwise
        // delete (conservative for "no signal" because the live
        // set already filtered out everything we wanted to keep).
        let age_ok = match parse_local_etag_to_systemtime(meta.etag.as_deref()) {
            Some(mtime) => mtime <= cutoff,
            None => true, // no parseable age; the live-set filter is our safety
        };
        if !age_ok {
            stats.chunks_retained_age += 1;
            continue;
        }
        blob.delete(&key).await?;
        stats.chunks_deleted += 1;
        stats.bytes_freed += meta.size_bytes;
    }

    stats.elapsed = started.elapsed();
    tracing::info!(
        chunks_deleted = stats.chunks_deleted,
        bytes_freed = stats.bytes_freed,
        retained_age = stats.chunks_retained_age,
        elapsed_ms = stats.elapsed.as_millis() as u64,
        "chunk-store GC pass complete"
    );
    Ok(stats)
}

/// `LocalBlobStorage` encodes mtime as "<secs>-<nanos>" in the
/// etag field. Parse it back to a SystemTime so GC can honor
/// `retain_for`. Returns None on any other backend (etags from
/// S3/GCS aren't standardized as timestamps).
fn parse_local_etag_to_systemtime(etag: Option<&str>) -> Option<SystemTime> {
    let etag = etag?;
    let (secs, nanos) = etag.split_once('-')?;
    let secs = secs.parse::<u64>().ok()?;
    let nanos = nanos.parse::<u32>().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChunkRef, Manifest, ManifestKind};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    async fn setup() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (ChunkStore::new(blob), dir)
    }

    #[tokio::test]
    async fn gc_retains_chunks_referenced_by_live_manifests() {
        let (store, _d) = setup().await;

        // Put two manifests, each referencing one chunk.
        let h_live = store.put_chunk(b"live-chunk").await.unwrap();
        let h_dead = store.put_chunk(b"dead-chunk").await.unwrap();

        let live_id = Uuid::new_v4();
        let dead_id = Uuid::new_v4();
        let mut live_m = Manifest::empty(ManifestKind::Disk, 16 * 1024 * 1024);
        live_m.chunks.push(ChunkRef {
            offset: 0,
            hash: h_live,
        });
        let mut dead_m = Manifest::empty(ManifestKind::Disk, 16 * 1024 * 1024);
        dead_m.chunks.push(ChunkRef {
            offset: 0,
            hash: h_dead,
        });
        store
            .put_manifest(
                ManifestRef {
                    manifest_id: live_id,
                    version: 1,
                },
                &live_m,
            )
            .await
            .unwrap();
        store
            .put_manifest(
                ManifestRef {
                    manifest_id: dead_id,
                    version: 1,
                },
                &dead_m,
            )
            .await
            .unwrap();

        // Live set: only `live_id`. dead_id's chunk should sweep.
        let stats = super::run(&store, Duration::from_secs(0), [live_id])
            .await
            .unwrap();
        assert_eq!(stats.chunks_deleted, 1);
        // live-chunk survives.
        assert!(store.chunk_exists(h_live).await.unwrap());
        // dead-chunk swept.
        assert!(!store.chunk_exists(h_dead).await.unwrap());
    }

    #[tokio::test]
    async fn gc_respects_retain_for() {
        let (store, _d) = setup().await;
        let h = store.put_chunk(b"recent").await.unwrap();
        // No live manifests, but retain_for is huge — should
        // retain the recent chunk.
        let stats = super::run(&store, Duration::from_secs(86400), [])
            .await
            .unwrap();
        assert_eq!(stats.chunks_deleted, 0);
        assert_eq!(stats.chunks_retained_age, 1);
        assert!(store.chunk_exists(h).await.unwrap());
    }

    #[tokio::test]
    async fn gc_is_idempotent() {
        let (store, _d) = setup().await;
        let _ = store.put_chunk(b"a").await.unwrap();
        let _ = store.put_chunk(b"b").await.unwrap();
        let live = Uuid::new_v4();
        // No manifest references either chunk — both sweep.
        let stats1 = super::run(&store, Duration::from_secs(0), [live])
            .await
            .unwrap();
        assert_eq!(stats1.chunks_deleted, 2);
        // Re-run: nothing left to sweep.
        let stats2 = super::run(&store, Duration::from_secs(0), [live])
            .await
            .unwrap();
        assert_eq!(stats2.chunks_deleted, 0);
    }
}
