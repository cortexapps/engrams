//! ADR 0007: materialize-dir orphan reaper.
//!
//! `PooledBackend::materialize_chunked_rootfs` writes
//! `<work_dir>/chunked-rootfs/<manifest_id>-vN.ext4` files
//! permanently. Over time, as image versions roll, the directory
//! accumulates files for manifests no longer referenced by any
//! live row. The local NVMe `ChunkCache` LRU bounds *chunk* growth
//! but not these assembled files — each is a multi-GiB ext4.
//!
//! Reap rule: parse `manifest_id` out of the filename, drop the
//! file if it's not in the caller-supplied `live_set`. Same
//! live-set source as the chunk-store GC (`MetadataStore::list_live_disk_manifest_ids`)
//! so the two reapers stay coherent.
//!
//! Concurrency: we don't lock. VZ's per-sandbox APFS clonefile
//! completes in ~50 ms, and the materialized file's mtime is
//! within seconds of the clone. The reaper additionally requires
//! a `min_age` gate so a freshly-materialized file in use by an
//! in-flight `create()` isn't ripped out from under it.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use uuid::Uuid;

/// Outcome of one reap pass. Useful for the admin endpoint's
/// response + future telemetry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReapStats {
    pub files_scanned: u64,
    pub files_deleted: u64,
    pub bytes_freed: u64,
    /// Files in `materialize_dir` whose name didn't match the
    /// `<uuid>-v<num>.ext4` shape. Logged + skipped — operators
    /// can place sidecar files (e.g. README) without tripping
    /// the reaper.
    pub files_skipped_unparseable: u64,
    /// Files newer than `min_age`. Not deleted; might still be
    /// in active use by an in-flight `create()`.
    pub files_skipped_too_young: u64,
}

/// Run one reap pass over `materialize_dir`. Async because the
/// underlying `fs::read_dir` is. `live_set` is the set of
/// manifest_ids that must be preserved.
///
/// Files NOT eligible for deletion:
/// - Filename doesn't parse to `<uuid>-v<num>.ext4`.
/// - mtime is within `min_age` of now.
/// - manifest_id is in `live_set`.
///
/// The combination is deliberate: even a long-orphaned file
/// (live_set says delete) gets a grace period (min_age), so a
/// racing materialize finishing the last-modified write isn't
/// ripped.
pub async fn reap_materialize_dir(
    materialize_dir: &Path,
    live_set: &HashSet<Uuid>,
    min_age: Duration,
) -> std::io::Result<ReapStats> {
    let mut stats = ReapStats::default();

    let mut rd = match tokio::fs::read_dir(materialize_dir).await {
        Ok(r) => r,
        // Missing dir = nothing to do. Surfaces cleanly when the
        // coord runs the admin endpoint on a fresh deployment
        // before any session ever materialized.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
        Err(e) => return Err(e),
    };
    let now = SystemTime::now();
    while let Some(entry) = rd.next_entry().await? {
        stats.files_scanned += 1;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            stats.files_skipped_unparseable += 1;
            continue;
        };

        let Some(manifest_id) = parse_manifest_id(name) else {
            stats.files_skipped_unparseable += 1;
            tracing::debug!(file = %name, "reap: unparseable filename; skipping");
            continue;
        };

        if live_set.contains(&manifest_id) {
            // Live manifest. Keep it — even though we could
            // re-materialize from chunks cheaply, keeping the
            // assembled file saves us the assembly cost on the
            // next session for this image.
            continue;
        }

        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(file = %name, error = %e, "reap: stat failed; skipping");
                continue;
            }
        };
        let mtime = meta.modified().unwrap_or(now);
        if now
            .duration_since(mtime)
            .map(|age| age < min_age)
            .unwrap_or(false)
        {
            stats.files_skipped_too_young += 1;
            continue;
        }

        let size = meta.len();
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                stats.files_deleted += 1;
                stats.bytes_freed += size;
                tracing::debug!(file = %name, size, "reap: deleted orphan");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Raced with another reaper (or the operator).
                // Not an error; another caller already won.
            }
            Err(e) => {
                tracing::warn!(file = %name, error = %e, "reap: delete failed");
            }
        }
    }
    Ok(stats)
}

/// Parse `<uuid>-v<num>.ext4` → manifest_id. Returns None for any
/// filename that doesn't match (sidecar files, partial uploads,
/// operator scratch).
fn parse_manifest_id(name: &str) -> Option<Uuid> {
    let stem = name.strip_suffix(".ext4")?;
    let (uuid_part, version_part) = stem.rsplit_once("-v")?;
    // Both halves must be present + the version must parse as
    // u64 (we don't use it but reject malformed names anyway).
    if version_part.is_empty() || version_part.parse::<u64>().is_err() {
        return None;
    }
    Uuid::parse_str(uuid_part).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn reap_missing_dir_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let stats = reap_materialize_dir(&missing, &HashSet::new(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(stats, ReapStats::default());
    }

    #[tokio::test]
    async fn reap_keeps_live_manifests_and_drops_orphans() {
        // `min_age = ZERO` lets the test stay portable — no need
        // to fiddle with mtime via a `filetime` dep just to age
        // a file. The min_age gate has its own focused test below.
        let tmp = tempfile::tempdir().unwrap();
        let live_id = Uuid::new_v4();
        let dead_id = Uuid::new_v4();

        let live_path = tmp.path().join(format!("{live_id}-v1.ext4"));
        let dead_path = tmp.path().join(format!("{dead_id}-v1.ext4"));
        tokio::fs::write(&live_path, b"live").await.unwrap();
        tokio::fs::write(&dead_path, b"dead-content").await.unwrap();

        let mut live_set = HashSet::new();
        live_set.insert(live_id);

        let stats = reap_materialize_dir(tmp.path(), &live_set, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(stats.files_scanned, 2);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.bytes_freed, b"dead-content".len() as u64);
        assert!(live_path.exists(), "live manifest must survive");
        assert!(
            !dead_path.exists(),
            "orphan manifest must be removed from disk"
        );
    }

    #[tokio::test]
    async fn reap_skips_files_younger_than_min_age() {
        let tmp = tempfile::tempdir().unwrap();
        let dead_id = Uuid::new_v4();
        let dead_path = tmp.path().join(format!("{dead_id}-v1.ext4"));
        tokio::fs::write(&dead_path, b"young-orphan").await.unwrap();

        // Default mtime is "now"; min_age=1h gate keeps it.
        let stats = reap_materialize_dir(tmp.path(), &HashSet::new(), Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.files_skipped_too_young, 1);
        assert!(dead_path.exists());
    }

    #[tokio::test]
    async fn reap_skips_unparseable_names() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("README.md"), b"hi")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("bogus-v1.ext4"), b"x")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("nouuid.ext4"), b"y")
            .await
            .unwrap();

        let stats = reap_materialize_dir(tmp.path(), &HashSet::new(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(stats.files_scanned, 3);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.files_skipped_unparseable, 3);
    }

    #[test]
    fn parse_round_trips_well_formed_names() {
        let id = Uuid::new_v4();
        let name = format!("{id}-v17.ext4");
        assert_eq!(parse_manifest_id(&name), Some(id));
    }

    #[test]
    fn parse_rejects_malformed_names() {
        assert!(parse_manifest_id("nope").is_none());
        assert!(parse_manifest_id("notauuid-v1.ext4").is_none());
        assert!(parse_manifest_id("00000000-0000-0000-0000-000000000000-vNaN.ext4").is_none());
        assert!(parse_manifest_id("00000000-0000-0000-0000-000000000000-v1.bin").is_none());
        // Missing version part.
        assert!(parse_manifest_id("00000000-0000-0000-0000-000000000000.ext4").is_none());
    }
}
