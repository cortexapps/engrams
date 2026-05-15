//! Host-side admin RPCs the coord drives via WS (`HostAdminHandler`).
//!
//! ADR 0007 follow-up. The materialize-dir orphan reap is the
//! primitive the coord fans out across every connected host in
//! `--mode=coordinator`; in `--mode=all` the coord just calls the
//! library function directly. Both paths share the same
//! `orphan_reap::reap_materialize_dir` so the wire variant doesn't
//! drift from the in-proc variant.
//!
//! Live-set source: the coord ships
//! `live_disk_manifest_ids` in the RPC body — hosts don't have DB
//! access. This is the same shape the chunk-store GC uses (deduped
//! Vec<Uuid>), so the two reapers stay coherent.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use engram_protocol::admin::HostAdminHandler;
use engram_protocol::wire::WireReapStats;

use crate::orphan_reap;

/// Host-side `HostAdminHandler` backed by an on-disk materialize_dir.
///
/// Construct with the same path the host-agent's PooledBackend uses
/// for `materialize_chunked_rootfs`. The handler is read-only over
/// configuration; per-RPC arguments (min_age, live_set) come from
/// the coord.
#[derive(Clone, Debug)]
pub struct MaterializeDirReaper {
    materialize_dir: PathBuf,
}

impl MaterializeDirReaper {
    pub fn new(materialize_dir: PathBuf) -> Self {
        Self { materialize_dir }
    }
}

#[async_trait]
impl HostAdminHandler for MaterializeDirReaper {
    async fn reap_materialize_dir(
        &self,
        min_age_secs: u64,
        live_disk_manifest_ids: Vec<uuid::Uuid>,
    ) -> Result<WireReapStats, String> {
        let live: HashSet<uuid::Uuid> = live_disk_manifest_ids.into_iter().collect();
        let stats = orphan_reap::reap_materialize_dir(
            &self.materialize_dir,
            &live,
            Duration::from_secs(min_age_secs),
        )
        .await
        .map_err(|e| format!("reap_materialize_dir: {e}"))?;
        Ok(WireReapStats {
            files_scanned: stats.files_scanned,
            files_deleted: stats.files_deleted,
            bytes_freed: stats.bytes_freed,
            files_skipped_unparseable: stats.files_skipped_unparseable,
            files_skipped_too_young: stats.files_skipped_too_young,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn reaper_returns_zeroes_on_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let reaper = MaterializeDirReaper::new(tmp.path().to_path_buf());
        let stats = reaper
            .reap_materialize_dir(0, Vec::new())
            .await
            .expect("empty-dir reap must succeed");
        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.bytes_freed, 0);
    }

    #[tokio::test]
    async fn reaper_sweeps_orphans_with_zero_min_age_and_empty_live_set() {
        let tmp = tempfile::tempdir().unwrap();
        // Plant a parseable orphan: `<uuid>-v1.ext4` shape, no live
        // manifest covers it, zero min_age so the grace window
        // doesn't save it.
        let orphan = tmp.path().join(format!("{}-v1.ext4", uuid::Uuid::new_v4()));
        fs::write(&orphan, vec![0u8; 4096]).unwrap();
        let reaper = MaterializeDirReaper::new(tmp.path().to_path_buf());
        let stats = reaper
            .reap_materialize_dir(0, Vec::new())
            .await
            .expect("reap must succeed");
        assert_eq!(stats.files_scanned, 1);
        assert_eq!(stats.files_deleted, 1);
        assert!(stats.bytes_freed >= 4096);
        assert!(!orphan.exists(), "orphan must be deleted");
    }
}
