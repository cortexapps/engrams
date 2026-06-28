//! ADR 0014 portable snapshot artifacts in `BlobStorage`.
//!
//! Memory chunks are already in `BlobStorage` (chunked manifests,
//! ADR 0007). What's missing for cross-host restore are:
//!
//! - **state.bin** — FC VMM + device state, embeds the canonical
//!   `path_on_host` for every drive. Tens of KiB to low MiB.
//! - **sidecar.json** — FC snapshot manifest (`FcSnapshotManifest`):
//!   spec, network config, memory_manifest ref, source sandbox_id.
//! - **rootfs.tar.zst** (M2 interim) — writable rootfs blob for
//!   sessions whose disk state isn't already represented as a
//!   chunked-disk manifest. Replaced by the chunked path when the
//!   writable-NBD plumbing lands.
//!
//! All three are opaque blobs (not chunked). Keys are derived from
//! the snapshot_id so the same scheme works for base template
//! snapshots (image-builder bake-time) and durability snapshots
//! (host-agent runtime) alike. Lives in `engram-chunk-store` rather
//! than `engram-host-agent` so producers in either crate (image
//! builder + host-agent + future migration tooling) reach the same
//! key scheme without cross-dependency.

use std::path::Path;

use bytes::Bytes;
use engram_core::error::BlobError;
use engram_core::traits::storage::BlobStorage;
use engram_core::types::SnapshotId;

/// `snapshots/<snapshot_id>/state.bin` — FC VMM/device state.
pub fn state_blob_key(snapshot_id: SnapshotId) -> String {
    format!("snapshots/{snapshot_id}/state.bin")
}

/// `snapshots/<snapshot_id>/sidecar.json` — FC `FcSnapshotManifest`
/// JSON. Carries spec, net, memory_manifest, source sandbox_id.
pub fn sidecar_blob_key(snapshot_id: SnapshotId) -> String {
    format!("snapshots/{snapshot_id}/sidecar.json")
}

/// `snapshots/<snapshot_id>/rootfs.tar.zst` — writable rootfs
/// upload, M2 interim. Replaced by chunked-disk manifest when the
/// writable-NBD plumbing lands.
pub fn rootfs_blob_key(snapshot_id: SnapshotId) -> String {
    format!("snapshots/{snapshot_id}/rootfs.tar.zst")
}

/// Upload an opaque file to `BlobStorage`. Reads the whole file
/// into memory — meant for small payloads (state.bin sidecar,
/// kilobytes; sidecar.json, low KiB). Don't use on rootfs blobs
/// (multi-MiB to GiB); those want streaming, currently TODO.
pub async fn upload_file(blob: &dyn BlobStorage, key: &str, path: &Path) -> Result<u64, BlobError> {
    let bytes = tokio::fs::read(path).await?;
    let len = bytes.len() as u64;
    blob.put(key, Bytes::from(bytes)).await?;
    Ok(len)
}

/// Download a small opaque blob to a local file path. Atomic via
/// write-temp-rename. Creates parent dirs.
pub async fn download_file(
    blob: &dyn BlobStorage,
    key: &str,
    dest: &Path,
) -> Result<(), BlobError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = blob.get(key).await?;
    let tmp = dest.with_extension("partial");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::SnapshotId;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    #[tokio::test]
    async fn blob_key_format_is_stable() {
        let id = SnapshotId::new();
        assert_eq!(state_blob_key(id), format!("snapshots/{id}/state.bin"),);
        assert_eq!(sidecar_blob_key(id), format!("snapshots/{id}/sidecar.json"),);
        assert_eq!(
            rootfs_blob_key(id),
            format!("snapshots/{id}/rootfs.tar.zst"),
        );
    }

    #[tokio::test]
    async fn upload_download_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));

        let src = tmp.path().join("source.bin");
        tokio::fs::write(&src, b"hello-state-bin").await.unwrap();

        let id = SnapshotId::new();
        let key = state_blob_key(id);
        let size = upload_file(blob.as_ref(), &key, &src).await.unwrap();
        assert_eq!(size, b"hello-state-bin".len() as u64);

        let dest = tmp.path().join("downloaded.bin");
        download_file(blob.as_ref(), &key, &dest).await.unwrap();
        let body = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(body, b"hello-state-bin");
    }

    #[tokio::test]
    async fn download_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let src = tmp.path().join("source.bin");
        tokio::fs::write(&src, b"x").await.unwrap();
        let id = SnapshotId::new();
        upload_file(blob.as_ref(), &state_blob_key(id), &src)
            .await
            .unwrap();
        let dest = tmp.path().join("nested/dir/state.bin");
        download_file(blob.as_ref(), &state_blob_key(id), &dest)
            .await
            .unwrap();
        assert!(dest.exists());
    }

    #[tokio::test]
    async fn download_missing_blob_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let dest = tmp.path().join("never.bin");
        let id = SnapshotId::new();
        let err = download_file(blob.as_ref(), &state_blob_key(id), &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, BlobError::NotFound));
        assert!(!dest.exists());
    }
}
