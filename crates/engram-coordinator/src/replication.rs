//! Snapshot replication driver.
//!
//! Phase 2 follow-up: snapshots taken on a host land on local disk
//! first (`snapshots.local_path = Some(...)`, `blob_url = NULL`). This
//! driver polls Postgres for those rows, tars + zstd-3-compresses the
//! directory, streams it to BlobStorage, and stamps `blob_url` +
//! `replicated_at` on the row. Once a snapshot is replicated the
//! host's local disk can be evicted under LRU pressure (the LRU side
//! lives in `engram-host-agent::snapshot::SnapshotManager`).
//!
//! Coordinator-side rather than host-side for now — works for any
//! deployment where the coordinator can read the host's snapshot
//! directory (`--mode=all`, or coord + host on a shared filesystem).
//! True multi-machine deployments will need a host-side
//! `UploadSnapshot` RPC that pushes the bytes from the host instead;
//! tracked under Phase 4 / Phase 6 production hardening.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_core::traits::{BlobStorage, ByteStream, MetadataStore};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::SnapshotId;

#[derive(Clone, Debug)]
pub struct ReplicationConfig {
    /// How often to poll for pending replications. 30s keeps the
    /// driver lazy enough to coexist with bursty snapshot creation
    /// without overwhelming Postgres or BlobStorage.
    pub poll_interval: Duration,
    /// Cap how many pending snapshots one tick replicates. Prevents
    /// a backlog from monopolising the driver when something else
    /// (e.g. a CI run) just took 100 snapshots in a row.
    pub batch_size: i64,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            batch_size: 8,
        }
    }
}

/// Spawn the driver as a background task. Returns immediately; the
/// task lives for the coordinator's lifetime. Errors per-tick are
/// logged but don't stop the loop — a transient BlobStorage outage
/// retries on the next tick.
pub fn spawn(
    cfg: ReplicationConfig,
    blob: Arc<dyn BlobStorage>,
    meta: Arc<dyn MetadataStore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick — `interval` fires once at t=0
        // by default; we want the first replication to happen one
        // poll-interval after startup.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &blob, &meta).await {
                tracing::warn!(error = %e, "replication tick failed; will retry");
            }
        }
    })
}

async fn run_once(
    cfg: &ReplicationConfig,
    blob: &Arc<dyn BlobStorage>,
    meta: &Arc<dyn MetadataStore>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pending = meta.list_pending_replications(cfg.batch_size).await?;
    if pending.is_empty() {
        return Ok(());
    }
    tracing::debug!(count = pending.len(), "replicating pending snapshots");
    for snap in pending {
        if let Err(e) = replicate_one(blob, meta, &snap).await {
            tracing::warn!(
                snapshot_id = %snap.id,
                error = %e,
                "replicate one failed; will retry next tick",
            );
        }
    }
    Ok(())
}

async fn replicate_one(
    blob: &Arc<dyn BlobStorage>,
    meta: &Arc<dyn MetadataStore>,
    snap: &SnapshotRecord,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let local_path = snap
        .local_path
        .as_ref()
        .ok_or("snapshot has no local_path; nothing to replicate")?;
    if !tokio::fs::try_exists(local_path).await.unwrap_or(false) {
        return Err(format!("local_path {} does not exist", local_path.display()).into());
    }

    let key = blob_key_for(snap.id);

    // tar+zstd is sync work — pin to spawn_blocking so a multi-GB
    // snapshot doesn't stall the runtime. The encoded bytes land in
    // a Vec<u8> for now; future iteration can stream chunk-by-chunk
    // via an mpsc channel for memory-bounded uploads of huge
    // memory.bin payloads. For dev-tier ProcessBackend snapshots
    // (a tarball of a workdir) the in-memory hop is fine.
    let local_path_owned = local_path.clone();
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        encode_tar_zstd(&local_path_owned)
    })
    .await
    .map_err(|e| format!("tar+zstd join: {e}"))??;

    let size_compressed = bytes.len();
    let chunk = Bytes::from(bytes);
    let stream: ByteStream = Box::pin(futures::stream::once(async move {
        Ok::<_, engram_core::StorageError>(chunk)
    }));
    blob.put(&key, stream).await?;

    meta.mark_snapshot_replicated(snap.id, key.clone()).await?;

    tracing::info!(
        snapshot_id = %snap.id,
        session_id = %snap.session_id,
        compressed_bytes = size_compressed,
        original_bytes = snap.size_bytes,
        blob_key = %key,
        "snapshot replicated to blob storage",
    );
    Ok(())
}

/// Blob key shape for a snapshot. Stable so cold-tier restore can
/// derive the same key from the snapshot id alone if needed.
pub fn blob_key_for(id: SnapshotId) -> String {
    format!("snapshots/{id}.tar.zst")
}

fn encode_tar_zstd(local_path: &Path) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let zstd = zstd::Encoder::new(&mut buf, 3)?.auto_finish();
    let mut tar = tar::Builder::new(zstd);
    // Pack the directory contents (not the directory itself) so the
    // restore-side untar lands files at the snapshot root rather than
    // under an extra `<snapshot_id>/` prefix. Empty third arg so the
    // archive paths are relative.
    tar.append_dir_all(".", local_path)?;
    tar.finish()?;
    drop(tar); // flushes zstd via auto_finish
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::snapshot::SnapshotRecord;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_snapshot_dir(root: &Path, files: &[(&str, &[u8])]) {
        std::fs::create_dir_all(root).unwrap();
        for (name, body) in files {
            std::fs::write(root.join(name), body).unwrap();
        }
    }

    #[tokio::test]
    async fn encode_tar_zstd_round_trips_through_decompression() {
        let dir = TempDir::new().unwrap();
        let snap_root = dir.path().join("snap-1");
        write_snapshot_dir(
            &snap_root,
            &[
                ("memory.bin", &(0..=255u8).collect::<Vec<u8>>()),
                ("vmstate.bin", b"some-vm-state"),
            ],
        );

        let local_path = snap_root.clone();
        let bytes =
            tokio::task::spawn_blocking(move || encode_tar_zstd(&local_path).unwrap())
                .await
                .unwrap();

        // Decode and verify the payload landed.
        let cur = std::io::Cursor::new(bytes);
        let dec = zstd::Decoder::new(cur).unwrap();
        let mut tar = tar::Archive::new(dec);
        let extract = TempDir::new().unwrap();
        tar.unpack(extract.path()).unwrap();
        let mem = std::fs::read(extract.path().join("memory.bin")).unwrap();
        assert_eq!(mem, (0..=255u8).collect::<Vec<u8>>());
        let vm = std::fs::read(extract.path().join("vmstate.bin")).unwrap();
        assert_eq!(vm, b"some-vm-state");
    }

    #[test]
    fn blob_key_is_stable_per_snapshot() {
        let id = SnapshotId::new();
        let k1 = blob_key_for(id);
        let k2 = blob_key_for(id);
        assert_eq!(k1, k2);
        assert!(k1.starts_with("snapshots/"));
        assert!(k1.ends_with(".tar.zst"));
    }

    #[test]
    fn replication_config_default_is_lazy_enough_for_dev() {
        let cfg = ReplicationConfig::default();
        assert!(cfg.poll_interval >= Duration::from_secs(10));
        assert!(cfg.batch_size > 0);
    }

    /// Smoke test the full driver against in-process fakes: a record_snapshot
    /// + run_once cycle should produce a blob entry and stamp replicated_at.
    #[tokio::test]
    async fn run_once_replicates_pending_snapshot_via_local_blob() {
        use chrono::Utc;
        use engram_storage_local::LocalStorage;

        let workdir = TempDir::new().unwrap();
        let snap_root = workdir.path().join("snap-1");
        write_snapshot_dir(&snap_root, &[("data.bin", b"hello-replication")]);

        let blob: Arc<dyn BlobStorage> = Arc::new(LocalStorage::new(workdir.path().join("blob")));
        let meta: Arc<dyn MetadataStore> = Arc::new(StubMeta::default());

        let snapshot_id = SnapshotId::new();
        let session_id = engram_core::SessionId::new();
        meta.record_snapshot(SnapshotRecord {
            id: snapshot_id,
            session_id,
            host_id: None,
            local_path: Some(snap_root),
            blob_url: None,
            image_version: "warm-test".into(),
            size_bytes: 17,
            replicated_at: None,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
        })
        .await
        .unwrap();

        run_once(&ReplicationConfig::default(), &blob, &meta)
            .await
            .unwrap();

        // The blob landed under the expected key.
        assert!(blob.exists(&blob_key_for(snapshot_id)).await.unwrap());

        // The meta row's blob_url is now populated.
        let listed = meta.list_snapshots_for_session(session_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].blob_url.as_deref(),
            Some(blob_key_for(snapshot_id).as_str())
        );
        assert!(listed[0].replicated_at.is_some());

        // Subsequent run_once finds no pending — already replicated.
        run_once(&ReplicationConfig::default(), &blob, &meta)
            .await
            .unwrap();
        let still = meta.list_snapshots_for_session(session_id).await.unwrap();
        assert_eq!(still.len(), 1, "no duplicate row inserted");
    }

    // ---- Inline stub MetadataStore for the smoke test ----

    use async_trait::async_trait;
    use engram_core::types::{
        HostRecord, HostStatus, ImageVersion, PersistedEvent, Session, SessionSpec, SessionStatus,
    };
    use engram_core::{HostId, MetaError, SandboxId, SessionId};
    use parking_lot::Mutex;
    use std::collections::HashMap;

    #[derive(Default)]
    struct StubMeta {
        snapshots: Mutex<HashMap<SessionId, Vec<SnapshotRecord>>>,
    }

    #[async_trait]
    impl MetadataStore for StubMeta {
        async fn create_session(
            &self,
            _: SessionSpec,
            _: String,
        ) -> Result<SessionId, MetaError> {
            Ok(SessionId::new())
        }
        async fn get_session(&self, _: SessionId) -> Result<Session, MetaError> {
            Err(MetaError::NotFound)
        }
        async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
            Ok(Vec::new())
        }
        async fn set_session_status(
            &self,
            _: SessionId,
            _: SessionStatus,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn assign_session_host(
            &self,
            _: SessionId,
            _: Option<HostId>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn assign_session_sandbox(
            &self,
            _: SessionId,
            _: Option<SandboxId>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn upsert_host(&self, _: HostRecord) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn set_host_status(&self, _: HostId, _: HostStatus) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_reassign_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<SessionId>, MetaError> {
            Ok(Vec::new())
        }
        async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
            self.snapshots
                .lock()
                .entry(snap.session_id)
                .or_default()
                .push(snap);
            Ok(())
        }
        async fn list_snapshots_for_session(
            &self,
            sid: SessionId,
        ) -> Result<Vec<SnapshotRecord>, MetaError> {
            Ok(self.snapshots.lock().get(&sid).cloned().unwrap_or_default())
        }
        async fn latest_snapshot_for_session(
            &self,
            sid: SessionId,
        ) -> Result<Option<SnapshotRecord>, MetaError> {
            Ok(self
                .snapshots
                .lock()
                .get(&sid)
                .and_then(|v| v.last().cloned()))
        }
        async fn list_pending_replications(
            &self,
            limit: i64,
        ) -> Result<Vec<SnapshotRecord>, MetaError> {
            let g = self.snapshots.lock();
            let mut out: Vec<SnapshotRecord> = g
                .values()
                .flatten()
                .filter(|s| s.blob_url.is_none() && s.local_path.is_some())
                .cloned()
                .collect();
            out.sort_by_key(|s| s.created_at);
            out.truncate(limit.max(0) as usize);
            Ok(out)
        }
        async fn mark_snapshot_replicated(
            &self,
            id: SnapshotId,
            blob_url: String,
        ) -> Result<(), MetaError> {
            let mut g = self.snapshots.lock();
            for v in g.values_mut() {
                for s in v.iter_mut() {
                    if s.id == id {
                        s.blob_url = Some(blob_url);
                        s.replicated_at = Some(chrono::Utc::now());
                        return Ok(());
                    }
                }
            }
            Err(MetaError::NotFound)
        }
        async fn upsert_image_version(&self, _: ImageVersion) -> Result<(), MetaError> {
            Ok(())
        }
        async fn latest_ready_image(&self, _: &str) -> Result<Option<ImageVersion>, MetaError> {
            Ok(None)
        }
        async fn append_session_event(
            &self,
            _: SessionId,
            _: &str,
            _: serde_json::Value,
        ) -> Result<i64, MetaError> {
            Ok(0)
        }
        async fn list_session_events_since(
            &self,
            _: SessionId,
            _: i64,
            _: i64,
        ) -> Result<Vec<PersistedEvent>, MetaError> {
            Ok(Vec::new())
        }
    }

    // Silence unused-import warnings on PathBuf / SnapshotId in
    // older test harnesses that don't reach this far.
    #[allow(dead_code)]
    fn _force_unused() -> (PathBuf, SnapshotId) {
        (PathBuf::new(), SnapshotId::new())
    }
}
