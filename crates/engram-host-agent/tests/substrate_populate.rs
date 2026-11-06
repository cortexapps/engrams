//! ADR 0075 integration: the real `PopulateClient` (the uffd handler's
//! miss path) against the real `SubstrateServer` (the host-agent's
//! writer) over a real UDS: framing, Hello probes, slot delivery, and
//! pinned chunks that survive cache pressure. The property under test
//! is the socket contract, so no VM is needed.

use std::sync::Arc;

use engram_chunk_store::cache::{ChunkCacheConfig, FillSource};
use engram_chunk_store::{ChunkCache, ChunkStore, Class};
use engram_core::traits::BlobStorage;
use engram_host_agent::substrate_server::SubstrateServer;
use engram_storage_local::LocalBlobStorage;
use engram_uffd_handler::populate_client::PopulateClient;

fn fixture(
    dir: &std::path::Path,
    budget_bytes: u64,
) -> (SubstrateServer, ChunkCache, Arc<ChunkStore>) {
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.join("blobs")));
    // The cache root must exist — write_local does NOT create it, which
    // is exactly the prod ENOENT class (583/7d) this epic bounds to one
    // process. (The host-agent's startup creates it in production.)
    std::fs::create_dir_all(dir.join("chunk-cache")).unwrap();
    let mut cfg = ChunkCacheConfig::new(dir.join("chunk-cache"));
    if budget_bytes > 0 {
        cfg.budget_bytes = budget_bytes;
    }
    let cache = ChunkCache::new(cfg);
    let store = Arc::new(ChunkStore::new(blob).with_chunk_cache(cache.clone()));
    let fill = engram_fill::FillEngine::new((*store).clone(), engram_fill::Config::default());
    let server = SubstrateServer::new(cache.clone(), fill, dir.join("shm"));
    (server, cache, store)
}

/// The real client against the real server speaks version 2: chunks
/// arrive in the slot, the slot is reused once a reply's bytes drop,
/// and bytes still held keep their connection out of the pool, so a
/// second request on another connection cannot overwrite them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_client_gets_chunks_through_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let (server, cache, store) = fixture(dir.path(), 0);
    let first = bytes::Bytes::from(vec![0x42u8; 4096]);
    let second = bytes::Bytes::from(vec![0x17u8; 8192]);
    let first_hash = store.put_chunk(&first).await.unwrap();
    let second_hash = store.put_chunk(&second).await.unwrap();
    cache.evict_on_disk_for_test(first_hash);
    cache.evict_on_disk_for_test(second_hash);

    let sock = dir.path().join("substrate.sock");
    let _accept = server.spawn(sock.clone()).unwrap();

    let client = Arc::new(PopulateClient::new(sock));
    let (_tmpfs_ok, cache_writable) = tokio::task::spawn_blocking(move || {
        let probes = client.hello(None, None, 2).expect("hello");
        let held = client.request(first_hash).expect("populate first");
        let other = client.request(second_hash).expect("populate second");
        assert_eq!(held, first, "slot bytes must match the chunk");
        assert_eq!(other, second, "a held reply must not be overwritten");
        drop(held);
        drop(other);
        let again = client.request(first_hash).expect("populate again");
        assert_eq!(again, first);
        probes
    })
    .await
    .unwrap();
    // A tempdir is not always tmpfs; the probe answered, and a tempdir
    // must probe writable.
    assert!(cache_writable);
    assert!(
        cache.contains_on_disk(first_hash),
        "the writer, not the client, writes the cache",
    );
}

/// The regression the whole epic exists for: with the cache AT its
/// budget and the session's chunks PINNED (the Hello pin path), a
/// storm of populates for new chunks never evicts a pinned chunk.
/// Pre-0069 this failed by construction — the handler's sweep skipped
/// only its own (empty) pin map.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_chunks_survive_populate_pressure() {
    let dir = tempfile::tempdir().unwrap();
    // Budget: room for ~4 chunks of 64 KiB.
    let (server, cache, store) = fixture(dir.path(), 256 * 1024);
    let _ = &server;

    // The "session" chunk, pinned the way a Hello pins it: pin first,
    // then populate, as production orders it.
    let pinned_bytes = bytes::Bytes::from(vec![0xAAu8; 64 * 1024]);
    let pinned_hash = store.put_chunk(&pinned_bytes).await.unwrap();
    cache.pin(pinned_hash);
    // Deterministic populate via the same public path the substrate
    // server's Populate runs (put_chunk's write-through is
    // best-effort by design).
    let s2 = store.clone();
    cache
        .get_with_source(pinned_hash, Class::Demand, || async move {
            Ok((s2.get_chunk(pinned_hash).await?, FillSource::BlobStorage))
        })
        .await
        .unwrap();
    assert!(cache.contains_on_disk(pinned_hash));

    // Storm: populate 16 fresh chunks through the writer's own get
    // path (what Populate runs), driving the budget sweep repeatedly.
    for i in 0..16u8 {
        let fresh = bytes::Bytes::from(vec![i; 64 * 1024]);
        let h = store.put_chunk(&fresh).await.unwrap();
        let store2 = store.clone();
        let _ = cache
            .get_with_source(h, Class::Demand, || async move {
                Ok((store2.get_chunk(h).await?, FillSource::BlobStorage))
            })
            .await;
    }
    cache.sweep_for_test().await;

    assert!(
        cache.contains_on_disk(pinned_hash),
        "a pinned chunk must survive budget pressure from populate traffic",
    );
    cache.unpin(pinned_hash);
}
