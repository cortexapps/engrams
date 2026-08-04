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

/// `ChunkResolver` that composes other resolvers as a fallback
/// chain, with optional write-through fill to a `BlobStorage`
/// cache on origin-tier hits.
///
/// ADR 0008 Phase 2: the production wiring is
/// `Tiered([BlobStorageResolver, OciChunkResolver],
/// write_back: Some(blob))`, giving `BlobStorage → OCI` with
/// CDN-fill semantics. (The NVMe `ChunkCache` LRU sits in front
/// as a separate hot-path layer; it's not modeled as a resolver
/// because it has different lifetime semantics.)
///
/// On `fetch_chunk(hash)`:
///
/// 1. Try each tier in order. First success wins.
/// 2. If the hit was at tier > 0 (i.e., not the cache tier) and
///    `write_back` is set, write the chunk to BlobStorage at
///    `chunks/sha256/<hex>` so subsequent reads hit the cache.
///    Write-back failures are logged but not returned — a fresh
///    fetch returned good bytes; failing the read because the
///    cache fill failed would punish the caller for a separate
///    fault.
///
/// On `chunk_exists(hash)`: returns `true` if *any* tier reports
/// existence. "Exists somewhere reachable."
pub struct TieredChunkResolver {
    tiers: Vec<Arc<dyn ChunkResolver>>,
    write_back: Option<Arc<dyn BlobStorage>>,
}

impl TieredChunkResolver {
    /// Build a tiered resolver. `tiers` are tried in order; first
    /// hit wins. Pass `write_back: Some(blob)` to enable CDN-fill
    /// to BlobStorage on origin-tier hits (typical production
    /// wiring: write back to the same BlobStorage that backs the
    /// cache tier).
    pub fn new(
        tiers: Vec<Arc<dyn ChunkResolver>>,
        write_back: Option<Arc<dyn BlobStorage>>,
    ) -> Self {
        Self { tiers, write_back }
    }
}

#[async_trait]
impl ChunkResolver for TieredChunkResolver {
    async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        if self.tiers.is_empty() {
            return Err(ChunkStoreError::Origin(
                "TieredChunkResolver: no tiers configured".to_string(),
            ));
        }
        let mut last_err: Option<ChunkStoreError> = None;
        for (idx, tier) in self.tiers.iter().enumerate() {
            match tier.fetch_chunk(hash).await {
                Ok(bytes) => {
                    if idx > 0 {
                        self.fill_cache(hash, bytes.clone()).await;
                    }
                    return Ok(bytes);
                }
                Err(e) => {
                    tracing::trace!(
                        tier = idx,
                        hash = %hash,
                        error = %e,
                        "tier miss, falling through"
                    );
                    last_err = Some(e);
                    continue;
                }
            }
        }
        // All tiers missed. Surface the last error so the caller
        // sees a real diagnostic, not a generic "not found."
        Err(last_err.unwrap_or_else(|| {
            ChunkStoreError::Origin(format!(
                "TieredChunkResolver: all tiers missed for {}",
                hash.to_hex()
            ))
        }))
    }

    async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        for tier in &self.tiers {
            if tier.chunk_exists(hash).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl TieredChunkResolver {
    /// Write a freshly-fetched chunk into the cache tier
    /// (BlobStorage). Best-effort: failures are logged but don't
    /// fail the parent `fetch_chunk` call.
    async fn fill_cache(&self, hash: ChunkHash, bytes: Bytes) {
        let Some(blob) = &self.write_back else {
            return;
        };
        let key = hash.storage_key();
        // Skip the PUT if the cache already has it — another concurrent
        // fetch may have raced and won. PUT-idempotent backends would
        // be fine either way; this just avoids a needless network call.
        match blob.exists(&key).await {
            Ok(true) => {
                tracing::trace!(hash = %hash, "cache-fill skipped: already present");
                return;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::debug!(hash = %hash, error = %e, "cache-fill exists-check failed");
                return;
            }
        }
        match blob.put(&key, bytes).await {
            Ok(_) => {
                tracing::debug!(hash = %hash, "cache-fill PUT ok");
            }
            Err(e) => {
                tracing::warn!(hash = %hash, error = %e, "cache-fill PUT failed");
            }
        }
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

    // ---- TieredChunkResolver ----

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test double: returns prepared bytes when asked for `hash`,
    /// otherwise `Origin` error. Counts calls.
    struct CannedResolver {
        hash: ChunkHash,
        body: Bytes,
        fetch_calls: AtomicUsize,
        exists_calls: AtomicUsize,
    }

    #[async_trait]
    impl ChunkResolver for CannedResolver {
        async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
            self.fetch_calls.fetch_add(1, Ordering::Relaxed);
            if hash == self.hash {
                Ok(self.body.clone())
            } else {
                Err(ChunkStoreError::Origin(format!(
                    "canned: unknown hash {}",
                    hash.to_hex()
                )))
            }
        }
        async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
            self.exists_calls.fetch_add(1, Ordering::Relaxed);
            Ok(hash == self.hash)
        }
    }

    /// Test double: always misses.
    struct AlwaysMissResolver {
        fetch_calls: AtomicUsize,
    }

    #[async_trait]
    impl ChunkResolver for AlwaysMissResolver {
        async fn fetch_chunk(&self, _hash: ChunkHash) -> Result<Bytes> {
            self.fetch_calls.fetch_add(1, Ordering::Relaxed);
            Err(ChunkStoreError::Origin("always miss".into()))
        }
        async fn chunk_exists(&self, _hash: ChunkHash) -> Result<bool> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn tiered_resolver_returns_first_tier_hit_without_consulting_origin() {
        let body = Bytes::from_static(b"cache hit");
        let hash = ChunkHash::of(&body);

        let cache = Arc::new(CannedResolver {
            hash,
            body: body.clone(),
            fetch_calls: AtomicUsize::new(0),
            exists_calls: AtomicUsize::new(0),
        });
        let origin = Arc::new(AlwaysMissResolver {
            fetch_calls: AtomicUsize::new(0),
        });

        let tiered = TieredChunkResolver::new(
            vec![cache.clone(), origin.clone()],
            None, // no write-back for this test
        );

        let got = tiered.fetch_chunk(hash).await.unwrap();
        assert_eq!(got, body);
        // Origin was never asked because cache hit.
        assert_eq!(origin.fetch_calls.load(Ordering::Relaxed), 0);
        assert_eq!(cache.fetch_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn tiered_resolver_falls_through_to_origin_on_cache_miss() {
        let body = Bytes::from_static(b"origin hit");
        let hash = ChunkHash::of(&body);

        let cache = Arc::new(AlwaysMissResolver {
            fetch_calls: AtomicUsize::new(0),
        });
        let origin = Arc::new(CannedResolver {
            hash,
            body: body.clone(),
            fetch_calls: AtomicUsize::new(0),
            exists_calls: AtomicUsize::new(0),
        });

        let tiered = TieredChunkResolver::new(vec![cache.clone(), origin.clone()], None);

        let got = tiered.fetch_chunk(hash).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(cache.fetch_calls.load(Ordering::Relaxed), 1);
        assert_eq!(origin.fetch_calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn tiered_resolver_writes_back_to_blob_on_origin_hit() {
        let body = Bytes::from_static(b"cdn fill bytes");
        let hash = ChunkHash::of(&body);

        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));

        // Cache tier reads from the (empty) blob — cache miss
        // since we haven't written anything yet.
        let cache = Arc::new(BlobStorageResolver::new(blob.clone()));
        let origin = Arc::new(CannedResolver {
            hash,
            body: body.clone(),
            fetch_calls: AtomicUsize::new(0),
            exists_calls: AtomicUsize::new(0),
        });

        let tiered =
            TieredChunkResolver::new(vec![cache.clone(), origin.clone()], Some(blob.clone()));

        // First read: cache miss → origin hit → write-back.
        let got = tiered.fetch_chunk(hash).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(origin.fetch_calls.load(Ordering::Relaxed), 1);

        // Verify the write-back landed: blob now holds the chunk.
        assert!(blob.exists(&hash.storage_key()).await.unwrap());

        // Second read: cache should hit (blob has it now). Origin
        // call-count must not increment.
        let _ = tiered.fetch_chunk(hash).await.unwrap();
        assert_eq!(
            origin.fetch_calls.load(Ordering::Relaxed),
            1,
            "second fetch should hit cache, not origin"
        );
    }

    #[tokio::test]
    async fn tiered_resolver_surfaces_last_error_when_all_tiers_miss() {
        let cache = Arc::new(AlwaysMissResolver {
            fetch_calls: AtomicUsize::new(0),
        });
        let origin = Arc::new(AlwaysMissResolver {
            fetch_calls: AtomicUsize::new(0),
        });
        let tiered = TieredChunkResolver::new(vec![cache, origin], None);

        let hash = ChunkHash::of(b"anything");
        match tiered.fetch_chunk(hash).await {
            Err(ChunkStoreError::Origin(msg)) => {
                assert!(msg.contains("always miss"), "got: {msg}");
            }
            other => panic!("expected Origin error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tiered_resolver_chunk_exists_returns_true_if_any_tier_has_it() {
        let body = Bytes::from_static(b"present somewhere");
        let hash = ChunkHash::of(&body);
        let cache = Arc::new(AlwaysMissResolver {
            fetch_calls: AtomicUsize::new(0),
        });
        let origin = Arc::new(CannedResolver {
            hash,
            body,
            fetch_calls: AtomicUsize::new(0),
            exists_calls: AtomicUsize::new(0),
        });
        let tiered = TieredChunkResolver::new(vec![cache, origin], None);
        assert!(tiered.chunk_exists(hash).await.unwrap());
        assert!(!tiered.chunk_exists(ChunkHash::of(b"absent")).await.unwrap());
    }

    #[tokio::test]
    async fn tiered_resolver_with_no_tiers_errors_explicitly() {
        let tiered = TieredChunkResolver::new(vec![], None);
        match tiered.fetch_chunk(ChunkHash::of(b"x")).await {
            Err(ChunkStoreError::Origin(msg)) => {
                assert!(msg.contains("no tiers"), "got: {msg}");
            }
            other => panic!("expected Origin error, got {other:?}"),
        }
    }
}
