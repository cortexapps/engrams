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
        loop {
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
                    if !report.deleted.is_empty() || !report.failed.is_empty() {
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
            if enabled.changed().await.is_err() {
                return;
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
