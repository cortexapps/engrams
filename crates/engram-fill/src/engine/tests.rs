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
    f.cache
        .put(c, Bytes::from_static(b"chunk c"))
        .await
        .unwrap();
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
    f.cache
        .put(hash, Bytes::from_static(b"resident"))
        .await
        .unwrap();
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

#[tokio::test(start_paused = true)]
async fn a_fault_on_a_chunk_a_bulk_fetch_holds_gets_its_own_fetch() {
    let f = fixture();
    let hash = put(&f, b"prefetched and faulted").await;
    let resolver = counting(&f, Some(Duration::from_secs(30)));
    let engine = engine_over(&f, resolver.clone(), Config::default());
    let bulk = tokio::spawn({
        let engine = engine.clone();
        async move { engine.get(hash, Class::Hydrate).await }
    });
    tokio::time::sleep(Duration::from_millis(1)).await;

    let started = tokio::time::Instant::now();
    let bytes = engine.get(hash, Class::Demand).await.unwrap();

    assert_eq!(&bytes[..], b"prefetched and faulted");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    let bulk = tokio::time::timeout(Duration::from_millis(10), bulk)
        .await
        .expect("the bulk caller gets the fault's answer");
    assert_eq!(&bulk.unwrap().unwrap()[..], b"prefetched and faulted");
}

#[tokio::test(start_paused = true)]
async fn a_caller_that_waits_for_another_fetch_holds_no_slot() {
    let f = fixture();
    let slow = put(&f, b"slow chunk").await;
    let other = put(&f, b"other chunk").await;
    let resolver = counting(&f, Some(Duration::from_secs(30)));
    let cfg = Config {
        in_flight: 2,
        max_in_flight: 2,
        demand_reserve: 0,
        ..Config::default()
    };
    let engine = engine_over(&f, resolver.clone(), cfg);
    let mut callers = Vec::new();
    for _ in 0..2 {
        let engine = engine.clone();
        callers.push(tokio::spawn(async move {
            engine.get(slow, Class::Hydrate).await
        }));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let started = tokio::time::Instant::now();
    engine.get(other, Class::Hydrate).await.unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the second slot is free while its caller waits"
    );
    for caller in callers {
        caller.abort();
    }
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

/// A peer that answers after `delay` with `answer`.
struct FakePeer {
    answer: Option<Bytes>,
    delay: Duration,
    asked: AtomicUsize,
}

#[async_trait]
impl crate::PeerSource for FakePeer {
    async fn fetch(&self, _hash: ChunkHash) -> Option<Bytes> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.answer.clone()
    }
}

fn peer(answer: Option<&'static [u8]>, delay: Duration) -> Arc<FakePeer> {
    Arc::new(FakePeer {
        answer: answer.map(Bytes::from_static),
        delay,
        asked: AtomicUsize::new(0),
    })
}

#[tokio::test(start_paused = true)]
async fn a_peer_hit_is_served_without_the_store() {
    let f = fixture();
    let hash = put(&f, b"on the peer").await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default())
        .with_peer(peer(Some(b"on the peer"), Duration::from_millis(1)));

    for class in [Class::Demand, Class::Hydrate] {
        f.cache.evict_on_disk_for_test(hash);
        assert_eq!(&engine.get(hash, class).await.unwrap()[..], b"on the peer");
    }
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn a_peer_miss_falls_back_to_the_store() {
    let f = fixture();
    let hash = put(&f, b"the real bytes").await;
    {
        let answer = None;
        f.cache.evict_on_disk_for_test(hash);
        let resolver = counting(&f, None);
        let engine = engine_over(&f, resolver.clone(), Config::default())
            .with_peer(peer(answer, Duration::from_millis(1)));
        assert_eq!(
            &engine.get(hash, Class::Hydrate).await.unwrap()[..],
            b"the real bytes"
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert!(f.cache.contains_on_disk(hash));
    }
}

#[tokio::test(start_paused = true)]
async fn a_slow_peer_loses_a_demand_race_to_the_store() {
    let f = fixture();
    let hash = put(&f, b"faulted").await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default())
        .with_peer(peer(Some(b"faulted"), Duration::from_secs(30)));

    let started = tokio::time::Instant::now();
    assert_eq!(
        &engine.get(hash, Class::Demand).await.unwrap()[..],
        b"faulted"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_store_waits_for_a_slow_peer_in_a_demand_race() {
    let f = fixture();
    let hash = ChunkHash::of(b"only on the peer");
    let engine = engine_over(&f, counting(&f, None), Config::default())
        .with_peer(peer(Some(b"only on the peer"), Duration::from_secs(2)));

    assert_eq!(
        &engine.get(hash, Class::Demand).await.unwrap()[..],
        b"only on the peer"
    );
}

#[tokio::test(start_paused = true)]
async fn a_registered_peer_serves_its_manifest_until_the_registration_drops() {
    let f = fixture();
    let hash = put(&f, b"registered").await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());
    let manifest = uuid::Uuid::from_u128(7);
    let registration = engine.register_peer(
        manifest,
        peer(Some(b"registered"), Duration::from_millis(1)),
    );

    engine
        .for_manifest(manifest)
        .get(hash, Class::Demand)
        .await
        .unwrap();
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);

    drop(registration);
    f.cache.evict_on_disk_for_test(hash);
    engine
        .for_manifest(manifest)
        .get(hash, Class::Demand)
        .await
        .unwrap();
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn dropping_a_replaced_registration_keeps_the_newer_peer() {
    let f = fixture();
    let hash = put(&f, b"re-registered").await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());
    let manifest = uuid::Uuid::from_u128(9);
    let older = engine.register_peer(
        manifest,
        peer(Some(b"re-registered"), Duration::from_millis(1)),
    );
    let _newer = engine.register_peer(
        manifest,
        peer(Some(b"re-registered"), Duration::from_millis(1)),
    );

    drop(older);
    engine
        .for_manifest(manifest)
        .get(hash, Class::Demand)
        .await
        .unwrap();

    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        0,
        "the newer peer served it"
    );
}

#[tokio::test]
async fn an_upload_through_the_engine_store_takes_an_engine_slot() {
    let f = fixture();
    let cfg = Config {
        in_flight: 1,
        max_in_flight: 1,
        demand_reserve: 0,
        ..Config::default()
    };
    let engine = engine_over(&f, counting(&f, None), cfg);
    let held = engine.admission().acquire(Class::Hydrate).await;

    let store = engine.store().clone();
    let upload = tokio::spawn(async move { store.put_chunk(b"an upload").await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!upload.is_finished(), "the upload waits for the held slot");

    drop(held);
    upload.await.unwrap().unwrap();
    assert_eq!(engine.admission().in_flight(), 0);
}
