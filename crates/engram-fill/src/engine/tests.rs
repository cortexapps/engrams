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
    // No eviction: the free-space floor of the test machine's disk must
    // not remove the chunks a test checks for.
    let cache = ChunkCache::new(ChunkCacheConfig {
        eviction_enabled: false,
        ..ChunkCacheConfig::new(cache_dir.path())
    });
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
    ranges: AtomicUsize,
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

    async fn fetch_chunk_range_unverified(
        &self,
        hash: ChunkHash,
        offset: u64,
        len: u64,
    ) -> Result<Bytes> {
        self.ranges.fetch_add(1, Ordering::SeqCst);
        self.inner
            .fetch_chunk_range_unverified(hash, offset, len)
            .await
    }

    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        self.inner.chunk_exists(hash).await
    }
}

fn counting(f: &Fixture, slow_first: Option<Duration>) -> Arc<Counting> {
    Arc::new(Counting {
        inner: BlobStorageResolver::new(f.blob.clone()),
        calls: AtomicUsize::new(0),
        ranges: AtomicUsize::new(0),
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
        .put_trusted(c, Bytes::from_static(b"chunk c"), Class::Background)
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
        .put_trusted(hash, Bytes::from_static(b"resident"), Class::Background)
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
async fn a_hedge_waits_for_a_slot_of_its_own() {
    let f = fixture();
    let hash = put(&f, b"one slot").await;
    let resolver = counting(&f, Some(Duration::from_secs(30)));
    let cfg = Config {
        in_flight: 1,
        max_in_flight: 1,
        demand_reserve: 0,
        hedge_after: Some(Duration::from_millis(200)),
        ..Config::default()
    };
    let engine = engine_over(&f, resolver.clone(), cfg);

    let fault = tokio::spawn({
        let engine = engine.clone();
        async move { engine.get(hash, Class::Demand).await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "the hedge waits while the first request holds the only slot"
    );
    assert_eq!(engine.admission().in_flight(), 1);
    fault.abort();
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

#[tokio::test]
async fn read_serves_a_resident_chunk_without_the_store() {
    let f = fixture();
    let hash = put(&f, b"staged once").await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());
    engine.get(hash, Class::Demand).await.unwrap();

    assert_eq!(
        &engine.read(hash, Class::Demand).await.unwrap()[..],
        b"staged once"
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
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
        .with_peer(peer(Some(b"only on the peer"), Duration::from_secs(1)));

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

#[test]
fn config_refuses_a_zero_or_inverted_limit() {
    let vars = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    };
    let cfg = Config::from_vars(vars(&[("ENGRAM_FILL_IN_FLIGHT", "32")])).unwrap();
    assert_eq!((cfg.in_flight, cfg.max_in_flight), (32, 32));
    assert_eq!(cfg.sandbox_demand_cap, None, "no cap unless one is set");
    let cfg = Config::from_vars(vars(&[("ENGRAM_FILL_SANDBOX_DEMAND_CAP", "4")])).unwrap();
    assert_eq!(cfg.sandbox_demand_cap, Some(4));
    for bad in [
        &[("ENGRAM_FILL_IN_FLIGHT", "0")][..],
        &[
            ("ENGRAM_FILL_IN_FLIGHT", "64"),
            ("ENGRAM_FILL_IN_FLIGHT_MAX", "32"),
        ][..],
        &[("ENGRAM_FILL_IN_FLIGHT_MAX", "8")][..],
        &[("ENGRAM_FILL_HEDGE_MS", "soon")][..],
        &[("ENGRAM_FILL_SANDBOX_DEMAND_CAP", "0")][..],
    ] {
        assert!(Config::from_vars(vars(bad)).is_err(), "{bad:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_materialize_through_the_engine_reads_the_peer() {
    let f = fixture();
    let body = b"materialized from the peer";
    let hash = put(&f, body).await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default())
        .with_peer(peer(Some(body), Duration::from_millis(1)));
    let mut manifest = engram_chunk_store::Manifest::empty(
        engram_chunk_store::ManifestKind::Disk,
        body.len() as u64,
    );
    manifest
        .chunks
        .push(engram_chunk_store::ChunkRef { offset: 0, hash });
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("disk.img");

    engine
        .store()
        .materialize_to_file_from(&manifest, &dest, &engine)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
}

/// 300 KiB of non-repeating bytes: wider than five range alignments.
fn wide_body() -> Vec<u8> {
    (0..300 * 1024u32).map(|i| (i % 251) as u8).collect()
}

/// Wait (in real time) until the cache holds `hash`.
async fn wait_resident(f: &Fixture, hash: ChunkHash) {
    for _ in 0..500 {
        if f.cache.contains_on_disk(hash) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the whole-chunk fill never landed");
}

#[tokio::test]
async fn a_range_of_a_resident_chunk_takes_no_store_request() {
    let f = fixture();
    let body = wide_body();
    let hash = put(&f, &body).await;
    f.cache
        .put_trusted(hash, Bytes::from(body.clone()), Class::Background)
        .await
        .unwrap();
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());

    let got = engine
        .read_range(hash, 70_000, 4096, Class::Demand)
        .await
        .unwrap();

    assert_eq!(&got[..], &body[70_000..74_096]);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.ranges.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_range_miss_is_served_by_a_ranged_get_and_the_whole_chunk_fills() {
    let f = fixture();
    let body = wide_body();
    let hash = put(&f, &body).await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());

    let got = engine
        .read_range(hash, 70_000, 4096, Class::Demand)
        .await
        .unwrap();

    assert_eq!(&got[..], &body[70_000..74_096]);
    assert_eq!(resolver.ranges.load(Ordering::SeqCst), 1);
    wait_resident(&f, hash).await;
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        1,
        "one whole-chunk fill"
    );
}

#[tokio::test]
async fn ranged_bytes_of_a_corrupt_object_never_reach_the_cache() {
    let f = fixture();
    let hash = ChunkHash::of(&wide_body());
    let mut other = wide_body();
    other[0] ^= 0xff;
    f.blob
        .put(&hash.storage_key(), Bytes::from(other.clone()))
        .await
        .unwrap();
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());

    let got = engine
        .read_range(hash, 0, 512, Class::Demand)
        .await
        .unwrap();

    assert_eq!(&got[..], &other[..512], "the range is not hash-checked");
    while resolver.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !f.cache.contains_on_disk(hash),
        "the fill's hash check refused it"
    );
}

#[tokio::test(start_paused = true)]
async fn with_a_peer_a_range_is_cut_from_the_peers_whole_chunk() {
    let f = fixture();
    let body: &'static [u8] = b"whole chunk on the peer";
    let hash = put(&f, body).await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default())
        .with_peer(peer(Some(body), Duration::from_millis(1)));

    let got = engine.read_range(hash, 6, 5, Class::Demand).await.unwrap();

    assert_eq!(&got[..], b"chunk");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.ranges.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_range_wider_than_the_limit_reads_the_whole_chunk() {
    let f = fixture();
    let body: Vec<u8> = (0..(MAX_RANGE as u32 + 1))
        .map(|i| (i % 251) as u8)
        .collect();
    let hash = put(&f, &body).await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());

    let got = engine
        .read_range(hash, 1, MAX_RANGE as usize, Class::Demand)
        .await
        .unwrap();

    assert_eq!(&got[..], &body[1..]);
    assert_eq!(resolver.ranges.load(Ordering::SeqCst), 0);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert!(f.cache.contains_on_disk(hash));
}

#[tokio::test]
async fn a_range_past_the_chunk_end_is_an_error() {
    let f = fixture();
    let hash = put(&f, b"short").await;
    let engine = engine_over(&f, counting(&f, None), Config::default());

    let err = engine
        .read_range(hash, 3, 10, Class::Demand)
        .await
        .unwrap_err();

    assert!(
        matches!(err, ChunkStoreError::RangeOutOfBounds { size: 5, .. }),
        "{err:?}"
    );
}

/// Store fetches of the `slow` chunks hang for 30 s.
struct SlowFor {
    inner: BlobStorageResolver,
    slow: HashSet<ChunkHash>,
}

#[async_trait]
impl ChunkResolver for SlowFor {
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        self.inner.fetch_chunk(hash).await
    }

    async fn fetch_chunk_unverified(&self, hash: ChunkHash) -> Result<Bytes> {
        if self.slow.contains(&hash) {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        self.inner.fetch_chunk_unverified(hash).await
    }

    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        self.inner.chunk_exists(hash).await
    }
}

#[tokio::test(start_paused = true)]
async fn a_sandbox_that_floods_demand_leaves_slots_for_another() {
    let f = fixture();
    let mut flood = Vec::new();
    for i in 0..4u8 {
        flood.push(put(&f, &[b'f', i]).await);
    }
    let other = put(&f, b"the other sandbox").await;
    let resolver = Arc::new(SlowFor {
        inner: BlobStorageResolver::new(f.blob.clone()),
        slow: flood.iter().copied().collect(),
    });
    let cfg = Config {
        in_flight: 1,
        max_in_flight: 1,
        demand_reserve: 1,
        hedge_after: None,
        sandbox_demand_cap: Some(1),
        ..Config::default()
    };
    let engine = engine_over(&f, resolver, cfg);
    let (a, b) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
    let _a = engine.register_sandbox(a);
    let _b = engine.register_sandbox(b);
    let mut faults = Vec::new();
    for hash in flood {
        let engine = engine.for_manifest(a);
        faults.push(tokio::spawn(async move {
            engine.get(hash, Class::Demand).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        engine.admission().in_flight(),
        1,
        "the flooding sandbox holds only its cap"
    );

    let started = tokio::time::Instant::now();
    engine
        .for_manifest(b)
        .get(other, Class::Demand)
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
    for fault in faults {
        fault.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn without_a_sandbox_cap_a_flood_takes_every_demand_slot() {
    let f = fixture();
    let mut flood = Vec::new();
    for i in 0..4u8 {
        flood.push(put(&f, &[b'f', i]).await);
    }
    let resolver = Arc::new(SlowFor {
        inner: BlobStorageResolver::new(f.blob.clone()),
        slow: flood.iter().copied().collect(),
    });
    let cfg = Config {
        in_flight: 1,
        max_in_flight: 1,
        demand_reserve: 1,
        hedge_after: None,
        ..Config::default()
    };
    let engine = engine_over(&f, resolver, cfg);
    let a = uuid::Uuid::from_u128(1);
    let _a = engine.register_sandbox(a);
    let mut faults = Vec::new();
    for hash in flood {
        let engine = engine.for_manifest(a);
        faults.push(tokio::spawn(async move {
            engine.get(hash, Class::Demand).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert_eq!(engine.admission().in_flight(), 2);
    for fault in faults {
        fault.abort();
    }
}

#[tokio::test]
async fn a_chunk_filled_once_is_not_fetched_again_at_a_lower_class() {
    let f = fixture();
    let a = put(&f, b"prefix chunk").await;
    let b = put(&f, b"hydrate chunk").await;
    let resolver = counting(&f, None);
    let engine = engine_over(&f, resolver.clone(), Config::default());

    engine.fill(vec![a], Class::Prefix).await.unwrap();
    // Let the index build and sweep that the first write starts run.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let stats = engine.fill(vec![b, a, b], Class::Hydrate).await.unwrap();

    assert_eq!(
        stats,
        FillStats {
            resident: 1,
            fetched: 1,
            duplicate: 1
        }
    );
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        2,
        "one GET per chunk"
    );
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_stops_answering_falls_back_to_the_store() {
    let f = fixture();
    let hash = put(&f, b"the peer never answers").await;
    let resolver = counting(&f, None);
    let cfg = Config {
        peer_timeout: Duration::from_secs(2),
        ..Config::default()
    };
    let engine = engine_over(&f, resolver.clone(), cfg).with_peer(peer(
        Some(b"the peer never answers"),
        Duration::from_secs(3600),
    ));

    let started = tokio::time::Instant::now();
    let bytes = engine.get(hash, Class::Hydrate).await.unwrap();

    assert_eq!(&bytes[..], b"the peer never answers");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(2) && waited < Duration::from_secs(3),
        "{waited:?}"
    );
}
