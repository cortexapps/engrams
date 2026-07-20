//! `MemBlobStorage` — a deterministic, fully in-memory [`BlobStorage`] for
//! the simulator's faithful world (ADR 0098 D5, determinism-audit item 7).
//!
//! # Why in-memory, not `LocalBlobStorage` over a tempdir
//!
//! The coordinator sim runs on a current-thread tokio runtime with
//! `start_paused = true`: virtual time only advances when the runtime goes
//! **idle**. `engram-storage-local::LocalBlobStorage` performs its I/O through
//! `tokio::fs`, which hands each operation to tokio's **blocking thread pool**
//! and awaits its completion. While that off-thread op is in flight the
//! current-thread runtime has nothing else ready, so it declares itself idle
//! and **auto-advances the paused clock** — by an amount that depends on how
//! long the real filesystem took. That makes virtual time a function of real
//! disk latency, host load, and platform: the same seed replays to *different*
//! virtual timelines across machines and under concurrent load. The #722
//! re-land divergence (green on macOS, invariant on Linux, only under
//! `nextest --workspace` load) was the visible tail of this class; #795's
//! per-world tempdir isolation removed cross-world *contamination* but left the
//! real-time leak, and #797's extra blob-HEAD await re-exposed it.
//!
//! A `Mutex<BTreeMap>` store completes every operation **synchronously inside
//! the poll** — no blocking-pool handoff, so no idle window ever opens and the
//! paused clock never auto-advances mid-I/O. Virtual time is decided only by
//! the scheduler's explicit `advance`s. That is the rule (ADR 0098 item 7):
//! sim worlds back `Services.blob` with a deterministic in-memory backend;
//! real-fs backends are only for crash-semantics seams that own their own
//! determinism story.
//!
//! # Fidelity
//!
//! - **Sorted listing.** `list_prefix` returns keys in lexicographic order —
//!   the GCS/S3 `list` contract, which prod runs against. A `BTreeMap` yields
//!   that ordering for free (no `read_dir`-order leak is even expressible).
//! - **Pure string prefix.** `list_prefix` matches keys by `starts_with`, the
//!   object-store semantics; every sim consumer (`snapshot_blob_gc`,
//!   `ChunkStore::latest_manifest_version`) already re-parses the returned keys
//!   with `strip_prefix`, so this is exactly what they expect.
//! - **Idempotent delete**, `NotFound` on missing get/head — the trait
//!   contract.

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use engram_core::error::BlobError;
use engram_core::traits::storage::{BlobObjectMeta, BlobStorage, ByteStream};
use futures::StreamExt;
use parking_lot::Mutex;

/// A deterministic, fully in-memory [`BlobStorage`]. Cheap to `clone`-share
/// (wrap in `Arc`): every clone of the `Arc` sees the same map, mirroring prod,
/// where one pod + its host share one bucket. Two *different* sim worlds get
/// two different `MemBlobStorage` values, so cross-world residue is impossible
/// by construction (the property #795's per-world tempdir also gave).
#[derive(Debug, Default)]
pub struct MemBlobStorage {
    map: Mutex<BTreeMap<String, Bytes>>,
}

impl MemBlobStorage {
    /// A fresh, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored objects — for test assertions only.
    pub fn len(&self) -> usize {
        self.map.lock().len()
    }

    /// Whether the store holds no objects.
    pub fn is_empty(&self) -> bool {
        self.map.lock().is_empty()
    }
}

#[async_trait]
impl BlobStorage for MemBlobStorage {
    async fn put_streaming(&self, key: &str, mut body: ByteStream) -> Result<u64, BlobError> {
        // Drain the body fully before publishing the key, so a key never
        // appears half-written (the trait's atomicity contract). In the sim
        // the body is always an in-memory `ByteStream::from_bytes`, so this
        // loop resolves synchronously — no blocking-pool handoff, no paused-
        // clock auto-advance.
        let mut buf = BytesMut::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk?);
        }
        let n = buf.len() as u64;
        self.map.lock().insert(key.to_string(), buf.freeze());
        Ok(n)
    }

    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        match self.map.lock().get(key) {
            Some(b) => Ok(ByteStream::from_bytes(b.clone())),
            None => Err(BlobError::NotFound),
        }
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        match self.map.lock().get(key) {
            Some(b) => Ok(BlobObjectMeta {
                size_bytes: b.len() as u64,
                etag: None,
            }),
            None => Err(BlobError::NotFound),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        // Idempotent: absent key is success.
        self.map.lock().remove(key);
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        // Pure string-prefix match (the GCS/S3 object-store contract), and the
        // `BTreeMap` iterates keys in lexicographic order — so the returned Vec
        // is already sorted, with no `read_dir`-order determinism leak
        // expressible. Consumers re-parse with `strip_prefix`, so string-prefix
        // is exactly the semantics they rely on.
        let map = self.map.lock();
        Ok(map
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_head_delete_roundtrip() {
        let s = MemBlobStorage::new();
        assert!(s.is_empty());
        s.put("a/1", Bytes::from_static(b"hello")).await.unwrap();
        assert_eq!(s.len(), 1);

        let got = s.get("a/1").await.unwrap();
        assert_eq!(&got[..], b"hello");

        let meta = s.head("a/1").await.unwrap();
        assert_eq!(meta.size_bytes, 5);

        assert!(s.exists("a/1").await.unwrap());
        assert!(!s.exists("a/2").await.unwrap());

        s.delete("a/1").await.unwrap();
        assert!(matches!(s.get("a/1").await, Err(BlobError::NotFound)));
        // Idempotent delete of an absent key.
        s.delete("a/1").await.unwrap();
    }

    #[tokio::test]
    async fn list_prefix_is_sorted_and_string_prefixed() {
        let s = MemBlobStorage::new();
        // Insert deliberately out of lexicographic order.
        for k in ["snapshots/z/state.bin", "snapshots/a/state.bin", "chunks/x"] {
            s.put(k, Bytes::from_static(b"v")).await.unwrap();
        }
        let snaps = s.list_prefix("snapshots/").await.unwrap();
        assert_eq!(
            snaps,
            vec![
                "snapshots/a/state.bin".to_string(),
                "snapshots/z/state.bin".to_string()
            ],
            "keys must come back lexicographically sorted (the GCS/S3 contract)"
        );

        let all = s.list_prefix("").await.unwrap();
        assert_eq!(
            all,
            vec![
                "chunks/x".to_string(),
                "snapshots/a/state.bin".to_string(),
                "snapshots/z/state.bin".to_string()
            ]
        );

        assert!(s.list_prefix("never-existed").await.unwrap().is_empty());
    }
}
