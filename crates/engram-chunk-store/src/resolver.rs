//! `ChunkResolver` — abstraction for the chunk-fetch path.
//!
//! Introduced in ADR 0008 (Phase 1). Today's `ChunkStore` fetches
//! chunks directly from a `BlobStorage` at `chunks/sha256/<hex>`.
//! That continues to be the default behavior via
//! `BlobStorageResolver`.
//!
//! Subsequent phases of ADR 0008 add more resolvers:
//!
//! - `OciChunkResolver` (Phase 2) — Range GET against a chunked
//!   OCI blob layer, for chunks that live in the OCI registry as
//!   part of a Nydus-shaped image artifact.
//! - `TieredChunkResolver` (Phase 2) — composes
//!   `BlobStorageResolver` and `OciChunkResolver` to give a
//!   `NVMe → BlobStorage → OCI` fault path with opportunistic
//!   write-through fill to BlobStorage.
//!
//! Writes always go through `BlobStorage` directly — the
//! tiered story is read-only. BlobStorage is both the snapshot
//! durability layer and the regional cache for image chunks; it's
//! always the right write target.
//!
//! # Hash verification
//!
//! Implementations **must** verify the returned bytes hash to the
//! requested `ChunkHash`. The contract is "byte-correct or error";
//! callers (e.g. `ChunkStore::get_chunk`) rely on this and do not
//! re-verify.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::traits::BlobStorage;

use crate::error::{ChunkStoreError, Result};
use crate::manifest::ChunkHash;

/// Fetch chunk bytes by hash. The contract:
///
/// - `fetch_chunk` returns bytes that hash to the requested
///   `ChunkHash`, or an error. Implementations verify before
///   returning — callers can rely on the result being
///   byte-correct without re-hashing.
/// - `chunk_exists` returns whether the chunk is reachable
///   through this resolver. For tiered resolvers this means
///   "reachable via *any* tier" — useful for caching decisions.
///
/// Cheap to clone via `Arc`. Production code passes
/// `Arc<dyn ChunkResolver>` around.
#[async_trait]
pub trait ChunkResolver: Send + Sync {
    /// Fetch a chunk's bytes by hash.
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes>;

    /// Whether the chunk is reachable through this resolver.
    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool>;
}

/// `ChunkResolver` backed by a `BlobStorage`. Reads from
/// `chunks/sha256/<hex>`; identical behavior to the pre-ADR-0008
/// `ChunkStore::get_chunk` implementation.
///
/// This is the default resolver wired up by `ChunkStore::new`.
#[derive(Clone)]
pub struct BlobStorageResolver {
    blob: Arc<dyn BlobStorage>,
}

impl BlobStorageResolver {
    pub fn new(blob: Arc<dyn BlobStorage>) -> Self {
        Self { blob }
    }
}

#[async_trait]
impl ChunkResolver for BlobStorageResolver {
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        let key = hash.storage_key();
        let bytes = self.blob.get(&key).await?;
        let actual = ChunkHash::of(&bytes);
        if actual != hash {
            return Err(ChunkStoreError::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(bytes)
    }

    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        Ok(self.blob.exists(&hash.storage_key()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_storage_local::LocalBlobStorage;

    async fn resolver() -> (BlobStorageResolver, Arc<dyn BlobStorage>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (BlobStorageResolver::new(blob.clone()), blob, dir)
    }

    #[tokio::test]
    async fn blob_storage_resolver_round_trips_chunk_bytes() {
        let (r, blob, _d) = resolver().await;
        let body = b"hello resolver";
        let hash = ChunkHash::of(body);
        // Write via the raw blob (the resolver is read-only).
        blob.put(&hash.storage_key(), Bytes::from_static(body))
            .await
            .unwrap();
        let back = r.fetch_chunk(hash).await.unwrap();
        assert_eq!(&back[..], body);
    }

    #[tokio::test]
    async fn blob_storage_resolver_detects_storage_corruption() {
        let (r, blob, _d) = resolver().await;
        let hash = ChunkHash::of(b"original");
        // Stash different bytes at the expected key.
        blob.put(&hash.storage_key(), Bytes::from_static(b"corrupted"))
            .await
            .unwrap();
        match r.fetch_chunk(hash).await {
            Err(ChunkStoreError::HashMismatch { .. }) => {}
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn blob_storage_resolver_chunk_exists_reflects_blob_state() {
        let (r, blob, _d) = resolver().await;
        let hash = ChunkHash::of(b"some chunk");
        assert!(!r.chunk_exists(hash).await.unwrap());
        blob.put(&hash.storage_key(), Bytes::from_static(b"some chunk"))
            .await
            .unwrap();
        assert!(r.chunk_exists(hash).await.unwrap());
    }
}
