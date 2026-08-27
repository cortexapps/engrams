//! Backend-agnostic [`BlobStorage`] conformance scenarios.
//!
//! Every blob backend (local fs, GCS, S3) must satisfy the same trait
//! contract: byte-exact round-trips, streaming without materializing
//! bodies, `NotFound` mapping, idempotent delete, transparent list
//! pagination, and no half-written destination after a failed put.
//! Before this module each backend re-implemented its own copy of
//! these tests and the copies drifted (the GCS suite checked
//! pagination, the local suite checked failed-put atomicity — neither
//! checked both). Backends now construct their store, apply their own
//! env gating (emulator-backed backends no-op without
//! `STORAGE_EMULATOR_HOST` / `ENGRAM_TEST_S3_ENDPOINT`), and call
//! these scenarios so a contract divergence is a test failure, not a
//! latent prod difference.
//!
//! Scenarios panic on violation (they run inside `#[tokio::test]`
//! bodies). Keys are namespaced per call via [`unique_prefix`] so
//! concurrent runs against a shared emulator bucket don't collide.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::StreamExt;

use engram_core::error::BlobError;
use engram_core::traits::{BlobStorage, ByteStream};

/// A key prefix unique to this call within the process and across
/// concurrent test invocations sharing an emulator: wall-clock nanos
/// plus a process-local counter.
pub fn unique_prefix(ns: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("engram/conformance/{ns}/{nanos}-{seq}")
}

/// The standard small test body: larger than one TCP frame so real
/// streaming is exercised, and containing 0x00 + 0xFF + every other
/// byte value to surface any text-mode mangling.
fn patterned_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 257) as u8).collect()
}

/// put → head → get → delete → idempotent re-delete → NotFound.
pub async fn round_trip(store: &dyn BlobStorage) {
    let key = format!("{}/round-trip.bin", unique_prefix("round-trip"));
    let body = patterned_body(4096);

    let put_size = store
        .put(&key, Bytes::from(body.clone()))
        .await
        .expect("put");
    assert_eq!(put_size, body.len() as u64);

    let head = store.head(&key).await.expect("head");
    assert_eq!(head.size_bytes, body.len() as u64);
    assert!(
        head.etag.is_some(),
        "every backend returns an (opaque) etag"
    );
    assert!(store.exists(&key).await.expect("exists"));

    let got = store.get(&key).await.expect("get");
    assert_eq!(&got[..], &body[..], "round-tripped bytes must match");

    store.delete(&key).await.expect("delete");
    assert!(!store.exists(&key).await.expect("exists after delete"));
    store
        .delete(&key)
        .await
        .expect("delete on missing key must be idempotent");

    assert!(matches!(store.head(&key).await, Err(BlobError::NotFound)));
    assert!(matches!(
        store.get_streaming(&key).await.map(|_| ()),
        Err(BlobError::NotFound)
    ));
}

/// Streams `chunks` × `chunk_len` bytes through `put_streaming`,
/// verifies the reported and `head` sizes, then pulls the object back
/// through `get_streaming` and byte-compares the first frame. (Full
/// equality on a large body would buffer it all in RAM, which defeats
/// the streaming point; size + first-frame catches truncation,
/// reordering, and mangling.)
///
/// Size the arguments to the lane: a wire backend wants enough volume
/// to cross its part/resumable-upload thresholds (e.g. 64 KiB × 1024);
/// the local backend only needs a handful of chunks to prove writer
/// advancement.
pub async fn streaming_round_trip(store: &dyn BlobStorage, chunk_len: usize, chunks: usize) {
    let key = format!("{}/streamed.bin", unique_prefix("streaming"));
    let chunk = patterned_body(chunk_len);
    let chunk_bytes = Bytes::from(chunk.clone());
    let total = (chunk_len * chunks) as u64;

    let body = ByteStream::new(futures::stream::iter(
        (0..chunks).map(move |_| Ok(chunk_bytes.clone())),
    ));
    let put_size = store
        .put_streaming(&key, body)
        .await
        .expect("put_streaming");
    assert_eq!(put_size, total);

    let head = store.head(&key).await.expect("head");
    assert_eq!(head.size_bytes, total);

    let mut got = store.get_streaming(&key).await.expect("get_streaming");
    let first = got
        .next()
        .await
        .expect("streamed object yields at least one frame")
        .expect("first frame ok");
    let n = first.len().min(chunk_len);
    assert!(n > 0, "first frame must not be empty");
    assert_eq!(&first[..n], &chunk[..n], "first frame bytes must match");
    drop(got);

    store.delete(&key).await.expect("delete");
}

/// A zero-length streaming put is a valid object, not an error: the
/// stream ends before the first byte, `head` reports size 0, and the
/// body reads back empty.
pub async fn zero_byte_put(store: &dyn BlobStorage) {
    let key = format!("{}/empty.bin", unique_prefix("empty"));

    let body = ByteStream::new(futures::stream::empty());
    let put_size = store.put_streaming(&key, body).await.expect("empty put");
    assert_eq!(put_size, 0);

    let head = store.head(&key).await.expect("head");
    assert_eq!(head.size_bytes, 0);
    let got = store.get(&key).await.expect("get");
    assert!(got.is_empty());

    store.delete(&key).await.expect("delete");
}

/// The trait contract for failed puts: a body stream that errors
/// mid-way surfaces the error and leaves the destination unchanged —
/// the previously stored object must still read back intact (local:
/// tempfile+rename; GCS: atomic object replace; S3: multipart abort).
pub async fn failed_streaming_put_preserves_prior_blob(store: &dyn BlobStorage) {
    let key = format!("{}/atomic.bin", unique_prefix("failed-put"));

    store
        .put(&key, Bytes::from_static(b"good"))
        .await
        .expect("initial put");

    let chunks: Vec<Result<Bytes, BlobError>> = vec![
        Ok(Bytes::from_static(b"new-partial")),
        Err(BlobError::Protocol("simulated mid-stream failure".into())),
    ];
    let res = store
        .put_streaming(&key, ByteStream::new(futures::stream::iter(chunks)))
        .await;
    assert!(res.is_err(), "partial body must surface as an error");

    let got = store.get(&key).await.expect("get after failed put");
    assert_eq!(
        &got[..],
        b"good",
        "prior blob must be preserved after a failed put"
    );

    store.delete(&key).await.expect("delete");
}

/// Writes `n` keys under one prefix plus a handful under a sibling
/// prefix, then asserts `list_prefix` returns exactly the prefixed
/// set — full keys, transparently paginated, no cross-prefix bleed.
/// Order is backend-specific per the trait doc, so the comparison is
/// set-based.
///
/// Size `n` to the lane: a wire backend wants `n` above its page size
/// (1000 for GCS and S3) so pagination is actually exercised; the
/// local backend only needs a few keys.
pub async fn list_prefix_scoped(store: &dyn BlobStorage, n: usize) {
    let base = unique_prefix("list");
    let prefix = format!("{base}/in/");
    let other_prefix = format!("{base}-other/");

    for i in 0..n {
        store
            .put(&format!("{prefix}{i:06}.bin"), Bytes::from(format!("v{i}")))
            .await
            .expect("put");
    }
    for i in 0..5 {
        store
            .put(&format!("{other_prefix}{i}.bin"), Bytes::from_static(b"x"))
            .await
            .expect("put");
    }

    let listed = store.list_prefix(&prefix).await.expect("list_prefix");
    assert_eq!(
        listed.len(),
        n,
        "expected {n} keys under {prefix}, got {}",
        listed.len()
    );
    let set: HashSet<String> = listed.into_iter().collect();
    for i in 0..n {
        let expected = format!("{prefix}{i:06}.bin");
        assert!(set.contains(&expected), "missing key {expected}");
    }
    for i in 0..5 {
        let unwanted = format!("{other_prefix}{i}.bin");
        assert!(!set.contains(&unwanted), "listing bled across prefixes");
    }

    // Cleanup so shared-emulator state doesn't grow across runs.
    for i in 0..n {
        let _ = store.delete(&format!("{prefix}{i:06}.bin")).await;
    }
    for i in 0..5 {
        let _ = store.delete(&format!("{other_prefix}{i}.bin")).await;
    }
}
