//! Read-only physical inputs. Catalog approval and lineage belong to the coordinator.
use engram_chunk_store::{ChunkHash, ChunkStore, Manifest, ManifestKind};
use engram_core::{types::sandbox::AuxRoDrive, SandboxError};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};
use tokio::io::AsyncReadExt;

const BUFFER_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
// One inspection per host, including after cancellation of manifest decoding.
static READ_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

fn invalid(_: impl std::fmt::Display) -> SandboxError {
    SandboxError::InvalidSpec("managed cold asset verification failed".into())
}

pub(crate) async fn inspect(
    chunks: &ChunkStore,
    reference: engram_core::types::manifest::ManifestRef,
    kernel: &Path,
    bundle_dir: &Path,
    drives: &[AuxRoDrive],
) -> Result<(String, String, u64), SandboxError> {
    let permit = Arc::new(READ_SLOT.acquire().await.map_err(invalid)?);
    // No spawned read worker survives cancellation. Tokio's file operations may
    // finish one bounded read; manifest decoding retains the slot until it exits.
    let work = async {
        let storage = chunks.blob_storage();
        let mut body = storage
            .get_streaming(&reference.storage_key())
            .await
            .map_err(invalid)?;
        let mut json = Vec::new();
        while let Some(bytes) = body.next().await {
            let bytes = bytes.map_err(invalid)?;
            if bytes.len() > MAX_MANIFEST_BYTES - json.len() {
                return Err(invalid("manifest size"));
            }
            json.extend_from_slice(&bytes);
        }
        let decode_permit = permit.clone();
        let manifest = tokio::task::spawn_blocking(move || {
            let _permit = decode_permit;
            let manifest: Manifest = serde_json::from_slice(&json).map_err(invalid)?;
            manifest.validate().map_err(invalid)?;
            if manifest.kind != ManifestKind::Disk || manifest.total_bytes == 0 {
                return Err(invalid("disk kind or size"));
            }
            Ok::<_, SandboxError>(manifest)
        })
        .await
        .map_err(invalid)??;
        let mut disk_hash = Sha256::new();
        let mut offset = 0;
        let zeros = vec![0; BUFFER_BYTES];
        for entry in &manifest.chunks {
            hash_zeros(&mut disk_hash, entry.offset - offset, &zeros).await;
            let expected = manifest
                .chunk_size
                .as_u64()
                .min(manifest.total_bytes - entry.offset);
            let mut body = storage
                .get_streaming(&entry.hash.storage_key())
                .await
                .map_err(invalid)?;
            let mut chunk_hash = Sha256::new();
            let mut size = 0;
            while let Some(bytes) = body.next().await {
                let bytes = bytes.map_err(invalid)?;
                if bytes.len() as u64 > expected - size {
                    return Err(invalid("chunk length"));
                }
                size += bytes.len() as u64;
                for block in bytes.chunks(BUFFER_BYTES) {
                    chunk_hash.update(block);
                    disk_hash.update(block);
                    tokio::task::yield_now().await;
                }
            }
            if size != expected || ChunkHash::from_bytes(chunk_hash.finalize().into()) != entry.hash
            {
                return Err(invalid("chunk content"));
            }
            offset = entry.offset + expected;
        }
        hash_zeros(&mut disk_hash, manifest.total_bytes - offset, &zeros).await;
        let (kernel_hash, _) = hash_file(kernel).await?;
        for drive in drives {
            let expected = drive
                .sha256
                .as_deref()
                .ok_or_else(|| invalid("unresolved bundle"))?;
            // The request validator checks slot order and digest syntax before
            // deriving a path. There is no caller-supplied host path.
            let (actual, _) =
                hash_file(&bundle_dir.join(AuxRoDrive::staged_file_name(expected))).await?;
            if actual != expected {
                return Err(invalid("bundle content"));
            }
        }
        Ok((
            kernel_hash,
            hex::encode(disk_hash.finalize()),
            manifest.total_bytes,
        ))
    };
    tokio::time::timeout(std::time::Duration::from_secs(300), work)
        .await
        .map_err(invalid)?
}

async fn hash_zeros(hash: &mut Sha256, mut count: u64, zeros: &[u8]) {
    while count > 0 {
        let n = count.min(zeros.len() as u64) as usize;
        hash.update(&zeros[..n]);
        count -= n as u64;
        tokio::task::yield_now().await;
    }
}

pub(crate) async fn hash_file(path: &Path) -> Result<(String, u64), SandboxError> {
    use std::os::unix::fs::MetadataExt;
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(invalid)?;
    let before = file.metadata().await.map_err(invalid)?;
    if !before.is_file() || before.len() == 0 {
        return Err(invalid("artifact type"));
    }
    let mut hash = Sha256::new();
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut size = 0;
    loop {
        let n = file.read(&mut buffer).await.map_err(invalid)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > before.len() {
            return Err(invalid("artifact grew"));
        }
        hash.update(&buffer[..n]);
        tokio::task::yield_now().await;
    }
    let after = file.metadata().await.map_err(invalid)?;
    let identity = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if size != before.len() || identity(&before) != identity(&after) {
        return Err(invalid("artifact changed"));
    }
    Ok((hex::encode(hash.finalize()), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::{ChunkRef, ChunkSize};
    use engram_core::types::manifest::ManifestRef;

    async fn fixture() -> (tempfile::TempDir, ChunkStore, ManifestRef) {
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(Arc::new(engram_storage_local::LocalBlobStorage::new(
            temp.path().join("blobs"),
        )));
        tokio::fs::write(temp.path().join("kernel"), b"kernel")
            .await
            .unwrap();
        (temp, store, ManifestRef::new())
    }

    #[tokio::test]
    async fn cold_assets_reject_corrupt_short_long_missing_and_non_disk_chunks() {
        let (temp, store, reference) = fixture().await;
        let mut manifest = Manifest::empty(ManifestKind::Disk, 12);
        manifest.chunk_size = ChunkSize::bytes(4);
        let hash = store.put_chunk(b"abcd").await.unwrap();
        manifest.chunks.push(ChunkRef { offset: 4, hash });
        store.put_manifest(reference, &manifest).await.unwrap();
        let kernel = temp.path().join("kernel");
        let read = || inspect(&store, reference, &kernel, temp.path(), &[]);
        let (_, actual, size) = read().await.unwrap();
        assert_eq!(size, 12);
        assert_eq!(actual, hex::encode(Sha256::digest(b"\0\0\0\0abcd\0\0\0\0")));
        let chunk = temp.path().join("blobs").join(hash.storage_key());
        for wrong in [b"wxyz".as_slice(), b"abc", b"abcde"] {
            tokio::fs::write(&chunk, wrong).await.unwrap();
            assert!(read().await.is_err());
        }
        tokio::fs::remove_file(&chunk).await.unwrap();
        assert!(read().await.is_err());
        // A correct hash is insufficient when the manifest gives a wrong length.
        for wrong in [b"abc".as_slice(), b"abcde"] {
            manifest.chunks[0].hash = store.put_chunk(wrong).await.unwrap();
            let next = ManifestRef::new();
            store.put_manifest(next, &manifest).await.unwrap();
            assert!(inspect(&store, next, &kernel, temp.path(), &[])
                .await
                .is_err());
        }
        manifest.kind = ManifestKind::Memory;
        let next = ManifestRef::new();
        store.put_manifest(next, &manifest).await.unwrap();
        assert!(inspect(&store, next, &kernel, temp.path(), &[])
            .await
            .is_err());
        assert!(
            inspect(&store, ManifestRef::new(), &kernel, temp.path(), &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cold_assets_sparse_hash_yields_and_cancel_releases_read_slot() {
        let (temp, store, reference) = fixture().await;
        store
            .put_manifest(reference, &Manifest::empty(ManifestKind::Disk, 1 << 40))
            .await
            .unwrap();
        let kernel = temp.path().join("kernel");
        // The single-thread runtime must poll its timer while the sparse read
        // hashes zeros. Cancellation must release the slot without a worker.
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            inspect(&store, reference, &kernel, temp.path(), &[])
        )
        .await
        .is_err());
        let small = ManifestRef::new();
        store
            .put_manifest(small, &Manifest::empty(ManifestKind::Disk, 3))
            .await
            .unwrap();
        let (_, hash, size) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            inspect(&store, small, &kernel, temp.path(), &[]),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(hash, hex::encode(Sha256::digest([0; 3])));
        assert_eq!(size, 3);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn cold_assets_file_reader_rejects_symlinks_empty_and_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        tokio::fs::write(&file, []).await.unwrap();
        assert!(hash_file(&file).await.is_err());
        tokio::fs::write(&file, b"abc").await.unwrap();
        let symlink = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &symlink).unwrap();
        assert!(hash_file(&symlink).await.is_err());
        assert!(hash_file(temp.path()).await.is_err());
        let fifo = temp.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), hash_file(&fifo))
                .await
                .unwrap()
                .is_err()
        );
        let (hash, size) = hash_file(&file).await.unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(size, 3);
    }
}
