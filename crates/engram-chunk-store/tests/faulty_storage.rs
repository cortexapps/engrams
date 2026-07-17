//! Chunk-store behavior under injected storage faults (ADR 0099, H5).
//!
//! These tests drive `ChunkStore` over an `engram_testkit::storage::
//! FaultyBlobStorage` to assert three durability/correctness properties
//! that only manifest when the object store misbehaves:
//!
//! 1. **Flush atomicity** — a put failure on chunk *k of n* aborts the
//!    flush before any manifest version is published, so no manifest ever
//!    references an un-uploaded chunk; a retry after the fault clears
//!    converges.
//! 2. **Truncated get** — a silently short-read chunk is caught by the
//!    resolver's hash verification (never served as short bytes), and a
//!    tiered resolver falls through to the next tier.
//! 3. **Missing chunk on cold read** — a `NotFound` on a manifest-
//!    referenced chunk surfaces a typed, retryable error promptly (no hang,
//!    no panic).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::error::ChunkStoreError;
use engram_chunk_store::manifest::{ChunkHash, ChunkRef, Manifest, ManifestKind, ManifestRef};
use engram_chunk_store::resolver::{BlobStorageResolver, ChunkResolver, TieredChunkResolver};
use engram_chunk_store::store::ChunkStore;
use engram_core::error::BlobError;
use engram_core::traits::BlobStorage;
use engram_storage_local::LocalBlobStorage;
use engram_testkit::storage::{
    FaultPlan, FaultyBlobStorage, GetFault, GetFaultKind, InjectedError, KeyMatch, PutFault,
    PutFaultKind, When,
};

const CHUNK_SIZE: u64 = engram_chunk_store::DEFAULT_DISK_CHUNK_SIZE;

/// Upload `chunks` (one PUT each) then publish a manifest referencing them.
/// Mirrors the real disk-flush / capture ordering: **all chunks durable
/// first, manifest last** — the manifest is the commit point, so an error
/// before it must leave nothing published.
async fn flush(
    store: &ChunkStore,
    r: ManifestRef,
    chunks: &[&[u8]],
) -> engram_chunk_store::Result<Manifest> {
    let total = CHUNK_SIZE * chunks.len() as u64;
    let mut m = Manifest::empty(ManifestKind::Disk, total);
    for (i, body) in chunks.iter().enumerate() {
        let hash = store.put_chunk_unchecked(body).await?;
        m.chunks.push(ChunkRef {
            offset: CHUNK_SIZE * i as u64,
            hash,
        });
    }
    store.put_manifest(r, &m).await?;
    Ok(m)
}

/// Property 1: a put failure on chunk k aborts the flush before publishing,
/// so no manifest version ever references an un-uploaded chunk. Clearing the
/// fault and retrying converges.
#[tokio::test]
async fn flush_put_failure_never_publishes_partial_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let backing: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));

    let chunks: &[&[u8]] = &[b"chunk-0", b"chunk-1", b"chunk-2", b"chunk-3"];
    let manifest_id = uuid::Uuid::new_v4();
    let r = ManifestRef {
        manifest_id,
        version: 1,
    };

    // Fault: fail the 3rd chunk upload (k = 3 of 4). Manifest PUTs live
    // under `manifests/` and are untouched.
    let plan = FaultPlan::new().with_put(PutFault {
        key: KeyMatch::Prefix("chunks/".into()),
        when: When::Nth(3),
        kind: PutFaultKind::FailBeforeWrite(InjectedError::Enospc),
    });
    let (faulty, counters) = FaultyBlobStorage::arc(backing.clone(), plan);
    let faulted_store = ChunkStore::new(faulty);

    let err = flush(&faulted_store, r, chunks)
        .await
        .expect_err("flush must fail when chunk 3 cannot be uploaded");
    assert!(
        matches!(err, ChunkStoreError::Blob(BlobError::Io(_))),
        "expected the injected ENOSPC to propagate, got {err:?}",
    );
    assert_eq!(counters.puts_faulted(), 1, "exactly one chunk PUT faulted");

    // THE property: no manifest version was published at all — so none can
    // reference the un-uploaded chunk 3.
    assert_eq!(
        faulted_store
            .latest_manifest_version(manifest_id)
            .await
            .unwrap(),
        None,
        "a failed flush must not publish any manifest version",
    );

    // Retry with the fault cleared, over the SAME backing store (the chunks
    // uploaded before the fault persist). It must converge.
    let clean_store = ChunkStore::new(backing.clone());
    let published = flush(&clean_store, r, chunks)
        .await
        .expect("retry after the fault clears must converge");

    // The manifest is now published and every referenced chunk resolves.
    let (got_ref, got) = clean_store
        .get_latest_manifest(manifest_id)
        .await
        .unwrap()
        .expect("manifest present after successful retry");
    assert_eq!(got_ref.version, 1);
    assert_eq!(got, published);
    for cr in &got.chunks {
        let bytes = clean_store.get_chunk(cr.hash).await.unwrap();
        assert_eq!(ChunkHash::of(&bytes), cr.hash, "referenced chunk resolves");
    }
}

/// Property 2a: a silently truncated get (short read, no error) is caught by
/// the resolver's hash verification — it errors `HashMismatch` rather than
/// serving short bytes.
#[tokio::test]
async fn truncated_get_is_caught_by_resolver_hash_check() {
    let dir = tempfile::tempdir().unwrap();
    let backing: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));

    let body = Bytes::from_static(b"a whole chunk body that must arrive intact");
    let hash = ChunkHash::of(&body);
    backing
        .put(&hash.storage_key(), body.clone())
        .await
        .unwrap();

    // Truncate reads of this chunk after 8 bytes, with a clean EOF — the
    // sharpest case: storage reports success, the body is just short.
    let plan = FaultPlan::new().with_get(GetFault {
        key: KeyMatch::Exact(hash.storage_key()),
        when: When::Always,
        kind: GetFaultKind::TruncateCleanEof { after_bytes: 8 },
    });
    let (faulty, counters) = FaultyBlobStorage::arc(backing.clone(), plan);

    let resolver = BlobStorageResolver::new(faulty);
    match resolver.fetch_chunk(hash).await {
        Err(ChunkStoreError::HashMismatch { .. }) => {}
        other => panic!("expected HashMismatch on a short read, got {other:?}"),
    }
    assert_eq!(counters.gets_truncated(), 1);
}

/// Property 2b: a `TieredChunkResolver` whose primary tier returns a
/// truncated (hash-mismatched) chunk falls through to the next tier and
/// serves the correct bytes.
#[tokio::test]
async fn tiered_resolver_falls_through_past_truncated_tier() {
    let dir = tempfile::tempdir().unwrap();
    let backing: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));

    let body = Bytes::from_static(b"correct bytes served by the fallback tier");
    let hash = ChunkHash::of(&body);
    backing
        .put(&hash.storage_key(), body.clone())
        .await
        .unwrap();

    // Tier 0 reads through a truncating wrapper over the SAME backing; tier 1
    // reads it cleanly.
    let plan = FaultPlan::new().with_get(GetFault {
        key: KeyMatch::Exact(hash.storage_key()),
        when: When::Always,
        kind: GetFaultKind::TruncateCleanEof { after_bytes: 4 },
    });
    let (faulty, _c) = FaultyBlobStorage::arc(backing.clone(), plan);
    let tier0: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(faulty));
    let tier1: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(backing.clone()));

    let tiered = TieredChunkResolver::new(vec![tier0, tier1], None);
    let got = tiered
        .fetch_chunk(hash)
        .await
        .expect("tiered resolver must fall through the truncated tier");
    assert_eq!(got, body, "fallback tier served the correct, full chunk");
}

/// Property 3: a `NotFound` on a manifest-referenced chunk during a cold
/// read surfaces a typed, retryable error promptly — no hang, no panic.
#[tokio::test]
async fn missing_chunk_on_cold_read_errors_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let backing: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));

    // A published manifest referencing one chunk, both durable.
    let clean = ChunkStore::new(backing.clone());
    let body = b"cold-resume chunk";
    let hash = clean.put_chunk(body).await.unwrap();
    let manifest_id = uuid::Uuid::new_v4();
    let r = ManifestRef {
        manifest_id,
        version: 1,
    };
    let mut m = Manifest::empty(ManifestKind::Disk, CHUNK_SIZE);
    m.chunks.push(ChunkRef { offset: 0, hash });
    clean.put_manifest(r, &m).await.unwrap();

    // The chunk vanishes (GC race / partial replica) at cold-read time.
    let plan = FaultPlan::new().with_get(GetFault {
        key: KeyMatch::Exact(hash.storage_key()),
        when: When::Always,
        kind: GetFaultKind::NotFound(InjectedError::NotFound),
    });
    let (faulty, counters) = FaultyBlobStorage::arc(backing.clone(), plan);
    let faulted = ChunkStore::new(faulty);

    // Cold read: manifest (untouched) resolves, then the missing chunk.
    let (_ref, cold_m) = faulted
        .get_latest_manifest(manifest_id)
        .await
        .unwrap()
        .expect("manifest itself still readable");
    let missing = cold_m.chunks[0].hash;

    // Bounded wait proves promptness (no hang).
    let result = tokio::time::timeout(Duration::from_secs(5), faulted.get_chunk(missing))
        .await
        .expect("get_chunk must return promptly, never hang");

    match result {
        Err(ChunkStoreError::Blob(BlobError::NotFound)) => {}
        other => panic!("expected a typed Blob(NotFound), got {other:?}"),
    }
    assert_eq!(counters.gets_faulted(), 1);
}
