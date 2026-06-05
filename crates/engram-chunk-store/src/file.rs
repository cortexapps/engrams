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

    /// ADR 0028 Fix A: produce the next full-image manifest after a
    /// sparse diff was overlaid onto `full_file`. Re-reads, re-hashes
    /// and uploads ONLY the chunks intersecting `dirty_ranges`
    /// (`(offset, len)` byte ranges, typically the diff file's data
    /// extents); every other `ChunkRef` carries over from `prev`
    /// untouched — that's what makes a checkpoint's at-rest and CPU
    /// cost O(dirty set) instead of O(guest RAM).
    ///
    /// Invariants preserved from [`Self::chunk_file`]:
    /// - all-zero chunks are elided (a dirty chunk that became zero
    ///   DROPS out of the manifest; gaps mean zero-filled),
    /// - offsets stay sorted and chunk-size-aligned,
    /// - `total_bytes` must match the file (guest RAM never resizes;
    ///   a mismatch is a caller bug surfaced as `InvalidManifest`).
    pub async fn update_for_dirty_ranges(
        &self,
        prev: &Manifest,
        full_file: &Path,
        dirty_ranges: &[(u64, u64)],
    ) -> Result<Manifest> {
        use std::collections::BTreeMap;
        use std::collections::BTreeSet;

        prev.validate()?;
        let chunk_size = prev.chunk_size.as_u64();
        let mut file = fs::File::open(full_file).await?;
        let total_bytes = file.metadata().await?.len();
        if total_bytes != prev.total_bytes {
            return Err(crate::error::ChunkStoreError::MalformedManifest(format!(
                "update_for_dirty_ranges: file is {total_bytes} bytes but the previous \
                 manifest says {} — guest memory images never resize",
                prev.total_bytes,
            )));
        }

        // Which chunk indices does the dirty set touch?
        let mut dirty_chunks: BTreeSet<u64> = BTreeSet::new();
        for &(off, len) in dirty_ranges {
            if len == 0 {
                continue;
            }
            let end = (off + len - 1).min(total_bytes.saturating_sub(1));
            for idx in (off / chunk_size)..=(end / chunk_size) {
                dirty_chunks.insert(idx);
            }
        }

        // Start from the previous manifest's chunk map; replace /
        // insert / remove at dirty offsets only.
        let mut by_offset: BTreeMap<u64, ChunkRef> =
            prev.chunks.iter().map(|c| (c.offset, c.clone())).collect();

        let mut buf = vec![0u8; chunk_size as usize];
        for idx in dirty_chunks {
            let offset = idx * chunk_size;
            if offset >= total_bytes {
                continue;
            }
            let want = chunk_size.min(total_bytes - offset) as usize;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let slice = &mut buf[..want];
            file.read_exact(slice).await?;
            if is_all_zero(slice) {
                by_offset.remove(&offset);
                continue;
            }
            let hash = self.put_chunk(slice).await?;
            by_offset.insert(offset, ChunkRef { offset, hash });
        }

        Ok(Manifest {
            schema_version: prev.schema_version,
            kind: prev.kind,
            chunk_size: prev.chunk_size,
            total_bytes,
            chunks: by_offset.into_values().collect(),
            parent: prev.parent,
            working_set_trace: prev.working_set_trace,
            annotations: prev.annotations.clone(),
        })
    }

    /// ADR 0038: like [`Self::update_for_dirty_ranges`], but for a
    /// sandbox with NO local rolling memfile — a UFFD-resumed session
    /// whose checkpoint chain was seeded manifest-only. Instead of
    /// reading each dirty chunk from a merged full image, it
    /// reconstructs the chunk in memory from its PREVIOUS content
    /// (fetched by hash, served from the warm chunk cache) overlaid
    /// with the sparse `diff_path`'s dirty bytes. This lets the first
    /// post-resume checkpoint be a cheap diff instead of a Full re-read
    /// of guest RAM — which under UFFD would fault the whole working
    /// set in from the store (the 60 s `PUT /snapshot/create` hang this
    /// ADR fixes).
    ///
    /// Invariants match `update_for_dirty_ranges`: all-zero chunks are
    /// elided, offsets stay sorted + aligned, and `total_bytes` is
    /// taken from `prev` — the diff is sparse and its on-disk length is
    /// not authoritative. A dirty chunk whose offset is *absent* from
    /// `prev` (elided = all-zero) seeds from zeros (it was
    /// UFFDIO_ZEROPAGE'd at the prev capture).
    ///
    /// Cost: one `get_chunk` per dirty chunk (warm-cache hit for any
    /// page the guest faulted/wrote), vs `update_for_dirty_ranges`'s
    /// local file reads — but it needs no GiB-scale rolling memfile.
    pub async fn update_for_dirty_ranges_sparse(
        &self,
        prev: &Manifest,
        diff_path: &Path,
        dirty_ranges: &[(u64, u64)],
    ) -> Result<Manifest> {
        use std::collections::BTreeMap;
        use std::collections::BTreeSet;

        prev.validate()?;
        let chunk_size = prev.chunk_size.as_u64();
        let total_bytes = prev.total_bytes;

        // Which chunk indices does the dirty set touch?
        let mut dirty_chunks: BTreeSet<u64> = BTreeSet::new();
        for &(off, len) in dirty_ranges {
            if len == 0 {
                continue;
            }
            let end = (off + len - 1).min(total_bytes.saturating_sub(1));
            for idx in (off / chunk_size)..=(end / chunk_size) {
                dirty_chunks.insert(idx);
            }
        }

        let mut by_offset: BTreeMap<u64, ChunkRef> =
            prev.chunks.iter().map(|c| (c.offset, c.clone())).collect();

        let mut diff = fs::File::open(diff_path).await?;
        let mut buf = vec![0u8; chunk_size as usize];
        for idx in dirty_chunks {
            let offset = idx * chunk_size;
            if offset >= total_bytes {
                continue;
            }
            let want = chunk_size.min(total_bytes - offset) as usize;
            let slice = &mut buf[..want];

            // Seed the chunk from its previous content (warm cache) —
            // or zeros if `prev` elided this offset (all-zero chunk).
            match by_offset.get(&offset).map(|c| c.hash) {
                Some(prev_hash) => {
                    let bytes = self.get_chunk(prev_hash).await?;
                    let n = bytes.len().min(want);
                    slice[..n].copy_from_slice(&bytes[..n]);
                    if n < want {
                        slice[n..].fill(0);
                    }
                }
                None => slice.fill(0),
            }

            // Overlay the diff's dirty bytes that fall in this chunk.
            // `dirty_ranges` are the diff's data extents, so reads land
            // on real bytes (never a hole).
            for &(off, len) in dirty_ranges {
                if len == 0 {
                    continue;
                }
                let lo = off.max(offset);
                let hi = (off + len).min(offset + want as u64);
                if lo >= hi {
                    continue;
                }
                diff.seek(std::io::SeekFrom::Start(lo)).await?;
                diff.read_exact(&mut slice[(lo - offset) as usize..(hi - offset) as usize])
                    .await?;
            }

            if is_all_zero(slice) {
                by_offset.remove(&offset);
                continue;
            }
            let hash = self.put_chunk(slice).await?;
            by_offset.insert(offset, ChunkRef { offset, hash });
        }

        Ok(Manifest {
            schema_version: prev.schema_version,
            kind: prev.kind,
            chunk_size: prev.chunk_size,
            total_bytes,
            chunks: by_offset.into_values().collect(),
            parent: prev.parent,
            working_set_trace: prev.working_set_trace,
            annotations: prev.annotations.clone(),
        })
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
        // Write to a `.partial` sibling and atomically `rename` to
        // `dest` only after every chunk has been written. A crash
        // or mid-write error leaves only the partial file behind;
        // the canonical name doesn't exist, so callers that probe
        // for it (e.g. PooledBackend's fast-path on size match)
        // never see a half-written rootfs.
        let temp = temp_for(dest);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)
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
        // Drop the file handle before rename — Windows wants this,
        // and on POSIX it's cheap insurance against rare async-fs
        // edge cases.
        drop(file);
        fs::rename(&temp, dest).await?;
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
        // Same atomicity invariant as `materialize_to_file`: write
        // to a `.partial` sibling, rename only on full success.
        // Without this, a mid-write failure would leave a file at
        // `dest` with `total_bytes` zero-filler — and any caller
        // probing size as "is this materialized?" (e.g.
        // PooledBackend's fast-path) would treat it as a success
        // and feed a junk rootfs to FC / VZ.
        let temp = temp_for(dest);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)
            .await?;
        file.set_len(manifest.total_bytes).await?;
        for entry in &manifest.chunks {
            // Cache miss → fetch through `self` (whatever resolver
            // `self` carries — including the per-session tiered
            // resolver from ADR 0008 Phase 5). The cache is
            // backend-agnostic; each call site chooses.
            let bytes: Bytes = cache.get(entry.hash, || self.get_chunk(entry.hash)).await?;
            file.seek(SeekFrom::Start(entry.offset)).await?;
            file.write_all(&bytes).await?;
        }
        file.flush().await?;
        drop(file);
        fs::rename(&temp, dest).await?;
        Ok(())
    }
}

/// Sibling temp path used by `materialize_to_file*` for the
/// write-then-rename atomicity guard. Includes a per-process
/// random suffix so concurrent materializes against the same
/// `dest` don't collide on the temp path.
///
/// (The outer `materialize_chunked_rootfs` in `pooled_backend`
/// already serialises concurrent writers via a mutex, but
/// `materialize_to_file*` is also called directly by tests and
/// could be called by future code without that mutex.)
fn temp_for(dest: &Path) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut path = dest.as_os_str().to_owned();
    path.push(format!(".partial-{nonce}"));
    std::path::PathBuf::from(path)
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
        let cache = ChunkCache::new(ChunkCacheConfig {
            root: cache_dir.path().to_path_buf(),
            budget_bytes: 1024 * 1024 * 1024,
        });
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

    /// Atomicity invariant: a failed materialize must NOT leave a
    /// file at `dest`. Specifically, a previous bug let
    /// `materialize_to_file` (and _cached) zero-fill via
    /// `set_len(total_bytes)` before the chunk loop ran; if the
    /// chunk loop errored, the file was left at full size full of
    /// zeros, fooling any downstream size-based fast-path check
    /// (PooledBackend's "already materialized") into clone-ing a
    /// poisoned rootfs to the guest. After the write-temp-then-
    /// rename fix, an errored materialize leaves no `dest` at all
    /// — only a `.partial-*` sibling.
    #[tokio::test]
    async fn errored_materialize_leaves_no_dest_file() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();

        // Reference a chunk hash that was never stored. The
        // chunk loop's first iteration will hit `get_chunk` and
        // surface NotFound.
        let phantom = crate::manifest::ChunkHash::of(b"never-stored");
        let bad_manifest = Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes: 4096,
            chunk_size: crate::manifest::ChunkSize::bytes(4096),
            chunks: vec![crate::manifest::ChunkRef {
                offset: 0,
                hash: phantom,
            }],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };

        let dest = work.path().join("rootfs.ext4");
        assert!(s.materialize_to_file(&bad_manifest, &dest).await.is_err());
        assert!(
            !dest.exists(),
            "errored materialize must NOT leave the canonical file behind"
        );
        // The partial may exist (atomic rename happens only on
        // success); verify the canonical name is clean. Production
        // callers probe `dest` for the fast-path; that probe must
        // miss after an error.
    }

    /// Same invariant for the cached path. Different code path —
    /// must also be temp-and-rename clean.
    #[tokio::test]
    async fn errored_materialize_cached_leaves_no_dest_file() {
        let (s, _d) = store().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new(ChunkCacheConfig {
            root: cache_dir.path().to_path_buf(),
            budget_bytes: 1024 * 1024,
        });
        let work = tempfile::tempdir().unwrap();

        let phantom = crate::manifest::ChunkHash::of(b"never-stored-cached");
        let bad_manifest = Manifest {
            schema_version: 1,
            kind: ManifestKind::Disk,
            total_bytes: 4096,
            chunk_size: crate::manifest::ChunkSize::bytes(4096),
            chunks: vec![crate::manifest::ChunkRef {
                offset: 0,
                hash: phantom,
            }],
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };

        let dest = work.path().join("rootfs.ext4");
        assert!(s
            .materialize_to_file_cached(&bad_manifest, &dest, &cache)
            .await
            .is_err());
        assert!(
            !dest.exists(),
            "errored cached materialize must NOT leave the canonical file behind"
        );
    }

    /// ADR 0028 Fix A: the incremental checkpoint re-chunk. Mutate a
    /// couple of ranges of a chunked image, update via
    /// `update_for_dirty_ranges`, and assert (a) the new manifest
    /// materializes byte-identical to the mutated file, (b) untouched
    /// chunks carry the EXACT same hashes (no re-upload), (c) a chunk
    /// dirtied to all-zero drops out of the manifest (sparse
    /// invariant).
    #[tokio::test]
    async fn update_for_dirty_ranges_is_incremental_and_byte_faithful() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();

        // 4 chunks of 4 KiB, distinct non-zero content.
        let cs: u64 = 4096;
        let mut content = Vec::new();
        for b in [0xAAu8, 0xBB, 0xCC, 0xDD] {
            content.extend(std::iter::repeat_n(b, cs as usize));
        }
        let img = work.path().join("mem.bin");
        tokio::fs::write(&img, &content).await.unwrap();
        let prev = s
            .chunk_file(&img, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();
        assert_eq!(prev.chunks.len(), 4);

        // Dirty chunk 1 (new content) + chunk 3 (all zeros). Chunk 0
        // and 2 untouched.
        let mut mutated = content.clone();
        mutated[cs as usize..2 * cs as usize].fill(0x11);
        mutated[3 * cs as usize..].fill(0x00);
        tokio::fs::write(&img, &mutated).await.unwrap();

        let next = s
            .update_for_dirty_ranges(&prev, &img, &[(cs, cs), (3 * cs, cs)])
            .await
            .unwrap();

        // (b) untouched chunks carry over hash-identical.
        let hash_at =
            |m: &Manifest, off: u64| m.chunks.iter().find(|c| c.offset == off).map(|c| c.hash);
        assert_eq!(hash_at(&next, 0), hash_at(&prev, 0));
        assert_eq!(hash_at(&next, 2 * cs), hash_at(&prev, 2 * cs));
        assert_ne!(hash_at(&next, cs), hash_at(&prev, cs));
        // (c) the zeroed chunk is elided.
        assert_eq!(hash_at(&next, 3 * cs), None);
        assert_eq!(next.total_bytes, prev.total_bytes);

        // (a) full byte fidelity through materialize.
        let out = work.path().join("out.bin");
        s.materialize_to_file(&next, &out).await.unwrap();
        let round = tokio::fs::read(&out).await.unwrap();
        assert_eq!(
            round, mutated,
            "incremental manifest must reproduce the mutated image"
        );
    }

    // ---- ADR 0038: sparse re-chunk (no rolling memfile) ----

    fn keys(m: &Manifest) -> Vec<(u64, crate::manifest::ChunkHash)> {
        m.chunks.iter().map(|c| (c.offset, c.hash)).collect()
    }

    /// Build an FC-diff-like sparse file: `total` bytes, holes
    /// everywhere except the supplied data extents.
    async fn write_sparse_diff(path: &Path, total: u64, extents: &[(u64, Vec<u8>)]) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await
            .unwrap();
        f.set_len(total).await.unwrap();
        for (off, bytes) in extents {
            f.seek(SeekFrom::Start(*off)).await.unwrap();
            f.write_all(bytes).await.unwrap();
        }
        f.flush().await.unwrap();
    }

    /// The core invariant: a sparse re-chunk (prev chunks + sparse diff,
    /// no merged full image) yields a manifest byte-identical to a full
    /// `chunk_file` re-chunk of the post-diff image — including
    /// hash-identical carryover of untouched chunks and elision of a
    /// chunk dirtied to all-zero.
    #[tokio::test]
    async fn update_for_dirty_ranges_sparse_matches_full_rechunk() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let cs: u64 = 4096;

        let mut prev_img = Vec::new();
        for b in [0xAAu8, 0xBB, 0xCC, 0xDD] {
            prev_img.extend(std::iter::repeat_n(b, cs as usize));
        }
        let p = work.path().join("prev.bin");
        fs::write(&p, &prev_img).await.unwrap();
        let prev = s
            .chunk_file(&p, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        // chunk 1 → 0x11; chunk 3 → all zeros (elided). 0 + 2 clean.
        let mut cur = prev_img.clone();
        cur[cs as usize..2 * cs as usize].fill(0x11);
        cur[3 * cs as usize..].fill(0x00);
        let cur_path = work.path().join("cur.bin");
        fs::write(&cur_path, &cur).await.unwrap();
        let ground = s
            .chunk_file(&cur_path, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        let diff = work.path().join("mem.diff");
        write_sparse_diff(
            &diff,
            prev.total_bytes,
            &[
                (cs, vec![0x11u8; cs as usize]),
                (3 * cs, vec![0x00u8; cs as usize]),
            ],
        )
        .await;
        let got = s
            .update_for_dirty_ranges_sparse(&prev, &diff, &[(cs, cs), (3 * cs, cs)])
            .await
            .unwrap();

        assert_eq!(
            keys(&got),
            keys(&ground),
            "sparse re-chunk must equal a full re-chunk of the post-diff image"
        );
        let hash_at =
            |m: &Manifest, off: u64| m.chunks.iter().find(|c| c.offset == off).map(|c| c.hash);
        assert_eq!(
            hash_at(&got, 0),
            hash_at(&prev, 0),
            "clean chunk 0 carries over"
        );
        assert_eq!(
            hash_at(&got, 2 * cs),
            hash_at(&prev, 2 * cs),
            "clean chunk 2 carries over"
        );
        assert_eq!(hash_at(&got, 3 * cs), None, "zeroed chunk 3 elided");

        let out = work.path().join("out.bin");
        s.materialize_to_file(&got, &out).await.unwrap();
        assert_eq!(fs::read(&out).await.unwrap(), cur);
    }

    /// A dirty chunk whose prev offset is ELIDED (all-zero in prev) must
    /// seed from zeros, then take the partial overlay.
    #[tokio::test]
    async fn update_for_dirty_ranges_sparse_zero_prev_chunk() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let cs: u64 = 4096;

        let mut prev_img = vec![0u8; 2 * cs as usize];
        prev_img[cs as usize..].fill(0xBB);
        let p = work.path().join("prev.bin");
        fs::write(&p, &prev_img).await.unwrap();
        let prev = s
            .chunk_file(&p, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();
        assert_eq!(prev.chunks.len(), 1, "chunk 0 is all-zero → elided in prev");

        // 0x33 into the FIRST HALF of chunk 0; second half stays zero.
        let mut cur = prev_img.clone();
        cur[0..(cs / 2) as usize].fill(0x33);
        let cur_path = work.path().join("cur.bin");
        fs::write(&cur_path, &cur).await.unwrap();
        let ground = s
            .chunk_file(&cur_path, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        let diff = work.path().join("mem.diff");
        write_sparse_diff(
            &diff,
            prev.total_bytes,
            &[(0, vec![0x33u8; (cs / 2) as usize])],
        )
        .await;
        let got = s
            .update_for_dirty_ranges_sparse(&prev, &diff, &[(0, cs / 2)])
            .await
            .unwrap();

        assert_eq!(keys(&got), keys(&ground));
        let out = work.path().join("out.bin");
        s.materialize_to_file(&got, &out).await.unwrap();
        assert_eq!(fs::read(&out).await.unwrap(), cur);
    }

    /// An unaligned interior dirty range over a non-zero prev chunk:
    /// merged bytes = prev outside the range, diff inside it.
    #[tokio::test]
    async fn update_for_dirty_ranges_sparse_partial_unaligned() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let cs: u64 = 4096;

        let prev_img = vec![0xCDu8; cs as usize];
        let p = work.path().join("prev.bin");
        fs::write(&p, &prev_img).await.unwrap();
        let prev = s
            .chunk_file(&p, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        let mut cur = prev_img.clone();
        cur[100..200].fill(0x77);
        let cur_path = work.path().join("cur.bin");
        fs::write(&cur_path, &cur).await.unwrap();
        let ground = s
            .chunk_file(&cur_path, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        let diff = work.path().join("mem.diff");
        write_sparse_diff(&diff, prev.total_bytes, &[(100, vec![0x77u8; 100])]).await;
        let got = s
            .update_for_dirty_ranges_sparse(&prev, &diff, &[(100, 100)])
            .await
            .unwrap();

        assert_eq!(keys(&got), keys(&ground));
        let out = work.path().join("out.bin");
        s.materialize_to_file(&got, &out).await.unwrap();
        assert_eq!(fs::read(&out).await.unwrap(), cur);
    }

    /// `total_bytes` is taken from `prev`, not the diff file's length:
    /// a diff truncated to just the dirty extent still reconstructs the
    /// full image.
    #[tokio::test]
    async fn update_for_dirty_ranges_sparse_total_bytes_from_prev_and_short_final() {
        let (s, _d) = store().await;
        let work = tempfile::tempdir().unwrap();
        let cs: u64 = 4096;
        let total = 2 * cs + 13; // short final chunk

        let mut prev_img = vec![0x10u8; total as usize];
        prev_img[cs as usize..2 * cs as usize].fill(0x20);
        prev_img[2 * cs as usize..].fill(0x30);
        let p = work.path().join("prev.bin");
        fs::write(&p, &prev_img).await.unwrap();
        let prev = s
            .chunk_file(&p, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        // dirty the short final chunk only.
        let mut cur = prev_img.clone();
        cur[2 * cs as usize..].fill(0x99);
        let cur_path = work.path().join("cur.bin");
        fs::write(&cur_path, &cur).await.unwrap();
        let ground = s
            .chunk_file(&cur_path, ManifestKind::Memory, Some(cs))
            .await
            .unwrap();

        // diff file is only 13 bytes past 2*cs — far shorter than `total`.
        let diff = work.path().join("mem.diff");
        write_sparse_diff(&diff, 2 * cs + 13, &[(2 * cs, vec![0x99u8; 13])]).await;
        let got = s
            .update_for_dirty_ranges_sparse(&prev, &diff, &[(2 * cs, 13)])
            .await
            .unwrap();

        assert_eq!(got.total_bytes, total);
        assert_eq!(keys(&got), keys(&ground));
        let out = work.path().join("out.bin");
        s.materialize_to_file(&got, &out).await.unwrap();
        assert_eq!(fs::read(&out).await.unwrap(), cur);
    }
}
