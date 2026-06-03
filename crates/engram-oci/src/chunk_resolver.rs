//! `OciChunkResolver` — fetches chunks as individual OCI blobs.
//!
//! ADR 0008 Phase 2 / ADR 0036: the origin tier in the `NVMe →
//! BlobStorage → OCI` fault path. Built from a chunked OCI
//! artifact's bootstrap layer: a precomputed index from `ChunkHash`
//! to `(blob_digest, length)` — where `blob_digest` is
//! `sha256:<chunk-hash>` (ADR 0036: one OCI blob per chunk).
//!
//! Construction is one-shot per (image, host). The bootstrap is
//! pulled at `image_cache::ensure_image` time (Phase 5 wiring) and
//! parsed into [`OciChunkIndex`]; the resolver borrows the index
//! and the configured [`OciClient`].
//!
//! # Verification contract
//!
//! Per the `ChunkResolver` trait, `fetch_chunk` returns bytes that
//! hash to the requested `ChunkHash`. `pull_chunk` already verifies
//! the response body against the blob digest (which *is* the chunk
//! hash); this resolver re-verifies against the bootstrap's
//! per-chunk `sha256` as cheap belt-and-braces. A mismatch surfaces
//! as `ChunkStoreError::HashMismatch`.
//!
//! # Errors
//!
//! - `ChunkStoreError::Origin(...)` — registry-side failure
//!   (network, 5xx, missing chunk in the index).
//! - `ChunkStoreError::HashMismatch` — fetched bytes don't hash to
//!   the requested chunk hash. Likely a stale bootstrap relative to
//!   the registry contents — caller should invalidate.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use engram_chunk_store::{ChunkHash, ChunkResolver, ChunkStoreError, Result};

use crate::OciClient;

/// Locator for a single chunk blob (ADR 0036: one OCI blob per
/// chunk, digest = `sha256:<chunk-hash>`).
///
/// Built at bootstrap-parse time. Cheap to clone; held by value
/// inside the [`OciChunkIndex`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OciBlobLocator {
    /// OCI blob digest (`sha256:<hex>`) of this chunk's blob.
    pub blob_digest: String,
    /// Chunk length (16 MiB for disk; the image's final chunk may
    /// be shorter).
    pub length: u64,
}

/// Lookup table from `ChunkHash` to `OciBlobLocator`. Built from
/// the parsed bootstrap; one per OCI image artifact.
///
/// The resolver consults this map on every `fetch_chunk` call.
/// A miss is reported as `ChunkStoreError::Origin` — the chunk
/// isn't part of this image, and the caller (a tiered resolver
/// or the host's resolution path) should fall through to a
/// different origin.
#[derive(Clone, Debug, Default)]
pub struct OciChunkIndex {
    entries: HashMap<ChunkHash, OciBlobLocator>,
}

impl OciChunkIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_entries(entries: HashMap<ChunkHash, OciBlobLocator>) -> Self {
        Self { entries }
    }

    pub fn insert(&mut self, hash: ChunkHash, loc: OciBlobLocator) -> Option<OciBlobLocator> {
        self.entries.insert(hash, loc)
    }

    pub fn get(&self, hash: &ChunkHash) -> Option<&OciBlobLocator> {
        self.entries.get(hash)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `ChunkResolver` impl that fetches chunks from an OCI registry,
/// one blob GET per chunk (ADR 0036).
///
/// One instance per OCI image (the index is image-specific). For
/// the multi-image case, wrap several `OciChunkResolver`s in a
/// composite resolver or use Phase 5's hybrid `image_cache`
/// dispatch.
#[derive(Clone)]
pub struct OciChunkResolver {
    client: OciClient,
    image_uri: String,
    index: OciChunkIndex,
}

impl OciChunkResolver {
    pub fn new(client: OciClient, image_uri: String, index: OciChunkIndex) -> Self {
        Self {
            client,
            image_uri,
            index,
        }
    }

    pub fn image_uri(&self) -> &str {
        &self.image_uri
    }

    pub fn index(&self) -> &OciChunkIndex {
        &self.index
    }
}

#[async_trait]
impl ChunkResolver for OciChunkResolver {
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        let loc = self.index.get(&hash).ok_or_else(|| {
            ChunkStoreError::Origin(format!(
                "chunk {} not in OCI index for {}",
                hash.to_hex(),
                self.image_uri
            ))
        })?;
        let bytes = self
            .client
            .pull_chunk(&self.image_uri, &loc.blob_digest, loc.length)
            .await
            .map_err(|e| ChunkStoreError::Origin(format!("{e}")))?;
        // `pull_chunk` already verified the body against the blob
        // digest (== the chunk hash); re-verify against the
        // bootstrap's expectation as cheap belt-and-braces — a stale
        // bootstrap surfaces here as a mismatch, not as corruption
        // downstream.
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
        // Cheap: just an index lookup. The registry isn't consulted;
        // an entry in the index means the chunk is *meant* to be
        // fetchable, not that the registry will succeed on every
        // fetch. For "is the chunk reachable at all" semantics this
        // is what callers want.
        Ok(self.index.get(&hash).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_round_trips_entries() {
        let hash = ChunkHash::of(b"abc");
        let loc = OciBlobLocator {
            blob_digest: "sha256:deadbeef".to_string(),
            length: 16 * 1024 * 1024,
        };
        let mut idx = OciChunkIndex::new();
        assert!(idx.is_empty());
        idx.insert(hash, loc.clone());
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get(&hash), Some(&loc));
        // Missing hash returns None.
        assert!(idx.get(&ChunkHash::of(b"missing")).is_none());
    }

    #[tokio::test]
    async fn chunk_exists_reflects_index_membership() {
        // We can exercise `chunk_exists` without any network — it's
        // just an index lookup. `fetch_chunk` needs a real registry,
        // so it's covered by gated integration tests.
        use crate::AnonymousResolver;
        use std::sync::Arc;

        let known = ChunkHash::of(b"present");
        let missing = ChunkHash::of(b"absent");
        let loc = OciBlobLocator {
            blob_digest: "sha256:abc".to_string(),
            length: 4,
        };
        let mut idx = OciChunkIndex::new();
        idx.insert(known, loc);

        let client = OciClient::new(Arc::new(AnonymousResolver));
        let resolver = OciChunkResolver::new(client, "localhost:5000/x:y".to_string(), idx);

        assert!(resolver.chunk_exists(known).await.unwrap());
        assert!(!resolver.chunk_exists(missing).await.unwrap());
    }

    #[tokio::test]
    async fn fetch_chunk_returns_origin_error_on_unknown_hash() {
        use crate::AnonymousResolver;
        use std::sync::Arc;

        let client = OciClient::new(Arc::new(AnonymousResolver));
        let resolver = OciChunkResolver::new(
            client,
            "localhost:5000/x:y".to_string(),
            OciChunkIndex::new(),
        );

        match resolver.fetch_chunk(ChunkHash::of(b"x")).await {
            Err(ChunkStoreError::Origin(msg)) => {
                assert!(msg.contains("not in OCI index"), "got: {msg}");
            }
            other => panic!("expected Origin error, got {other:?}"),
        }
    }
}
