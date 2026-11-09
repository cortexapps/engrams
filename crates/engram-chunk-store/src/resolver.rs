//! `ChunkResolver` — the object-store read behind the chunk cache.
//!
//! `ChunkStore` reads a chunk the local cache does not hold through its
//! resolver. The default, [`BlobStorageResolver`], reads the object store
//! at `chunks/sha256/<hex>`. On a host, the fill engine (`engram-fill`)
//! admits and schedules these reads. Writes go to `BlobStorage` directly.
//!
//! An implementation returns the object's bytes as stored. Every whole
//! chunk is checked by [`verify_fetched`] before it is served or stored:
//! by [`ChunkResolver::fetch_chunk`], by a cache fill, or by the fill
//! engine.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::traits::BlobStorage;

use crate::cache::verify_fetched;
use crate::error::Result;
use crate::manifest::ChunkHash;

/// Fetch chunk bytes by hash. Production code passes
/// `Arc<dyn ChunkResolver>` around.
#[async_trait]
pub trait ChunkResolver: Send + Sync {
    /// The chunk's bytes, checked against `hash`.
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        let bytes = self.fetch_chunk_unverified(hash).await?;
        verify_fetched(hash, &bytes).await?;
        Ok(bytes)
    }

    /// The chunk object's bytes as stored, for a caller that checks them
    /// with [`verify_fetched`] before it uses or stores them.
    async fn fetch_chunk_unverified(&self, hash: ChunkHash) -> Result<Bytes>;

    /// The bytes `[offset, offset + len)` of the chunk's object, fewer
    /// when the object ends first. Not hash-checked: only a whole chunk
    /// can be. The default fetches the whole chunk and cuts the range.
    async fn fetch_chunk_range_unverified(
        &self,
        hash: ChunkHash,
        offset: u64,
        len: u64,
    ) -> Result<Bytes> {
        let bytes = self.fetch_chunk_unverified(hash).await?;
        Ok(clip(&bytes, offset, len))
    }

    /// Whether the chunk is reachable through this resolver.
    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool>;
}

/// `bytes[offset..offset + len]`, cut short at the end of `bytes`.
fn clip(bytes: &Bytes, offset: u64, len: u64) -> Bytes {
    let size = bytes.len() as u64;
    let start = offset.min(size);
    let end = offset.saturating_add(len).min(size);
    bytes.slice(start as usize..end as usize)
}

/// `ChunkResolver` backed by a `BlobStorage`. Reads from
/// `chunks/sha256/<hex>`. This is the default resolver wired up by
/// `ChunkStore::new`.
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
    async fn fetch_chunk_unverified(&self, hash: ChunkHash) -> Result<Bytes> {
        Ok(self.blob.get(&hash.storage_key()).await?)
    }

    /// One ranged GET of the object. A range that runs past the end of
    /// the object is cut short at the end: the size comes from a HEAD,
    /// which only that rare case pays.
    async fn fetch_chunk_range_unverified(
        &self,
        hash: ChunkHash,
        offset: u64,
        len: u64,
    ) -> Result<Bytes> {
        let key = hash.storage_key();
        match self.blob.get_range(&key, offset, len).await {
            Err(engram_core::error::BlobError::RangeOutOfBounds { .. }) if len > 0 => {
                let size = self.blob.head(&key).await?.size_bytes;
                match size.saturating_sub(offset) {
                    0 => Ok(Bytes::new()),
                    left => Ok(self.blob.get_range(&key, offset, left.min(len)).await?),
                }
            }
            got => Ok(got?),
        }
    }

    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        Ok(self.blob.exists(&hash.storage_key()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ChunkStoreError;
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
    async fn a_ranged_fetch_returns_the_range_cut_short_at_the_object_end() {
        let (r, blob, _d) = resolver().await;
        let body: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let hash = ChunkHash::of(&body);
        blob.put(&hash.storage_key(), Bytes::from(body.clone()))
            .await
            .unwrap();
        let got = r
            .fetch_chunk_range_unverified(hash, 4096, 100)
            .await
            .unwrap();
        assert_eq!(&got[..], &body[4096..4196]);
        let tail = r
            .fetch_chunk_range_unverified(hash, 9_990, 64)
            .await
            .unwrap();
        assert_eq!(&tail[..], &body[9_990..]);
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
