//! ADR 0075 integration: the REAL `PopulateClient` (the uffd handler's
//! miss path) against the REAL `SubstrateServer` (the host-agent's
//! writer) over a real UDS — fd passing, framing, Hello probes, the
//! pin-around-open sequence, and the pinned-chunk-survives-pressure
//! property that was unrepresentable pre-0069 (the handler's own sweep
//! couldn't see the writer's pins).
//!
//! Linux-gated where fd passing is involved (SCM_RIGHTS); runs in the
//! standard Linux lanes and the FC lane — no VM needed, because the
//! property under test is the socket contract, not virtualization (the
//! FC spawn's contribution is one `--substrate-sock` argv string; the
//! end-to-end VM path is dev-vm-validated per the CI test-sizing rule).

use std::sync::Arc;

use engram_chunk_store::cache::ChunkCacheConfig;
use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::BlobStorage;
use engram_host_agent::substrate_server::SubstrateServer;
use engram_storage_local::LocalBlobStorage;
#[cfg(target_os = "linux")]
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
    let server = SubstrateServer::new(cache.clone(), store.clone(), dir.join("shm"));
    (server, cache, store)
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_client_populates_via_real_server_with_fd_passing() {
    let dir = tempfile::tempdir().unwrap();
    let (server, cache, store) = fixture(dir.path(), 0);
    let bytes = bytes::Bytes::from(vec![0x42u8; 4096]);
    let hash = store.put_chunk(&bytes).await.unwrap();
    cache.evict_on_disk_for_test(hash);

    let sock = dir.path().join("substrate.sock");
    let _accept = server.spawn(sock.clone()).unwrap();

    let client = PopulateClient::new(sock);
    let (tmpfs_ok, cache_writable) = tokio::task::spawn_blocking(move || {
        let got = client.hello(None, None).expect("hello");
        let bytes_back = client.request(hash).expect("populate");
        (got, bytes_back)
    })
    .await
    .map(|((t, w), b)| {
        assert_eq!(b, bytes.to_vec(), "fd-passed bytes must match the chunk");
        (t, w)
    })
    .unwrap();
    // /dev/... tempdir is not tmpfs necessarily — the honest assertion
    // is that the probe ANSWERED; writability must hold for a tempdir.
    let _ = tmpfs_ok;
    assert!(cache_writable);
    assert!(
        cache.contains_on_disk(hash),
        "populate must write-through to the cache dir (the writer wrote, not the client)",
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

    // The "session" chunk, pinned the way a Hello pins it: PIN FIRST,
    // then populate — the ADR 0075 pin-around-open ordering. (The
    // previous populate-then-pin ordering left a window where the
    // populate's own debounced background sweep snapshotted an empty
    // pin map and — under real host disk pressure via the statvfs
    // floor — evicted the chunk before/after the pin landed. That
    // interleaving is a REAL bug the sweep now guards with a
    // delete-time pin re-check; the test orders like production.)
    let pinned_bytes = bytes::Bytes::from(vec![0xAAu8; 64 * 1024]);
    let pinned_hash = store.put_chunk(&pinned_bytes).await.unwrap();
    cache.pin(pinned_hash);
    // Deterministic populate via the same public path the substrate
    // server's Populate runs (put_chunk's write-through is
    // best-effort by design).
    let s2 = store.clone();
    cache
        .get(
            pinned_hash,
            || async move { s2.get_chunk(pinned_hash).await },
        )
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
            .get(h, || async move { store2.get_chunk(h).await })
            .await;
    }
    cache.sweep_for_test().await;

    assert!(
        cache.contains_on_disk(pinned_hash),
        "a pinned chunk must survive budget pressure from populate traffic",
    );
    cache.unpin(pinned_hash);
}
