use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use engram_chunk_store::{
    BlobStorageResolver, ChunkCache, ChunkCacheConfig, ChunkResolver, ChunkStoreError,
};
use engram_core::traits::BlobStorage;
use engram_storage_local::LocalBlobStorage;

use super::*;

struct Fixture {
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    blob: Arc<dyn BlobStorage>,
    cache: ChunkCache,
}

fn fixture() -> Fixture {
    let blob_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(blob_dir.path()));
    let cache = ChunkCache::new(ChunkCacheConfig::new(cache_dir.path()));
    Fixture {
        _dirs: (blob_dir, cache_dir),
        blob,
        cache,
    }
}

fn engine_over(f: &Fixture, resolver: Arc<dyn ChunkResolver>, cfg: Config) -> FillEngine {
    let store = ChunkStore::new(f.blob.clone())
        .with_resolver(resolver)
        .with_chunk_cache(f.cache.clone());
    FillEngine::new(store, cfg)
}

async fn put(f: &Fixture, body: &[u8]) -> ChunkHash {
    ChunkStore::new(f.blob.clone())
        .put_chunk(body)
        .await
        .unwrap()
}

/// Counts every store fetch; `slow_first` makes the first one hang.
struct Counting {
    inner: BlobStorageResolver,
    calls: AtomicUsize,
    slow_first: Option<Duration>,
}

#[async_trait]
impl ChunkResolver for Counting {
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        self.inner.fetch_chunk(hash).await
    }

    async fn fetch_chunk_unverified(&self, hash: ChunkHash) -> Result<Bytes> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if let (0, Some(d)) = (n, self.slow_first) {
            tokio::time::sleep(d).await;
        }
        self.inner.fetch_chunk_unverified(hash).await
    }

    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        self.inner.chunk_exists(hash).await
    }
}

fn counting(f: &Fixture, slow_first: Option<Duration>) -> Arc<Counting> {
    Arc::new(Counting {
        inner: BlobStorageResolver::new(f.blob.clone()),
        calls: AtomicUsize::new(0),
        slow_first,
    })
}

#[tokio::test]
async fn fill_fetches_each_missing_chunk_once_and_skips_resident_ones() {
    let f = fixture();
    let a = put(&f, b"chunk a").await;
    let b = put(&f, b"chunk b").await;
    let c = put(&f, b"chunk c").await;
    f.cache.put(c, b"chunk c").await.unwrap();
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());

    let stats = engine.fill(vec![a, b, a, c], Class::Hydrate).await.unwrap();

    assert_eq!(
        stats,
        FillStats {
            resident: 1,
            fetched: 2,
            duplicate: 1
        }
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert!(f.cache.contains_on_disk(a) && f.cache.contains_on_disk(b));
}

#[tokio::test]
async fn a_corrupt_store_object_is_neither_served_nor_stored() {
    let f = fixture();
    let hash = ChunkHash::of(b"the real bytes");
    f.blob
        .put(&hash.storage_key(), Bytes::from_static(b"other bytes"))
        .await
        .unwrap();
    let engine = engine_over(&f, counting(&f, None), Config::default());

    let err = engine.get(hash, Class::Demand).await.unwrap_err();

    assert!(
        matches!(err, ChunkStoreError::HashMismatch { .. }),
        "{err:?}"
    );
    assert!(!f.cache.contains_on_disk(hash));
}

#[tokio::test]
async fn a_cache_hit_takes_no_slot() {
    let f = fixture();
    let hash = put(&f, b"resident").await;
    f.cache.put(hash, b"resident").await.unwrap();
    let engine = engine_over(&f, counting(&f, None), Config::default());
    let _all = engine.admission().acquire(Class::Demand).await;

    let bytes = engine.get(hash, Class::Background).await.unwrap();

    assert_eq!(&bytes[..], b"resident");
}

#[tokio::test(start_paused = true)]
async fn a_slow_demand_fetch_is_hedged_and_the_first_answer_wins() {
    let f = fixture();
    let hash = put(&f, b"faulted page").await;
    let resolver = counting(&f, Some(Duration::from_secs(30)));
    let cfg = Config {
        hedge_after: Some(Duration::from_millis(200)),
        ..Config::default()
    };
    let engine = engine_over(&f, resolver.clone(), cfg);

    let started = tokio::time::Instant::now();
    let bytes = engine.get(hash, Class::Demand).await.unwrap();

    assert_eq!(&bytes[..], b"faulted page");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn bulk_fetches_are_not_hedged() {
    let f = fixture();
    let hash = put(&f, b"bulk page").await;
    let resolver = counting(&f, Some(Duration::from_secs(2)));
    let engine = engine_over(&f, resolver.clone(), Config::default());

    engine.get(hash, Class::Hydrate).await.unwrap();

    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fill_returns_the_first_error_after_the_other_fetches_finish() {
    let f = fixture();
    let good = put(&f, b"good").await;
    let missing = ChunkHash::of(b"never uploaded");
    let engine = engine_over(&f, counting(&f, None), Config::default());

    let err = engine.fill(vec![missing, good], Class::Hydrate).await;

    assert!(err.is_err());
    assert!(f.cache.contains_on_disk(good));
}

#[tokio::test]
async fn read_verifies_and_leaves_the_cache_alone() {
    let f = fixture();
    let good = put(&f, b"volume bytes").await;
    let bad = ChunkHash::of(b"expected");
    f.blob
        .put(&bad.storage_key(), Bytes::from_static(b"tampered"))
        .await
        .unwrap();
    let engine = engine_over(&f, counting(&f, None), Config::default());

    assert_eq!(
        &engine.read(good, Class::Demand).await.unwrap()[..],
        b"volume bytes"
    );
    assert!(!f.cache.contains_on_disk(good));
    assert!(engine.read(bad, Class::Demand).await.is_err());
}
