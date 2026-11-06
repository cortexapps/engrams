//! [`DiskReader`]: the bytes of one disk manifest at any offset.
//!
//! A read takes each chunk it spans from the first tier that holds it:
//! a staged local volume with a chunk of the same hash, then the host's
//! chunk cache (read-only), then the chunk store. An offset the manifest
//! lists no chunk for reads as zeros and fetches nothing. Recently used
//! chunks stay in memory, and concurrent reads of one chunk share one
//! fetch.

use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use engram_chunk_store::reader::ChunkCacheReader;
use engram_chunk_store::{ChunkHash, ChunkStore, Manifest, ManifestKind};
use parking_lot::Mutex;
use tokio::sync::OnceCell;

/// Chunks kept in memory when the caller sets no other bound.
pub const DEFAULT_MEMO_CHUNKS: usize = 8;

/// A staged volume and the manifest it holds, indexed by chunk hash.
#[derive(Clone)]
pub struct LocalBase {
    file: Arc<File>,
    size_bytes: u64,
    offset_of: HashMap<ChunkHash, u64>,
}

impl LocalBase {
    /// Open the volume at `path` read-only. `manifest` must describe
    /// what the volume holds.
    pub fn open(path: &Path, manifest: &Manifest) -> io::Result<Self> {
        manifest.validate().map_err(io::Error::other)?;
        let mut file = File::open(path)?;
        // A block device reports no length in its metadata; its end does.
        let size_bytes = file.seek(SeekFrom::End(0))?;
        if size_bytes < manifest.total_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "base volume {} is {size_bytes} bytes; its manifest is {} bytes",
                    path.display(),
                    manifest.total_bytes
                ),
            ));
        }
        Ok(Self {
            file: Arc::new(file),
            size_bytes,
            offset_of: manifest.chunks.iter().map(|c| (c.hash, c.offset)).collect(),
        })
    }

    /// The chunk `hash` at `offset`; bytes that do not hash to `hash`
    /// are refused.
    async fn read(&self, hash: ChunkHash, offset: u64, len: usize) -> io::Result<Bytes> {
        if offset.saturating_add(len as u64) > self.size_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("base read {offset}+{len} is past the end of the volume"),
            ));
        }
        let file = Arc::clone(&self.file);
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            file.read_exact_at(&mut buf, offset)?;
            drop_cached_pages(&file, offset, len);
            if ChunkHash::of(&buf) != hash {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the {len} bytes at {offset} do not hash to {hash}"),
                ));
            }
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(|e| io::Error::other(format!("base read task: {e}")))?
    }
}

/// The source keeps the chunk in its own memory, so the host page cache
/// gains nothing from a second copy.
#[cfg(target_os = "linux")]
fn drop_cached_pages(file: &File, offset: u64, len: usize) {
    use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};
    let _ = posix_fadvise(
        file,
        offset as i64,
        len as i64,
        PosixFadviseAdvice::POSIX_FADV_DONTNEED,
    );
}

#[cfg(not(target_os = "linux"))]
fn drop_cached_pages(_file: &File, _offset: u64, _len: usize) {}

/// The bytes of one disk manifest.
pub struct DiskReader {
    chunk_size: u64,
    total_bytes: u64,
    /// Hash of the chunk at each index; `None` reads as zeros.
    chunks: Vec<Option<ChunkHash>>,
    tiers: Arc<Tiers>,
    memo: Memo,
}

/// Where a chunk's bytes come from, first match wins.
#[derive(Clone)]
struct Tiers {
    base: Option<LocalBase>,
    cache: Option<ChunkCacheReader>,
    store: ChunkStore,
}

impl DiskReader {
    /// A reader of `manifest` that fetches from `store`.
    pub fn new(manifest: &Manifest, store: ChunkStore) -> io::Result<Self> {
        manifest.validate().map_err(io::Error::other)?;
        if manifest.kind != ManifestKind::Disk {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a disk source serves disk manifests only",
            ));
        }
        let chunk_size = manifest.chunk_size.as_u64();
        let mut chunks = vec![None; manifest.expected_chunk_count() as usize];
        for c in &manifest.chunks {
            chunks[(c.offset / chunk_size) as usize] = Some(c.hash);
        }
        Ok(Self {
            chunk_size,
            total_bytes: manifest.total_bytes,
            chunks,
            tiers: Arc::new(Tiers {
                base: None,
                cache: None,
                store,
            }),
            memo: Memo::new(DEFAULT_MEMO_CHUNKS),
        })
    }

    /// Serve chunks the staged volume `base` holds from it.
    pub fn with_base(mut self, base: LocalBase) -> Self {
        let mut tiers = Arc::unwrap_or_clone(self.tiers);
        tiers.base = Some(base);
        self.tiers = Arc::new(tiers);
        self
    }

    /// Serve chunks resident in the host chunk cache from it. The source
    /// only reads the cache; the host-agent owns its contents.
    pub fn with_cache(mut self, cache: ChunkCacheReader) -> Self {
        let mut tiers = Arc::unwrap_or_clone(self.tiers);
        tiers.cache = Some(cache);
        self.tiers = Arc::new(tiers);
        self
    }

    /// Keep at most `chunks` (at least one) chunks in memory.
    pub fn with_memo_chunks(mut self, chunks: usize) -> Self {
        self.memo = Memo::new(chunks);
        self
    }

    pub fn size_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// `len` bytes at `offset`; the range must lie inside the disk.
    pub async fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let end = offset
            .checked_add(len as u64)
            .filter(|&end| end <= self.total_bytes)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "read {offset}+{len} is past the end of the disk ({} bytes)",
                        self.total_bytes
                    ),
                )
            })?;
        let mut out = vec![0u8; len];
        if len == 0 {
            return Ok(out);
        }
        let first = offset / self.chunk_size;
        let last = (end - 1) / self.chunk_size;
        let spans = (first..=last).filter_map(|index| {
            let hash = self.chunks[index as usize]?;
            Some(async move { Ok::<_, io::Error>((index, self.chunk(index, hash).await?)) })
        });
        for (index, bytes) in futures::future::try_join_all(spans).await? {
            let chunk_start = index * self.chunk_size;
            let from = offset.max(chunk_start);
            let to = end.min(chunk_start + self.chunk_size);
            out[(from - offset) as usize..(to - offset) as usize].copy_from_slice(
                &bytes[(from - chunk_start) as usize..(to - chunk_start) as usize],
            );
        }
        Ok(out)
    }

    /// The fetch runs in a task of its own, so the network, hashing and
    /// copying of different chunks spread across the runtime's workers.
    async fn chunk(&self, index: u64, hash: ChunkHash) -> io::Result<Bytes> {
        let start = index * self.chunk_size;
        let len = (self.total_bytes - start).min(self.chunk_size) as usize;
        let tiers = Arc::clone(&self.tiers);
        self.memo
            .get_or_fetch(hash, || async move {
                tokio::spawn(async move { tiers.fetch(hash, len).await })
                    .await
                    .map_err(|e| io::Error::other(format!("chunk fetch task: {e}")))?
            })
            .await
    }
}

impl Tiers {
    async fn fetch(&self, hash: ChunkHash, len: usize) -> io::Result<Bytes> {
        if let Some(base) = &self.base {
            if let Some(&offset) = base.offset_of.get(&hash) {
                match base.read(hash, offset, len).await {
                    Ok(bytes) => return Ok(bytes),
                    Err(e) => {
                        tracing::warn!(%hash, offset, error = %e, "base volume read failed; reading the chunk from the store")
                    }
                }
            }
        }
        if let Some(cache) = &self.cache {
            let cache = cache.clone();
            let found = tokio::task::spawn_blocking(move || cache.read(hash))
                .await
                .map_err(|e| io::Error::other(format!("cache read task: {e}")))?;
            match found {
                Ok(Some(bytes)) if bytes.len() == len => return Ok(Bytes::from(bytes)),
                Ok(Some(bytes)) => {
                    tracing::warn!(%hash, len = bytes.len(), want = len, "cached chunk has the wrong length; reading it from the store")
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(%hash, error = %e, "chunk cache read failed; reading the chunk from the store")
                }
            }
        }
        let bytes = self.store.get_chunk(hash).await.map_err(store_error)?;
        if bytes.len() != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chunk {hash} is {} bytes; the manifest needs {len}",
                    bytes.len()
                ),
            ));
        }
        Ok(bytes)
    }
}

/// A chunk the store does not hold reads as `NotFound`, and bytes that
/// fail the hash check as `InvalidData`: waiting does not repair either.
/// Every other store error may pass.
fn store_error(e: engram_chunk_store::ChunkStoreError) -> io::Error {
    use engram_chunk_store::ChunkStoreError;
    use engram_core::error::BlobError;
    let kind = match &e {
        ChunkStoreError::Blob(BlobError::NotFound) => io::ErrorKind::NotFound,
        ChunkStoreError::HashMismatch { .. } => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, e)
}

/// Recently used chunks, least recently used first out. One cell per
/// chunk: the first reader fetches and the others wait on the cell. A
/// failed fetch leaves the cell empty, so the next reader fetches again.
/// A cell that a reader holds is never evicted, so concurrent reads of
/// one chunk share one fetch even when more chunks are in flight than
/// the memo keeps; the memo then holds more cells until those reads end.
struct Memo {
    capacity: usize,
    state: Mutex<MemoState>,
}

#[derive(Default)]
struct MemoState {
    clock: u64,
    cells: HashMap<ChunkHash, (u64, Arc<OnceCell<Bytes>>)>,
}

impl Memo {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            state: Mutex::default(),
        }
    }

    async fn get_or_fetch<F, Fut>(&self, hash: ChunkHash, fetch: F) -> io::Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = io::Result<Bytes>>,
    {
        self.cell(hash).get_or_try_init(fetch).await.cloned()
    }

    fn cell(&self, hash: ChunkHash) -> Arc<OnceCell<Bytes>> {
        let mut state = self.state.lock();
        state.clock += 1;
        let now = state.clock;
        let entry = state
            .cells
            .entry(hash)
            .or_insert_with(|| (now, Arc::new(OnceCell::new())));
        entry.0 = now;
        let cell = Arc::clone(&entry.1);
        let excess = state.cells.len().saturating_sub(self.capacity);
        if excess > 0 {
            // Cells are cloned only under this lock, so a count of one
            // means no reader holds the cell. The cell just returned is
            // held.
            let mut idle: Vec<(u64, ChunkHash)> = state
                .cells
                .iter()
                .filter(|(_, (_, cell))| Arc::strong_count(cell) == 1)
                .map(|(hash, (used, _))| (*used, *hash))
                .collect();
            idle.sort_unstable_by_key(|(used, _)| *used);
            for (_, hash) in idle.into_iter().take(excess) {
                state.cells.remove(&hash);
            }
        }
        cell
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use engram_chunk_store::{ChunkRef, ChunkSize};
    use engram_core::error::BlobError;
    use engram_core::traits::storage::{BlobObjectMeta, BlobStorage, ByteStream};
    use engram_storage_local::LocalBlobStorage;
    use engram_testkit::storage::{Counters, FaultPlan, FaultyBlobStorage};
    use proptest::prelude::*;

    const CHUNK: u64 = 64 * 1024;

    /// A disk of `CHUNK`-sized chunks with a short final chunk. `None`
    /// slots are holes.
    fn layout() -> Vec<Option<u8>> {
        vec![
            Some(1),
            None,
            Some(2),
            Some(3),
            None,
            None,
            Some(4),
            Some(5),
        ]
    }

    const SHORT_TAIL: u64 = 12 * 1024;

    fn total_bytes() -> u64 {
        (layout().len() as u64 - 1) * CHUNK + SHORT_TAIL
    }

    /// Distinct, position-dependent bytes for chunk `seed`.
    fn chunk_bytes(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[seed as usize % 4] ^ seed)
            .collect()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: ChunkStore,
        counters: Arc<Counters>,
        manifest: Manifest,
        expected: Vec<u8>,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let local: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path()));
        let (blob, counters) = FaultyBlobStorage::arc(local, FaultPlan::new());
        let store = ChunkStore::new(blob);
        let total = total_bytes();
        let mut expected = vec![0u8; total as usize];
        let mut chunks = Vec::new();
        for (index, slot) in layout().into_iter().enumerate() {
            let offset = index as u64 * CHUNK;
            let len = (total - offset).min(CHUNK) as usize;
            if let Some(seed) = slot {
                let bytes = chunk_bytes(seed, len);
                let hash = store.put_chunk(&bytes).await.unwrap();
                expected[offset as usize..offset as usize + len].copy_from_slice(&bytes);
                chunks.push(ChunkRef { offset, hash });
            }
        }
        let mut manifest = Manifest::empty(ManifestKind::Disk, total);
        manifest.chunk_size = ChunkSize::bytes(CHUNK);
        manifest.chunks = chunks;
        Fixture {
            _dir: dir,
            store,
            counters,
            manifest,
            expected,
        }
    }

    /// One store and reader for every property case. A memo of two
    /// chunks makes the cases exercise eviction and refetch as well.
    static SHARED: std::sync::LazyLock<(tokio::runtime::Runtime, Fixture, DiskReader)> =
        std::sync::LazyLock::new(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .build()
                .unwrap();
            let f = rt.block_on(fixture());
            let reader = DiskReader::new(&f.manifest, f.store.clone())
                .unwrap()
                .with_memo_chunks(2);
            (rt, f, reader)
        });

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn any_read_equals_the_manifest_bytes(offset in 0..total_bytes(), len in 0usize..(3 * CHUNK as usize)) {
            let (rt, f, reader) = &*SHARED;
            let len = len.min((total_bytes() - offset) as usize);
            let got = rt.block_on(reader.read(offset, len)).unwrap();
            prop_assert_eq!(&got[..], &f.expected[offset as usize..offset as usize + len]);
        }
    }

    #[tokio::test]
    async fn a_read_past_the_end_is_refused() {
        let f = fixture().await;
        let reader = DiskReader::new(&f.manifest, f.store.clone()).unwrap();
        let err = reader.read(total_bytes() - 1, 2).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(reader.read(total_bytes(), 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn holes_read_as_zeros_and_fetch_nothing() {
        let f = fixture().await;
        let reader = DiskReader::new(&f.manifest, f.store.clone()).unwrap();
        let before = f.counters.gets_attempted();
        assert_eq!(
            reader.read(CHUNK, CHUNK as usize).await.unwrap(),
            vec![0u8; CHUNK as usize]
        );
        assert_eq!(
            reader
                .read(4 * CHUNK + 100, 2 * CHUNK as usize - 100)
                .await
                .unwrap(),
            vec![0u8; 2 * CHUNK as usize - 100]
        );
        assert_eq!(f.counters.gets_attempted(), before);
    }

    #[tokio::test]
    async fn a_chunk_the_store_does_not_hold_reads_as_not_found() {
        let f = fixture().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = ChunkStore::new(Arc::new(LocalBlobStorage::new(elsewhere.path())));
        let absent = other
            .put_chunk(&chunk_bytes(9, CHUNK as usize))
            .await
            .unwrap();
        let mut manifest = f.manifest.clone();
        manifest.chunks[0].hash = absent;
        let reader = DiskReader::new(&manifest, f.store.clone()).unwrap();
        let err = reader.read(0, 4096).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert_eq!(reader.read(2 * CHUNK, 4096).await.unwrap().len(), 4096);
    }

    #[tokio::test]
    async fn a_base_chunk_with_the_same_hash_at_any_offset_is_used_instead_of_the_store() {
        let f = fixture().await;
        // The base holds the target's chunks 0 and 2 at other offsets, and
        // not chunk 3.
        let base_dir = tempfile::tempdir().unwrap();
        let base_path = base_dir.path().join("base");
        let total = total_bytes();
        let mut base_bytes = vec![0u8; total as usize];
        let target_at =
            |index: u64| &f.expected[(index * CHUNK) as usize..((index + 1) * CHUNK) as usize];
        base_bytes[(5 * CHUNK) as usize..(6 * CHUNK) as usize].copy_from_slice(target_at(0));
        base_bytes[CHUNK as usize..(2 * CHUNK) as usize].copy_from_slice(target_at(2));
        std::fs::write(&base_path, &base_bytes).unwrap();
        let hash_of = |index: u64| f.manifest.chunk_at(index * CHUNK).unwrap().hash;
        let mut base_manifest = Manifest::empty(ManifestKind::Disk, total);
        base_manifest.chunk_size = ChunkSize::bytes(CHUNK);
        base_manifest.chunks = vec![
            ChunkRef {
                offset: CHUNK,
                hash: hash_of(2),
            },
            ChunkRef {
                offset: 5 * CHUNK,
                hash: hash_of(0),
            },
        ];
        let base = LocalBase::open(&base_path, &base_manifest).unwrap();
        let reader = DiskReader::new(&f.manifest, f.store.clone())
            .unwrap()
            .with_base(base);

        let before = f.counters.gets_attempted();
        assert_eq!(reader.read(0, CHUNK as usize).await.unwrap(), target_at(0));
        assert_eq!(
            reader.read(2 * CHUNK, CHUNK as usize).await.unwrap(),
            target_at(2)
        );
        assert_eq!(
            f.counters.gets_attempted(),
            before,
            "base chunks must not reach the store"
        );

        assert_eq!(
            reader.read(3 * CHUNK, CHUNK as usize).await.unwrap(),
            target_at(3)
        );
        assert_eq!(f.counters.gets_attempted(), before + 1);
    }

    #[tokio::test]
    async fn a_resident_cache_chunk_is_used_instead_of_the_store() {
        let f = fixture().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let hash = f.manifest.chunk_at(0).unwrap().hash;
        let path = engram_chunk_store::reader::chunk_path(cache_dir.path(), hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &f.expected[..CHUNK as usize]).unwrap();
        let reader = DiskReader::new(&f.manifest, f.store.clone())
            .unwrap()
            .with_cache(ChunkCacheReader::new(cache_dir.path()));

        let before = f.counters.gets_attempted();
        assert_eq!(reader.read(0, 4096).await.unwrap(), &f.expected[..4096]);
        assert_eq!(f.counters.gets_attempted(), before);
        assert_eq!(
            reader.read(2 * CHUNK, 4096).await.unwrap(),
            &f.expected[(2 * CHUNK) as usize..(2 * CHUNK) as usize + 4096]
        );
        assert_eq!(f.counters.gets_attempted(), before + 1);
    }

    #[tokio::test]
    async fn the_memo_is_bounded_least_recently_used_first() {
        let f = fixture().await;
        let reader = DiskReader::new(&f.manifest, f.store.clone())
            .unwrap()
            .with_memo_chunks(2);
        let before = f.counters.gets_attempted();
        for index in [0, 2, 0, 3, 0] {
            reader.read(index * CHUNK, 512).await.unwrap();
        }
        assert_eq!(
            f.counters.gets_attempted(),
            before + 3,
            "0, 2, 3 once each while 0 stays recent"
        );
        reader.read(2 * CHUNK, 512).await.unwrap();
        assert_eq!(
            f.counters.gets_attempted(),
            before + 4,
            "2 was evicted by 3"
        );
    }

    /// Counts chunk GETs; each waits `delay`, then for a gate permit.
    struct SlowBlob {
        inner: Arc<dyn BlobStorage>,
        gets: AtomicU64,
        delay: Duration,
        gate: tokio::sync::Semaphore,
    }

    impl SlowBlob {
        fn new(inner: Arc<dyn BlobStorage>, delay: Duration, open: bool) -> Arc<Self> {
            let permits = if open {
                tokio::sync::Semaphore::MAX_PERMITS
            } else {
                0
            };
            Arc::new(Self {
                inner,
                gets: AtomicU64::new(0),
                delay,
                gate: tokio::sync::Semaphore::new(permits),
            })
        }
    }

    #[async_trait]
    impl BlobStorage for SlowBlob {
        async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError> {
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            let _permit = self.gate.acquire().await.expect("the gate never closes");
            self.inner.get_streaming(key).await
        }
        async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_of_one_chunk_share_one_fetch() {
        let f = fixture().await;
        let slow = SlowBlob::new(f.store.blob_storage(), Duration::ZERO, false);
        let reader = Arc::new(DiskReader::new(&f.manifest, ChunkStore::new(slow.clone())).unwrap());
        let reads: Vec<_> = (0..16u64)
            .map(|i| {
                let reader = Arc::clone(&reader);
                tokio::spawn(async move { reader.read(2 * CHUNK + i * 4096, 4096).await })
            })
            .collect();
        while slow.gets.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Every read has had time to reach the store if it were going to.
        tokio::time::sleep(Duration::from_millis(100)).await;
        slow.gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        for (i, read) in reads.into_iter().enumerate() {
            let at = (2 * CHUNK) as usize + i * 4096;
            assert_eq!(read.await.unwrap().unwrap(), &f.expected[at..at + 4096]);
        }
        assert_eq!(slow.gets.load(Ordering::SeqCst), 1);
    }

    /// Reads of different chunks fetch at the same time, even when one
    /// task drives them all: the set takes less time than the fetches one
    /// after another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reads_of_different_chunks_fetch_in_parallel() {
        const DELAY: Duration = Duration::from_millis(300);
        let f = fixture().await;
        let slow = SlowBlob::new(f.store.blob_storage(), DELAY, true);
        let reader = DiskReader::new(&f.manifest, ChunkStore::new(slow.clone())).unwrap();
        let offsets: Vec<u64> = f.manifest.chunks.iter().map(|c| c.offset).collect();
        assert!(offsets.len() >= 4);

        let began = std::time::Instant::now();
        let reads = offsets.iter().map(|&offset| reader.read(offset, 4096));
        let results = futures::future::try_join_all(reads).await.unwrap();
        let took = began.elapsed();

        for (offset, got) in offsets.iter().zip(results) {
            assert_eq!(got, &f.expected[*offset as usize..*offset as usize + 4096]);
        }
        assert_eq!(slow.gets.load(Ordering::SeqCst), offsets.len() as u64);
        let serial = DELAY * offsets.len() as u32;
        assert!(
            took < serial - DELAY,
            "{} fetches of {DELAY:?} each took {took:?}",
            offsets.len()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn more_chunks_in_flight_than_the_memo_keeps_each_fetch_once() {
        let f = fixture().await;
        let slow = SlowBlob::new(f.store.blob_storage(), Duration::ZERO, false);
        let reader = Arc::new(
            DiskReader::new(&f.manifest, ChunkStore::new(slow.clone()))
                .unwrap()
                .with_memo_chunks(2),
        );
        let offsets: Vec<u64> = f.manifest.chunks.iter().map(|c| c.offset).collect();
        assert!(offsets.len() > 2);
        let read_all = || -> Vec<_> {
            offsets
                .iter()
                .map(|&offset| {
                    let reader = Arc::clone(&reader);
                    tokio::spawn(async move { (offset, reader.read(offset, 4096).await) })
                })
                .collect()
        };
        let first = read_all();
        while slow.gets.load(Ordering::SeqCst) < offsets.len() as u64 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Each chunk's fetch is in flight; a second read of each must
        // wait on it, not start another.
        let second = read_all();
        tokio::time::sleep(Duration::from_millis(100)).await;
        slow.gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
        for read in first.into_iter().chain(second) {
            let (offset, got) = read.await.unwrap();
            let at = offset as usize;
            assert_eq!(got.unwrap(), &f.expected[at..at + 4096]);
        }
        assert_eq!(slow.gets.load(Ordering::SeqCst), offsets.len() as u64);
    }

    #[tokio::test]
    async fn a_base_chunk_with_the_wrong_bytes_is_read_from_the_store() {
        let f = fixture().await;
        let base_dir = tempfile::tempdir().unwrap();
        let base_path = base_dir.path().join("base");
        let total = total_bytes();
        // The base manifest says chunk 0's hash is at offset 0, but the
        // volume holds other bytes there.
        std::fs::write(&base_path, vec![0xa5u8; total as usize]).unwrap();
        let mut base_manifest = Manifest::empty(ManifestKind::Disk, total);
        base_manifest.chunk_size = ChunkSize::bytes(CHUNK);
        base_manifest.chunks = vec![ChunkRef {
            offset: 0,
            hash: f.manifest.chunk_at(0).unwrap().hash,
        }];
        let reader = DiskReader::new(&f.manifest, f.store.clone())
            .unwrap()
            .with_base(LocalBase::open(&base_path, &base_manifest).unwrap());

        let before = f.counters.gets_attempted();
        assert_eq!(
            reader.read(0, CHUNK as usize).await.unwrap(),
            &f.expected[..CHUNK as usize]
        );
        assert_eq!(f.counters.gets_attempted(), before + 1);
    }
}
