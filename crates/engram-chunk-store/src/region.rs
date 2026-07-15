//! ADR 0093: streaming region → chunk pipeline (the fused pack+chunk).
//!
//! [`RegionChunker`] is the synchronous receiving end of a streaming
//! image producer (the mkext4 writer, adapted in
//! `engram-rootfs-materializer`): the producer emits every byte of
//! `[0, total_bytes)` exactly once — real bytes via [`RegionChunker::data`],
//! untouched space via [`RegionChunker::zeros`] — and each fixed-size
//! chunk is hashed and PUT the moment its byte range is fully covered.
//! No image file exists anywhere: the producing host's local NVMe cache
//! is seeded by `put_chunk`'s write-through (ADR 0078 move 4), which is
//! what the co-located capture VM's NBD page-in reads.
//!
//! All-zero chunks are elided from the manifest exactly as
//! [`ChunkStore::chunk_file_into`] elides them (a manifest gap reads as
//! zero-fill), so a region-built manifest of some bytes is
//! `content_ref`-identical to a file-built manifest of the same bytes.
//!
//! Memory model: partially-covered chunks buffer only their *data*
//! fragments — zero runs are stored as lengths. With the mkext4 layout
//! (dense metadata prefix, data ascending behind) the alive-partial
//! set stays small; a defensive cap turns a pathological producer into
//! an error instead of an OOM.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};

use crate::manifest::{ChunkRef, ChunkSize, Manifest, ManifestKind};
use crate::store::ChunkStore;

/// Upload width, matching `chunk_file_into`'s pipeline: enough to
/// hide per-PUT latency, small enough that the host-global
/// [`UploadBudget`](crate::budget::UploadBudget) stays the arbiter
/// under concurrent workloads.
const REGION_UPLOAD_CONCURRENCY: usize = 64;

/// Completed chunks in flight from the sync producer to the async
/// uploader. 8 × 16 MiB = 128 MiB of backpressure: the fill pass
/// stalls when uploads fall behind rather than ballooning memory.
const COMPLETED_CHANNEL_DEPTH: usize = 8;

/// Defensive cap on bytes buffered inside partially-covered chunks.
/// The mkext4 emission order keeps this in the tens of MiB; blowing
/// past it means the producer violated its layout contract, and an
/// error beats an OOM on a shared host.
const PARTIAL_BUFFER_CAP: u64 = 2 * 1024 * 1024 * 1024;

/// Stats from a completed region-chunk run.
#[derive(Debug, Clone, Copy, Default)]
pub struct RegionChunkStats {
    /// Chunks PUT to the store (non-zero content).
    pub chunks_uploaded: u64,
    /// All-zero chunks elided from the manifest.
    pub chunks_elided: u64,
    /// Bytes handed to `put_chunk` (pre-dedup).
    pub bytes_uploaded: u64,
    /// High-water mark of bytes buffered in partial chunks — the
    /// producer-layout health metric.
    pub partial_buffer_high_water: u64,
}

/// One finalized chunk travelling to the uploader. `None` body =
/// all-zero chunk (progress-counted, never uploaded).
enum Completed {
    Chunk { offset: u64, body: Vec<u8> },
    Zero,
}

/// A partially-covered chunk: data fragments buffered, zero runs
/// counted only.
#[derive(Default)]
struct Partial {
    covered: u64,
    /// (offset-within-chunk, bytes) data fragments, unordered.
    frags: Vec<(u32, Vec<u8>)>,
}

/// Synchronous receiving end — hand this to the blocking producer
/// thread. Every byte of `[0, total_bytes)` must be covered exactly
/// once across `data`/`zeros`; `finish` enforces full coverage.
pub struct RegionChunker {
    chunk_size: u64,
    total_bytes: u64,
    partials: BTreeMap<u64, Partial>,
    /// Chunks already emitted — re-covering one is the same
    /// exactly-once violation as double-covering within a partial.
    emitted: std::collections::BTreeSet<u64>,
    buffered: u64,
    high_water: u64,
    covered_total: u64,
    tx: tokio::sync::mpsc::Sender<Completed>,
}

impl RegionChunker {
    /// Real bytes at `offset`.
    pub fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.cover(offset, bytes.len() as u64, Some(bytes))
    }

    /// A zero run at `offset`.
    pub fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.cover(offset, len, None)
    }

    /// Producer done. Verifies exactly-once/full coverage and closes
    /// the channel so the uploader can finish its manifest.
    pub fn finish(self) -> io::Result<u64> {
        if self.covered_total != self.total_bytes || !self.partials.is_empty() {
            return Err(io::Error::other(format!(
                "region producer under-covered the image: {} of {} bytes, {} partial chunks",
                self.covered_total,
                self.total_bytes,
                self.partials.len()
            )));
        }
        Ok(self.high_water)
        // tx drops here → channel closes → uploader drains + returns.
    }

    fn cover(&mut self, offset: u64, len: u64, bytes: Option<&[u8]>) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len)
            .filter(|e| *e <= self.total_bytes)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "region [{offset}, +{len}) exceeds image length {}",
                    self.total_bytes
                ))
            })?;
        self.covered_total += len;

        // Split the run at chunk boundaries.
        let mut pos = offset;
        while pos < end {
            let idx = pos / self.chunk_size;
            let chunk_start = idx * self.chunk_size;
            let chunk_len = self.chunk_len(idx);
            let take = (chunk_start + chunk_len).min(end) - pos;

            if self.emitted.contains(&idx) {
                return Err(io::Error::other(format!(
                    "chunk {idx} over-covered: byte range emitted twice"
                )));
            }
            let entry = self.partials.entry(idx).or_default();
            entry.covered += take;
            if entry.covered > chunk_len {
                self.partials.remove(&idx);
                return Err(io::Error::other(format!(
                    "chunk {idx} over-covered: byte range emitted twice"
                )));
            }
            if let Some(b) = bytes {
                let s = (pos - offset) as usize;
                let frag = &b[s..s + take as usize];
                // A fragment of all zeros needs no buffer — identical
                // to a zeros() run for assembly purposes.
                if frag.iter().any(|&x| x != 0) {
                    entry
                        .frags
                        .push(((pos - chunk_start) as u32, frag.to_vec()));
                    self.buffered += take;
                    self.high_water = self.high_water.max(self.buffered);
                    if self.buffered > PARTIAL_BUFFER_CAP {
                        return Err(io::Error::other(
                            "partial-chunk buffer cap exceeded: producer emission order \
                             violates the dense-prefix layout contract",
                        ));
                    }
                }
            }
            if entry.covered == chunk_len {
                let done = self.partials.remove(&idx).expect("entry just touched");
                self.emitted.insert(idx);
                self.emit(idx, chunk_len, done)?;
            }
            pos += take;
        }
        Ok(())
    }

    fn emit(&mut self, idx: u64, chunk_len: u64, p: Partial) -> io::Result<()> {
        let msg = if p.frags.is_empty() {
            Completed::Zero
        } else {
            let mut body = vec![0u8; chunk_len as usize];
            let mut frag_bytes = 0u64;
            for (off, frag) in p.frags {
                frag_bytes += frag.len() as u64;
                body[off as usize..off as usize + frag.len()].copy_from_slice(&frag);
            }
            self.buffered -= frag_bytes;
            Completed::Chunk {
                offset: idx * self.chunk_size,
                body,
            }
        };
        self.tx.blocking_send(msg).map_err(|_| {
            io::Error::other("chunk uploader terminated (upload failure); aborting producer")
        })
    }

    fn chunk_len(&self, idx: u64) -> u64 {
        let start = idx * self.chunk_size;
        self.chunk_size.min(self.total_bytes - start)
    }
}

impl ChunkStore {
    /// ADR 0093: open a streaming region → chunk pipeline for a
    /// `total_bytes` image. Returns the synchronous [`RegionChunker`]
    /// (move it into the blocking producer thread) and a handle whose
    /// `.await` yields the finished manifest after the producer calls
    /// [`RegionChunker::finish`] and all uploads land.
    ///
    /// `progress(done, total)` counts finalized chunks (uploaded,
    /// deduped, and zero-elided alike) so callers get the same
    /// monotone fraction `chunk_file_into` reports.
    pub fn region_chunker(
        &self,
        kind: ManifestKind,
        total_bytes: u64,
        progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
    ) -> (
        RegionChunker,
        tokio::task::JoinHandle<crate::Result<(Manifest, RegionChunkStats)>>,
    ) {
        let chunk_size = kind.default_chunk_size();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Completed>(COMPLETED_CHANNEL_DEPTH);
        let chunker = RegionChunker {
            chunk_size,
            total_bytes,
            partials: BTreeMap::new(),
            emitted: std::collections::BTreeSet::new(),
            buffered: 0,
            high_water: 0,
            covered_total: 0,
            tx,
        };
        let store = self.clone();
        let total_chunks = total_bytes.div_ceil(chunk_size).max(1);
        let uploader = tokio::spawn(async move {
            let mut stats = RegionChunkStats::default();
            let mut chunks: Vec<ChunkRef> = Vec::new();
            let mut inflight = FuturesUnordered::new();
            let mut done: u64 = 0;
            let report = |done: u64| {
                if let Some(p) = &progress {
                    p(done, total_chunks);
                }
            };
            loop {
                tokio::select! {
                    // Bias toward draining completions so `inflight`
                    // stays near the cap instead of queueing sends.
                    biased;
                    Some(result) = inflight.next(), if !inflight.is_empty() => {
                        let (offset, hash) = result?;
                        chunks.push(ChunkRef { offset, hash });
                        done += 1;
                        if done.is_multiple_of(32) { report(done); }
                    }
                    msg = rx.recv(), if inflight.len() < REGION_UPLOAD_CONCURRENCY => {
                        match msg {
                            Some(Completed::Chunk { offset, body }) => {
                                stats.chunks_uploaded += 1;
                                stats.bytes_uploaded += body.len() as u64;
                                let store = store.clone();
                                inflight.push(async move {
                                    let hash = store.put_chunk(&body).await?;
                                    crate::Result::Ok((offset, hash))
                                });
                            }
                            Some(Completed::Zero) => {
                                stats.chunks_elided += 1;
                                done += 1;
                                if done.is_multiple_of(32) { report(done); }
                            }
                            None => break,
                        }
                    }
                }
            }
            while let Some(result) = inflight.next().await {
                let (offset, hash) = result?;
                chunks.push(ChunkRef { offset, hash });
                done += 1;
            }
            report(done);
            chunks.sort_by_key(|c| c.offset);
            let mut manifest = Manifest::empty(kind, total_bytes);
            manifest.chunk_size = ChunkSize::bytes(chunk_size);
            manifest.chunks = chunks;
            Ok((manifest, stats))
        });
        (chunker, uploader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;

    fn store() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (ChunkStore::new(blob), dir)
    }

    /// Deterministic pseudo-random bytes (no RNG in tests).
    fn pattern(len: usize, salt: u8) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u64 * 2654435761 + salt as u64) >> 3) as u8)
            .collect()
    }

    /// Region-built and file-built manifests of the same bytes must be
    /// content_ref-identical — including zero elision.
    #[tokio::test(flavor = "multi_thread")]
    async fn region_manifest_matches_chunk_file_manifest() {
        let (store, _dir) = store();
        let chunk = ManifestKind::Disk.default_chunk_size() as usize;

        // 3.5 chunks: [pattern][all-zero][pattern][half chunk tail, pattern]
        let mut image = Vec::new();
        image.extend(pattern(chunk, 1));
        image.extend(vec![0u8; chunk]);
        image.extend(pattern(chunk, 2));
        image.extend(pattern(chunk / 2, 3));

        // File path.
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("img.bin");
        std::fs::write(&path, &image).unwrap();
        let (file_manifest, _) = store
            .chunk_file_into(&path, ManifestKind::Disk, None, None, None)
            .await
            .unwrap();

        // Region path: emit out of order (chunk 2 data, then 0, then the
        // zero chunk as a zeros() run, then the tail), fragmented.
        let (mut rc, uploader) = store.region_chunker(ManifestKind::Disk, image.len() as u64, None);
        let img = image.clone();
        let producer = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            let c = chunk as u64;
            rc.data(2 * c, &img[2 * chunk..3 * chunk])?;
            rc.data(0, &img[..chunk / 2])?;
            rc.zeros(c, c)?;
            rc.data(c / 2, &img[chunk / 2..chunk])?;
            rc.data(3 * c, &img[3 * chunk..])?;
            rc.finish()
        });
        producer.await.unwrap().unwrap();
        let (region_manifest, stats) = uploader.await.unwrap().unwrap();

        assert_eq!(
            region_manifest.content_ref(),
            file_manifest.content_ref(),
            "region-built manifest must be bit-identical to file-built"
        );
        assert_eq!(stats.chunks_uploaded, 3);
        assert_eq!(stats.chunks_elided, 1);
    }

    /// Double-covering a byte is a producer bug and must error, not
    /// silently corrupt.
    #[tokio::test(flavor = "multi_thread")]
    async fn over_coverage_is_an_error() {
        let (store, _dir) = store();
        let (mut rc, _uploader) = store.region_chunker(ManifestKind::Disk, 4096, None);
        // blocking_send inside — must run off the async runtime, like
        // the real producer does.
        let err = tokio::task::spawn_blocking(move || {
            rc.data(0, &pattern(4096, 1)).unwrap();
            // Chunk 0 is emitted; touching it again is exactly-once
            // violation even though its partial is gone.
            rc.zeros(0, 1).unwrap_err()
        })
        .await
        .unwrap();
        assert!(err.to_string().contains("over-covered"), "{err}");
    }

    /// Under-coverage is caught at finish().
    #[tokio::test(flavor = "multi_thread")]
    async fn under_coverage_fails_finish() {
        let (store, _dir) = store();
        let (mut rc, _uploader) = store.region_chunker(ManifestKind::Disk, 8192, None);
        rc.data(0, &pattern(4096, 1)).unwrap();
        let err = rc.finish().unwrap_err();
        assert!(err.to_string().contains("under-covered"), "{err}");
    }

    /// All-zero data() fragments cost no buffer and elide like zeros().
    #[tokio::test(flavor = "multi_thread")]
    async fn zero_data_fragments_elide() {
        let (store, _dir) = store();
        let len = ManifestKind::Disk.default_chunk_size();
        let (mut rc, uploader) = store.region_chunker(ManifestKind::Disk, len, None);
        let producer = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            rc.data(0, &vec![0u8; len as usize])?;
            rc.finish()
        });
        let high_water = producer.await.unwrap().unwrap();
        let (manifest, stats) = uploader.await.unwrap().unwrap();
        assert_eq!(high_water, 0, "all-zero fragments must not buffer");
        assert!(manifest.chunks.is_empty());
        assert_eq!(stats.chunks_elided, 1);
    }

    /// Progress counts every chunk once (uploaded + elided) and ends
    /// exactly at total.
    #[tokio::test(flavor = "multi_thread")]
    async fn progress_is_monotone_and_complete() {
        let (store, _dir) = store();
        let chunk = ManifestKind::Disk.default_chunk_size();
        let total = chunk * 3;
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<(u64, u64)>::new()));
        let seen2 = Arc::clone(&seen);
        let progress: Arc<dyn Fn(u64, u64) + Send + Sync> =
            Arc::new(move |d, t| seen2.lock().push((d, t)));
        let (mut rc, uploader) = store.region_chunker(ManifestKind::Disk, total, Some(progress));
        let producer = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            rc.data(0, &pattern(chunk as usize, 1))?;
            rc.zeros(chunk, chunk * 2)?;
            rc.finish()
        });
        producer.await.unwrap().unwrap();
        uploader.await.unwrap().unwrap();
        let seen = seen.lock();
        let last = seen.last().copied().unwrap();
        assert_eq!(last, (3, 3));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "monotone");
    }
}
