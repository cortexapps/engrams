//! `ChunkStore` — the central API.
//!
//! Wraps a `BlobStorage` impl with chunk-store semantics:
//!
//! - **Chunks**: PUT with content-hash-derived key; GET verifies
//!   the hash matches before returning bytes.
//! - **Manifests**: PUT under `manifests/<id>/v<version>.json`,
//!   versioned. `put_manifest` is the commit point — it allocates
//!   a new version and refuses to overwrite an existing one
//!   (returns `VersionConflict`).
//! - **Forks**: `fork_manifest(src)` shallow-copies the source
//!   manifest under a new `manifest_id` at version 1, with
//!   `parent` set.
//! - **Traces**: PUT/GET working-set traces by manifest+host.
//! - **GC**: see `gc::run()` — sweeps chunks not referenced by any
//!   live manifest after a retention TTL.
//!
//! The local NVMe cache (`cache::ChunkCache`) is a separate layer
//! consumed by adapters; this module's GETs go through the
//! configured `ChunkResolver` (see ADR 0008 Phase 1). The default
//! resolver wraps `BlobStorage` directly — identical behavior to
//! pre-ADR-0008. Phase 2+ swaps in `TieredChunkResolver` to add
//! OCI fallback. Writes always go through `BlobStorage` directly;
//! the tiered story is read-only.

use std::sync::Arc;

use bytes::Bytes;
use engram_core::traits::BlobStorage;
use uuid::Uuid;

use crate::error::{ChunkStoreError, Result};
use crate::manifest::{ChunkHash, Manifest, ManifestRef};
use crate::resolver::{BlobStorageResolver, ChunkResolver};
use crate::working_set::{TraceRef, WorkingSetTrace};

/// Front door to chunked-immutable storage. Cheap to clone (it's
/// `Arc`s internally); pass clones around freely.
#[derive(Clone)]
pub struct ChunkStore {
    /// Write target for chunks, manifests, and traces. Always
    /// `BlobStorage` — writes don't have a tiered story.
    inner: Arc<dyn BlobStorage>,
    /// Read path for chunks. Defaults to a `BlobStorageResolver`
    /// over `inner`; production may swap in a tiered resolver
    /// (see ADR 0008 Phase 2 — `TieredChunkResolver`).
    resolver: Arc<dyn ChunkResolver>,
}

impl ChunkStore {
    /// Wrap any `BlobStorage` impl. Production uses
    /// `engram-storage-gcs::GcsBlobStorage`; tests +
    /// `--mode=all` use `engram-storage-local::LocalBlobStorage`.
    ///
    /// Chunk reads default to `BlobStorageResolver` over the same
    /// blob; use [`Self::with_resolver`] to swap in a tiered
    /// resolver for ADR 0008's OCI-fallback path.
    pub fn new(blob: Arc<dyn BlobStorage>) -> Self {
        let resolver: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(blob.clone()));
        Self {
            inner: blob,
            resolver,
        }
    }

    /// Replace the chunk-fetch resolver. Phase 2+ of ADR 0008
    /// uses this to install a `TieredChunkResolver` that falls
    /// through `BlobStorage → OCI`, with opportunistic
    /// write-through fill on miss.
    ///
    /// Phase 1 (today): no caller swaps the resolver; the default
    /// `BlobStorageResolver` preserves pre-ADR-0008 behavior.
    pub fn with_resolver(mut self, resolver: Arc<dyn ChunkResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Borrow the underlying blob storage. Crate-internal: GC and
    /// corruption tests need to bypass the typed surface for raw
    /// `list_prefix` / `head` / `delete`. Production callers go
    /// through `put_chunk`, `get_manifest`, etc.
    pub(crate) fn blob(&self) -> &Arc<dyn BlobStorage> {
        &self.inner
    }

    /// Cloneable handle to the BlobStorage backing this `ChunkStore`.
    /// Used by callers (ADR 0008 Phase 5+) that need to construct
    /// a `TieredChunkResolver` write-back to the same BlobStorage
    /// the store reads from. The handle is just an `Arc`-clone;
    /// PUT/GET semantics are unchanged.
    pub fn blob_storage(&self) -> Arc<dyn BlobStorage> {
        Arc::clone(&self.inner)
    }

    // ---------- chunks ----------

    /// PUT a chunk. Computes its hash, writes to
    /// `chunks/sha256/<hex>` (idempotent — same bytes write to the
    /// same key), returns the hash for inclusion in a manifest.
    pub async fn put_chunk(&self, body: &[u8]) -> Result<ChunkHash> {
        let hash = ChunkHash::of(body);
        let key = hash.storage_key();
        // Idempotency: if the chunk already exists, skip the PUT.
        // For S3/GCS this is a HEAD round-trip; for local fs it's
        // a stat. Both cheap relative to a multi-MB PUT.
        if self.inner.exists(&key).await? {
            return Ok(hash);
        }
        let bytes = Bytes::copy_from_slice(body);
        let _ = self.inner.put(&key, bytes).await?;
        tracing::trace!(hash = %hash, bytes = body.len(), "chunk PUT");
        Ok(hash)
    }

    /// GET a chunk's bytes. The resolver verifies the returned
    /// content hashes to the requested value — callers don't
    /// re-hash. With the default `BlobStorageResolver` this is a
    /// direct `BlobStorage` GET; with `TieredChunkResolver`
    /// (ADR 0008 Phase 2) it falls through `BlobStorage → OCI`
    /// with opportunistic write-through fill on miss.
    pub async fn get_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        self.resolver.fetch_chunk(hash).await
    }

    /// Existence check for a chunk. Routed through the resolver,
    /// so "exists" means "reachable through any tier" in the
    /// tiered world. With the default `BlobStorageResolver` this
    /// is equivalent to a `BlobStorage::exists` call.
    pub async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        self.resolver.chunk_exists(hash).await
    }

    // ---------- manifests ----------

    /// PUT a manifest at the given (`manifest_id`, `version`).
    /// Returns `VersionConflict` if that version already exists —
    /// the caller raced another writer and should re-read,
    /// re-apply, and retry at a new version.
    ///
    /// **The first call for a new `manifest_id` should use
    /// `version = 1`.** Use `next_version()` on a `ManifestRef`
    /// from `get_latest_manifest` to advance.
    pub async fn put_manifest(&self, r: ManifestRef, m: &Manifest) -> Result<()> {
        m.validate()?;
        let key = r.storage_key();
        if self.inner.exists(&key).await? {
            // Look up the latest version to surface in the error.
            let latest = self
                .latest_manifest_version(r.manifest_id)
                .await?
                .unwrap_or(0);
            return Err(ChunkStoreError::VersionConflict {
                manifest_id: r.manifest_id,
                attempted: r.version,
                latest,
            });
        }
        let bytes = serde_json::to_vec(m)?;
        let _ = self.inner.put(&key, Bytes::from(bytes)).await?;
        tracing::debug!(
            manifest_id = %r.manifest_id,
            version = r.version,
            kind = ?m.kind,
            total_bytes = m.total_bytes,
            chunks = m.chunks.len(),
            "manifest PUT"
        );
        Ok(())
    }

    /// GET a specific manifest version. Returns `NotFound` if no
    /// such version was ever written (the version was never
    /// committed, or was GC'd).
    pub async fn get_manifest(&self, r: ManifestRef) -> Result<Manifest> {
        let bytes = self.inner.get(&r.storage_key()).await?;
        let m: Manifest = serde_json::from_slice(&bytes)?;
        m.validate()?;
        Ok(m)
    }

    /// Returns the highest `version` stored for `manifest_id`, or
    /// `None` if no versions exist. Lists the manifest's prefix
    /// directly via `BlobStorage::list_prefix` and parses out the
    /// version numbers.
    pub async fn latest_manifest_version(&self, manifest_id: Uuid) -> Result<Option<u64>> {
        let prefix = ManifestRef::id_prefix(manifest_id);
        let keys = self.inner.list_prefix(&prefix).await?;
        let mut max: Option<u64> = None;
        for key in keys {
            // Parse out "vN.json" from the tail.
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(num) = rest.strip_prefix('v').and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            if let Ok(v) = num.parse::<u64>() {
                max = Some(max.map_or(v, |cur| cur.max(v)));
            }
        }
        Ok(max)
    }

    /// Convenience: fetch the highest existing version of a
    /// manifest. Returns the `ManifestRef` alongside its body.
    pub async fn get_latest_manifest(
        &self,
        manifest_id: Uuid,
    ) -> Result<Option<(ManifestRef, Manifest)>> {
        let Some(version) = self.latest_manifest_version(manifest_id).await? else {
            return Ok(None);
        };
        let r = ManifestRef {
            manifest_id,
            version,
        };
        let m = self.get_manifest(r).await?;
        Ok(Some((r, m)))
    }

    /// Fork: copy `src`'s manifest under a fresh `manifest_id` at
    /// version 1, with `parent = Some(src)`. Chunks are unchanged
    /// (and unreferenced extra writes are avoided — content
    /// addressing means the forked manifest already references
    /// the same immutable chunks). Cheap; ~one PUT of a small
    /// JSON blob.
    pub async fn fork_manifest(&self, src: ManifestRef) -> Result<ManifestRef> {
        let mut m = self.get_manifest(src).await?;
        m.parent = Some(src);
        // Working-set traces are host-local; don't inherit them on
        // fork — the fork is a different logical session and its
        // access patterns may differ.
        m.working_set_trace = None;
        let new_ref = ManifestRef::new();
        self.put_manifest(new_ref, &m).await?;
        Ok(new_ref)
    }

    // ---------- working-set traces ----------

    /// PUT a working-set trace. Overwrites any prior trace for the
    /// same `(manifest_id, host_id)` — replay always uses the
    /// latest. Caller decides cadence (e.g. record once on first
    /// restore per host, refresh occasionally as access patterns
    /// drift).
    pub async fn put_trace(&self, r: TraceRef, t: &WorkingSetTrace) -> Result<()> {
        let bytes = serde_json::to_vec(t)?;
        let _ = self.inner.put(&r.storage_key(), Bytes::from(bytes)).await?;
        Ok(())
    }

    /// GET a working-set trace. `None` if no trace was ever
    /// recorded — first restore on a host hits this and falls
    /// back to lazy UFFD without prefault.
    pub async fn get_trace(&self, r: TraceRef) -> Result<Option<WorkingSetTrace>> {
        match self.inner.get(&r.storage_key()).await {
            Ok(bytes) => {
                let t: WorkingSetTrace = serde_json::from_slice(&bytes)?;
                Ok(Some(t))
            }
            Err(engram_core::error::BlobError::NotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChunkRef, ManifestKind};
    use engram_storage_local::LocalBlobStorage;

    async fn store() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (ChunkStore::new(blob), dir)
    }

    #[tokio::test]
    async fn put_then_get_chunk_round_trips() {
        let (s, _d) = store().await;
        let body = b"hello chunk world";
        let h = s.put_chunk(body).await.unwrap();
        let back = s.get_chunk(h).await.unwrap();
        assert_eq!(&back[..], body);
    }

    #[tokio::test]
    async fn put_chunk_is_idempotent_on_identical_bytes() {
        let (s, _d) = store().await;
        let body = b"same bytes twice";
        let h1 = s.put_chunk(body).await.unwrap();
        let h2 = s.put_chunk(body).await.unwrap();
        assert_eq!(h1, h2);
        // And the body is still retrievable.
        let back = s.get_chunk(h1).await.unwrap();
        assert_eq!(&back[..], body);
    }

    #[tokio::test]
    async fn put_manifest_at_version_1_then_get() {
        let (s, _d) = store().await;
        let r = ManifestRef::new();
        let mut m = Manifest::empty(ManifestKind::Disk, 16 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"a"),
        });
        s.put_manifest(r, &m).await.unwrap();
        let back = s.get_manifest(r).await.unwrap();
        assert_eq!(back, m);
    }

    #[tokio::test]
    async fn put_manifest_twice_at_same_version_returns_conflict() {
        let (s, _d) = store().await;
        let r = ManifestRef::new();
        let m = Manifest::empty(ManifestKind::Disk, 0);
        s.put_manifest(r, &m).await.unwrap();
        match s.put_manifest(r, &m).await {
            Err(ChunkStoreError::VersionConflict { latest, .. }) => {
                assert_eq!(latest, 1);
            }
            other => panic!("expected VersionConflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn latest_manifest_version_returns_none_for_unknown_id() {
        let (s, _d) = store().await;
        let id = Uuid::new_v4();
        assert!(s.latest_manifest_version(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn latest_manifest_version_finds_highest_after_many_writes() {
        let (s, _d) = store().await;
        let id = Uuid::new_v4();
        let m = Manifest::empty(ManifestKind::Disk, 0);
        for v in 1..=7 {
            s.put_manifest(
                ManifestRef {
                    manifest_id: id,
                    version: v,
                },
                &m,
            )
            .await
            .unwrap();
        }
        assert_eq!(s.latest_manifest_version(id).await.unwrap(), Some(7));
        let (r, _back) = s.get_latest_manifest(id).await.unwrap().unwrap();
        assert_eq!(r.version, 7);
    }

    #[tokio::test]
    async fn fork_manifest_produces_new_id_with_parent_set() {
        let (s, _d) = store().await;
        let src = ManifestRef::new();
        let mut m = Manifest::empty(ManifestKind::Disk, 16 * 1024 * 1024);
        m.chunks.push(ChunkRef {
            offset: 0,
            hash: ChunkHash::of(b"x"),
        });
        s.put_manifest(src, &m).await.unwrap();

        let forked = s.fork_manifest(src).await.unwrap();
        assert_ne!(forked.manifest_id, src.manifest_id);
        assert_eq!(forked.version, 1);

        let forked_m = s.get_manifest(forked).await.unwrap();
        assert_eq!(forked_m.parent, Some(src));
        assert_eq!(forked_m.chunks, m.chunks); // chunk list unchanged
        assert!(forked_m.working_set_trace.is_none());
    }

    #[tokio::test]
    async fn get_chunk_detects_storage_corruption() {
        // Write a chunk via the API; then mangle the bytes
        // directly through the underlying blob storage and verify
        // the next get_chunk returns HashMismatch.
        let (s, _d) = store().await;
        let h = s.put_chunk(b"original").await.unwrap();
        // Overwrite the stored chunk with different bytes — bypass
        // the chunk-store API.
        let blob = s.blob().clone();
        blob.put(&h.storage_key(), Bytes::from_static(b"corrupted"))
            .await
            .unwrap();
        match s.get_chunk(h).await {
            Err(ChunkStoreError::HashMismatch { .. }) => {}
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_and_get_trace_round_trips() {
        let (s, _d) = store().await;
        let r = TraceRef::canonical(Uuid::new_v4());
        let mut t = WorkingSetTrace::new(2, 5000);
        t.chunks.push(ChunkHash::of(b"a"));
        t.chunks.push(ChunkHash::of(b"b"));
        s.put_trace(r, &t).await.unwrap();
        let back = s.get_trace(r).await.unwrap().unwrap();
        assert_eq!(back, t);
    }

    #[tokio::test]
    async fn get_trace_returns_none_when_absent() {
        let (s, _d) = store().await;
        let r = TraceRef::canonical(Uuid::new_v4());
        assert!(s.get_trace(r).await.unwrap().is_none());
    }

    // ---- ADR 0008 Phase 1: ChunkResolver indirection ----

    /// `with_resolver` swaps the chunk-fetch path. A custom resolver
    /// that returns a sentinel ChunkHash for any request lets us
    /// observe that `get_chunk` and `chunk_exists` flow through it
    /// rather than `inner`.
    #[tokio::test]
    async fn with_resolver_redirects_chunk_reads() {
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingResolver {
            calls: AtomicUsize,
            body: Bytes,
        }

        #[async_trait]
        impl crate::resolver::ChunkResolver for CountingResolver {
            async fn fetch_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                // Sanity: the returned bytes must hash to the request
                // for the contract to hold; precompute a body that
                // satisfies that.
                assert_eq!(hash, ChunkHash::of(&self.body));
                Ok(self.body.clone())
            }
            async fn chunk_exists(&self, _hash: ChunkHash) -> Result<bool> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(true)
            }
        }

        let (base, _d) = store().await;
        let body = Bytes::from_static(b"resolver redirect target");
        let hash = ChunkHash::of(&body);
        let counting = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
            body,
        });
        let s = base.with_resolver(counting.clone());

        // get_chunk + chunk_exists both flow through the resolver,
        // not the underlying BlobStorage (which never had the chunk).
        let bytes = s.get_chunk(hash).await.unwrap();
        assert_eq!(&bytes[..], b"resolver redirect target");
        assert!(s.chunk_exists(hash).await.unwrap());

        assert_eq!(counting.calls.load(Ordering::Relaxed), 2);
    }

    /// Default `ChunkStore::new` keeps pre-ADR-0008 semantics:
    /// writes via `put_chunk` are immediately readable via
    /// `get_chunk`, with no resolver swap needed.
    #[tokio::test]
    async fn default_resolver_preserves_legacy_behavior() {
        let (s, _d) = store().await;
        let body = b"default behavior";
        let h = s.put_chunk(body).await.unwrap();
        let back = s.get_chunk(h).await.unwrap();
        assert_eq!(&back[..], body);
        assert!(s.chunk_exists(h).await.unwrap());
    }
}
