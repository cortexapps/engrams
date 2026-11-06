//! Image volumes on this host. The enabled set the coordinator sends is
//! the only trigger: each change runs one convergence pass. Readiness on
//! disk is read from the pool when a heartbeat is built, never stored.

use std::sync::Arc;

use engram_chunk_store::ChunkStore;
use engram_core::types::manifest::ManifestRef;
use engram_host_core::image_volumes::{reconcile_once, staged};
use engram_host_core::SessionDisk;
use engram_protocol::heartbeat::{EnabledImageRef, ManifestDigest};
use tokio::sync::watch;

pub fn spawn(
    disk: Arc<dyn SessionDisk>,
    store: ChunkStore,
    mut enabled: watch::Receiver<Vec<EnabledImageRef>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The channel's initial value is not a heartbeat ack. A pass on it
        // would delete every image volume, so the first pass waits for the
        // set the coordinator sends.
        while enabled.changed().await.is_ok() {
            let desired: Vec<ManifestRef> = enabled
                .borrow_and_update()
                .iter()
                .map(|image| image.base_snapshot_disk_manifest)
                .collect();
            match reconcile_once(disk.as_ref(), &store, &desired).await {
                Ok(report) => {
                    for (name, error) in &report.failed {
                        tracing::warn!(volume = %name, %error, "image volume operation failed");
                    }
                    if !report.staged.is_empty()
                        || !report.deleted.is_empty()
                        || !report.failed.is_empty()
                    {
                        tracing::info!(
                            staged = report.staged.len(),
                            deleted = report.deleted.len(),
                            failed = report.failed.len(),
                            "image volumes reconciled"
                        );
                    }
                }
                Err(error) => tracing::error!(%error, "image volumes: the disk cannot be listed"),
            }
        }
    })
}

/// `ready` narrowed to the images whose base disk volume exists now.
pub async fn ready_on_disk(
    disk: &dyn SessionDisk,
    enabled: &[EnabledImageRef],
    ready: Vec<ManifestDigest>,
) -> Vec<ManifestDigest> {
    let on_disk = match disk.list().await {
        Ok(names) => staged(&names),
        Err(error) => {
            tracing::warn!(%error, "image volumes: the disk cannot be listed; reporting no image ready");
            return Vec::new();
        }
    };
    ready
        .into_iter()
        .filter(|digest| {
            enabled.iter().any(|image| {
                image.manifest_digest == *digest
                    && on_disk.contains(&image.base_snapshot_disk_manifest)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use engram_core::types::SnapshotId;
    use engram_host_core::disk::FileDisk;
    use engram_host_core::image_volumes::volume_name;

    use super::*;

    fn image(digest: &str, disk_manifest: ManifestRef) -> EnabledImageRef {
        EnabledImageRef {
            image_uri: format!("example.test/{digest}"),
            manifest_digest: ManifestDigest::new(digest),
            base_snapshot_id: SnapshotId::new(),
            base_snapshot_disk_manifest: disk_manifest,
            base_snapshot_memory_manifest: None,
            warm_peers: Vec::new(),
        }
    }

    async fn volume_exists(disk: &FileDisk, name: &engram_host_core::VolumeName) -> bool {
        disk.list().await.unwrap().contains(name)
    }

    /// Poll until `cond` holds, for at most two seconds.
    async fn eventually(mut cond: impl AsyncFnMut() -> bool) -> bool {
        for _ in 0..200 {
            if cond().await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_restart_keeps_staged_volumes_until_the_coordinator_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), 4096).unwrap());
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(dir.path().join("blob")),
        );
        let staged_ref = ManifestRef::new();
        let staged_name = volume_name(staged_ref);
        disk.create(&staged_name, 4096).await.unwrap();

        // A fresh process: the channel holds its empty initial value.
        let (tx, rx) = watch::channel(Vec::<EnabledImageRef>::new());
        let task = spawn(disk.clone(), ChunkStore::new(blob), rx);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            volume_exists(&disk, &staged_name).await,
            "the initial empty value is not an ack and must delete nothing"
        );

        tx.send_replace(vec![image("a", staged_ref)]);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            volume_exists(&disk, &staged_name).await,
            "an enabled image keeps its volume"
        );

        tx.send_replace(Vec::new());
        assert!(
            eventually(async || !volume_exists(&disk, &staged_name).await).await,
            "an acked empty set deletes the volume"
        );
        task.abort();
    }

    #[tokio::test]
    async fn only_images_with_a_volume_on_disk_stay_ready() {
        let dir = tempfile::tempdir().unwrap();
        let disk = FileDisk::new(dir.path(), 4096).unwrap();
        let staged_ref = ManifestRef::new();
        let missing_ref = ManifestRef::new();
        disk.create(&volume_name(staged_ref), 4096).await.unwrap();
        let enabled = [image("a", staged_ref), image("b", missing_ref)];
        let ready = vec![
            ManifestDigest::new("a"),
            ManifestDigest::new("b"),
            ManifestDigest::new("not-enabled"),
        ];
        assert_eq!(
            ready_on_disk(&disk, &enabled, ready).await,
            vec![ManifestDigest::new("a")]
        );
    }
}
