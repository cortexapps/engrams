//! File ↔ manifest helpers.
//!
//! `chunk_file()` takes a path + `ManifestKind` and produces an
//! unstored `Manifest`: it walks the file in `chunk_size` blocks,
//! PUTs each non-zero chunk into the store (idempotent — same
//! bytes → same hash → same key), and builds the `Vec<ChunkRef>`.
//! Zero-filled blocks are *omitted* from the manifest entirely
//! (consumers treat absence as zero-fill), keeping sparse images
//! cheap.
//!
//! `materialize_to_file()` is the inverse: take a stored manifest
//! and reconstruct the original byte sequence on disk. Used by
//! the macOS adapter (which needs a regular file to feed VZ) and
//! by tests.
//!
//! Both are cancel-safe (caller's drop just aborts the next
//! chunk's work) and stream — never load the whole image into
//! RAM.

use std::path::Path;

use bytes::Bytes;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::error::Result;
use crate::manifest::{ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION};
use crate::store::ChunkStore;

impl ChunkStore {
    /// Walk a file in `chunk_size` blocks, PUT each non-zero block
    /// into the store, return a `Manifest` describing the result.
    /// Caller commits it via `put_manifest()` to make it durable.
    ///
    /// **Skips all-zero blocks.** A 1 GiB sparse file with one
    /// non-zero 16 MiB region produces a manifest with ONE
    /// `ChunkRef`. On `materialize_to_file()` the holes show up
    /// as actual filesystem holes (`SEEK_HOLE` works), so storage
    /// cost downstream is also sparse.
    ///
    /// `chunk_size` defaults to the kind's standard if `None`.
    pub async fn chunk_file(
        &self,
        path: &Path,
        kind: ManifestKind,
        chunk_size: Option<u64>,
    ) -> Result<Manifest> {
        let chunk_size = chunk_size.unwrap_or_else(|| kind.default_chunk_size());
        let mut file = fs::File::open(path).await?;
        let meta = file.metadata().await?;
        let total_bytes = meta.len();

        let mut manifest = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes,
            chunks: Vec::new(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };

        let mut buf = vec![0u8; chunk_size as usize];
        let mut offset: u64 = 0;
        while offset < total_bytes {
            let want = chunk_size.min(total_bytes - offset) as usize;
            let slice = &mut buf[..want];
            file.read_exact(slice).await?;

            if is_all_zero(slice) {
                offset += want as u64;
                continue;
            }

            let hash = self.put_chunk(slice).await?;
            manifest.chunks.push(ChunkRef { offset, hash });
            offset += want as u64;
        }

        Ok(manifest)
    }

    /// Reconstruct a file from a manifest. Creates `dest` (or
    /// truncates if it exists), writes each chunk at its offset,
    /// seeks past zero-filled gaps (producing real filesystem
    /// holes on ext4/APFS/btrfs/zfs), truncates to
    /// `manifest.total_bytes`.
    ///
    /// Consumers wanting a *fully dense* file (e.g. virtio-blk
    /// disk attachments that don't tolerate holes) can `dd
    /// conv=notrunc` afterward, but most don't need to.
    pub async fn materialize_to_file(&self, manifest: &Manifest, dest: &Path) -> Result<()> {
        manifest.validate()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dest)
            .await?;

        // Pre-extend to total_bytes so seeks past existing-EOF
        // don't have to grow the file chunk-by-chunk. On
        // sparse-supporting filesystems this is a metadata-only op.
        file.set_len(manifest.total_bytes).await?;

        for entry in &manifest.chunks {
            let bytes = self.get_chunk(entry.hash).await?;
            file.seek(SeekFrom::Start(entry.offset)).await?;
            file.write_all(&bytes).await?;
        }
        file.flush().await?;
        Ok(())
    }

    /// Like [`materialize_to_file`] but reads chunks from the
    /// supplied [`crate::cache::ChunkCache`] instead of going
    /// straight to the underlying store. Use this when the caller
    /// has a hot path with locality (multiple sessions reusing
    /// base-image chunks).
    pub async fn materialize_to_file_cached(
        &self,
        manifest: &Manifest,
        dest: &Path,
        cache: &crate::cache::ChunkCache,
    ) -> Result<()> {
        manifest.validate()?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dest)
            .await?;
        file.set_len(manifest.total_bytes).await?;
        for entry in &manifest.chunks {
            let bytes: Bytes = cache.get(entry.hash).await?;
            file.seek(SeekFrom::Start(entry.offset)).await?;
            file.write_all(&bytes).await?;
        }
        file.flush().await?;
        Ok(())
    }
}

fn is_all_zero(buf: &[u8]) -> bool {
    // Auto-vectorized by LLVM on x86_64 + arm64. ~10 ms for a
    // 16 MiB block on a recent laptop — negligible against the
    // sha256 we'd skip if the answer is "yes."
    buf.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{ChunkCache, ChunkCacheConfig};
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    async fn store() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (ChunkStore::new(blob), dir)
    }

    #[tokio::test]
    async fn chunk_and_materialize_round_trip_byte_for_byte() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();

        // 40 MiB of random-ish bytes (deterministic pattern so
        // the round-trip is reproducible).
        let src = work.path().join("src.bin");
        let mut data = vec![0u8; 40 * 1024 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 257) as u8;
        }
        fs::write(&src, &data).await.unwrap();

        let manifest = s
            .chunk_file(&src, ManifestKind::Disk, Some(16 * 1024 * 1024))
            .await
            .unwrap();
        // 40 MiB / 16 MiB = 3 chunks (16 + 16 + 8).
        assert_eq!(manifest.chunks.len(), 3);
        assert_eq!(manifest.total_bytes, data.len() as u64);

        let dest = work.path().join("dest.bin");
        s.materialize_to_file(&manifest, &dest).await.unwrap();
        let got = fs::read(&dest).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn chunk_file_skips_zero_blocks() {
        // 32 MiB total, 16-MiB chunks. First chunk is all zeros,
        // second has data. Manifest should list exactly one
        // ChunkRef at offset 16 MiB.
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let mut data = vec![0u8; 32 * 1024 * 1024];
        for (i, b) in data[16 * 1024 * 1024..].iter_mut().enumerate() {
            *b = (i % 200 + 1) as u8;
        }
        let src = work.path().join("sparse.bin");
        fs::write(&src, &data).await.unwrap();

        let m = s
            .chunk_file(&src, ManifestKind::Disk, Some(16 * 1024 * 1024))
            .await
            .unwrap();
        assert_eq!(m.chunks.len(), 1);
        assert_eq!(m.chunks[0].offset, 16 * 1024 * 1024);
    }

    #[tokio::test]
    async fn materialize_reproduces_zero_holes() {
        // Round-trip a sparse file: the materialized output must
        // have zero bytes where the source did, even if no
        // chunk was stored for that offset.
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let mut data = vec![0u8; 32 * 1024 * 1024];
        for b in data[16 * 1024 * 1024..16 * 1024 * 1024 + 100].iter_mut() {
            *b = 0xAB;
        }
        let src = work.path().join("src.bin");
        fs::write(&src, &data).await.unwrap();
        let m = s
            .chunk_file(&src, ManifestKind::Disk, Some(16 * 1024 * 1024))
            .await
            .unwrap();
        let dest = work.path().join("dest.bin");
        s.materialize_to_file(&m, &dest).await.unwrap();
        let got = fs::read(&dest).await.unwrap();
        assert_eq!(got.len(), data.len());
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn chunk_file_handles_short_final_chunk() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        // 1 MiB + 13 bytes; 16-MiB chunk size means one short
        // final chunk and total_bytes = 1 MiB + 13.
        let mut data = vec![0u8; 1024 * 1024 + 13];
        for (i, b) in data.iter_mut().enumerate() {
            *b = ((i + 1) % 250) as u8;
        }
        let src = work.path().join("short.bin");
        fs::write(&src, &data).await.unwrap();
        let m = s
            .chunk_file(&src, ManifestKind::Disk, Some(16 * 1024 * 1024))
            .await
            .unwrap();
        assert_eq!(m.chunks.len(), 1);
        assert_eq!(m.total_bytes, data.len() as u64);
        let dest = work.path().join("dest.bin");
        s.materialize_to_file(&m, &dest).await.unwrap();
        assert_eq!(fs::read(&dest).await.unwrap(), data);
    }

    #[tokio::test]
    async fn identical_files_share_chunk_storage() {
        // Two source files with identical bytes produce
        // manifests whose ChunkRefs point at the same hashes —
        // automatic cross-file dedup at the storage layer.
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let data = (0..32u8).cycle().take(20 * 1024 * 1024).collect::<Vec<_>>();
        let p1 = work.path().join("a.bin");
        let p2 = work.path().join("b.bin");
        fs::write(&p1, &data).await.unwrap();
        fs::write(&p2, &data).await.unwrap();

        let m1 = s
            .chunk_file(&p1, ManifestKind::Disk, Some(16 * 1024 * 1024))
            .await
            .unwrap();
        let m2 = s
            .chunk_file(&p2, ManifestKind::Disk, Some(16 * 1024 * 1024))
            .await
            .unwrap();
        // Same chunk lists.
        assert_eq!(m1.chunks, m2.chunks);
        // And the hashes are equal — confirming dedup.
        for (a, b) in m1.chunks.iter().zip(m2.chunks.iter()) {
            assert_eq!(a.hash, b.hash);
        }
    }

    #[tokio::test]
    async fn materialize_via_cache_uses_cache_path() {
        let (s, _d) = store().await;
        let blob = tempfile::tempdir().unwrap();
        let _ = blob; // already held by store via _d
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: 1024 * 1024 * 1024,
            },
            s.clone(),
        );
        let work = tempfile::tempdir().unwrap();
        let data = (0..40u8).cycle().take(5 * 1024 * 1024).collect::<Vec<_>>();
        let src = work.path().join("src.bin");
        fs::write(&src, &data).await.unwrap();
        let m = s
            .chunk_file(&src, ManifestKind::Memory, Some(512 * 1024))
            .await
            .unwrap();
        let dest = work.path().join("dest.bin");
        s.materialize_to_file_cached(&m, &dest, &cache)
            .await
            .unwrap();
        assert_eq!(fs::read(&dest).await.unwrap(), data);
        // Cache should hold every chunk now.
        for entry in &m.chunks {
            assert!(cache.contains(entry.hash).await);
        }
    }
}
