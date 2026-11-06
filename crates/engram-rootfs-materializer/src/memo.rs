//! The materialize memo: which disk manifest one set of inputs produced.
//! It lives in the blob store under `materialize-memo/<key>`, so a
//! refresh whose layers did not change skips the download, the pack and
//! the chunk upload on any host.
//!
//! The key covers every input of the packed bytes. A hit is used only
//! when its manifest and every chunk it lists are still in the store:
//! chunk GC can delete the chunks of a manifest no image references.

use engram_chunk_store::{ChunkStore, Manifest, ManifestRef};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::pull::{LayerPlan, Platform};

/// The version of the bytes a materialize writes for one set of inputs.
/// Increase it with every change to what the packer writes, or a memo
/// hit returns the output of the earlier layout.
/// The pinned fixture manifest in `tests/materialize.rs` fails when the
/// output changes.
pub const LAYOUT_VERSION: u32 = 1;

const PREFIX: &str = "materialize-memo/";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoKey(String);

#[derive(Serialize, Deserialize)]
struct Record {
    disk_manifest: ManifestRef,
}

impl MemoKey {
    /// SHA-256 over the layout version, the chunk size of the disk
    /// manifest, the platform, the ordered layer digests and media types,
    /// the init shim and the size floor.
    pub fn new(
        chunk_size: u64,
        platform: Platform,
        layers: &[LayerPlan],
        shim: &(String, u16, Vec<u8>),
        min_fs_size_bytes: u64,
    ) -> Self {
        let mut h = sha2::Sha256::new();
        let mut field = |bytes: &[u8]| {
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        };
        field(&LAYOUT_VERSION.to_le_bytes());
        field(&chunk_size.to_le_bytes());
        field(platform.to_string().as_bytes());
        for layer in layers {
            field(layer.blob.digest.as_bytes());
            field(layer.blob.media_type.as_bytes());
        }
        let (path, mode, body) = shim;
        field(path.as_bytes());
        field(&mode.to_le_bytes());
        field(body);
        field(&min_fs_size_bytes.to_le_bytes());
        Self(hex::encode(h.finalize()))
    }

    fn storage_key(&self) -> String {
        format!("{PREFIX}{}", self.0)
    }
}

/// The manifest a previous materialize of these inputs committed, when
/// the store still holds it and all its chunks. Every failure is a miss.
pub async fn lookup(store: &ChunkStore, key: &MemoKey) -> Option<(ManifestRef, Manifest)> {
    let bytes = store.blob_storage().get(&key.storage_key()).await.ok()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    let manifest = store.get_manifest(record.disk_manifest).await.ok()?;
    match store.all_chunks_stored(&manifest).await {
        Ok(true) => Some((record.disk_manifest, manifest)),
        Ok(false) => {
            tracing::info!(
                manifest = %record.disk_manifest,
                "materialize memo: the manifest has chunks the store no longer holds"
            );
            None
        }
        Err(e) => {
            tracing::warn!(manifest = %record.disk_manifest, error = %e, "materialize memo: chunk probe failed");
            None
        }
    }
}

/// Record that `key` produced `disk_manifest`. A failure costs only the
/// next refresh its shortcut.
pub async fn record(store: &ChunkStore, key: &MemoKey, disk_manifest: ManifestRef) {
    let body = match serde_json::to_vec(&Record { disk_manifest }) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!(error = %e, "materialize memo: encode failed");
            return;
        }
    };
    if let Err(e) = store
        .blob_storage()
        .put(&key.storage_key(), body.into())
        .await
    {
        tracing::warn!(error = %e, "materialize memo: write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pull::LayerCompression;

    fn layer(index: usize, digest: &str) -> LayerPlan {
        LayerPlan {
            blob: engram_oci::DockerBlobRef {
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
                digest: digest.into(),
                size: 1,
            },
            compression: LayerCompression::Gzip,
            index,
        }
    }

    fn shim(body: &str) -> (String, u16, Vec<u8>) {
        ("sbin/init".into(), 0o755, body.as_bytes().to_vec())
    }

    #[test]
    fn every_input_changes_the_key() {
        let layers = [layer(0, "sha256:a"), layer(1, "sha256:b")];
        const CHUNK: u64 = 16 << 20;
        let base = MemoKey::new(CHUNK, Platform::LinuxAmd64, &layers, &shim("x"), 0);
        assert_eq!(
            base,
            MemoKey::new(CHUNK, Platform::LinuxAmd64, &layers, &shim("x"), 0)
        );
        let swapped = [layer(0, "sha256:b"), layer(1, "sha256:a")];
        for other in [
            MemoKey::new(CHUNK / 2, Platform::LinuxAmd64, &layers, &shim("x"), 0),
            MemoKey::new(CHUNK, Platform::LinuxArm64, &layers, &shim("x"), 0),
            MemoKey::new(CHUNK, Platform::LinuxAmd64, &swapped, &shim("x"), 0),
            MemoKey::new(CHUNK, Platform::LinuxAmd64, &layers[..1], &shim("x"), 0),
            MemoKey::new(CHUNK, Platform::LinuxAmd64, &layers, &shim("y"), 0),
            MemoKey::new(CHUNK, Platform::LinuxAmd64, &layers, &shim("x"), 1),
        ] {
            assert_ne!(base, other);
        }
    }
}
