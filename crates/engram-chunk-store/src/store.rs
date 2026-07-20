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
//!
//! Chunk-store GC ships as ADR 0016 Phase C: the coordinator's
//! `chunk_gc.rs` mark-and-sweep reclaims unreferenced BlobStorage
//! chunks. (The local NVMe cache has its own free-floor eviction —
//! see `cache::ChunkCache`.)
//!
//! The local NVMe cache (`cache::ChunkCache`) is consumed by adapters,
//! and — when wired in via [`ChunkStore::with_chunk_cache`] — is also a
//! **write-through** tier for `put_chunk`: a chunk written durably to
//! `BlobStorage` is additionally populated into the local cache, so the
//! host that *produced* it reads it back locally (μs) instead of
//! re-fetching from the blob store (~110 ms) on the next resume. This is
//! symmetric with the read side (`TieredChunkResolver`'s populate-on-miss)
//! and closes the asymmetry that made idle-eviction capture ship the
//! divergent memory chunks to GCS while discarding the local copy the host
//! already had (so every resume re-paged its working set from GCS). GETs go
//! through the configured `ChunkResolver` (ADR 0008); the default resolver
//! wraps `BlobStorage` directly, and for chunked-OCI images the host-agent
//! installs a `TieredChunkResolver` (`BlobStorage → OCI`) via
//! [`ChunkStore::with_resolver`].

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
    /// Durable write target for chunks, manifests, and traces —
    /// `BlobStorage`. Always written (the durability tier).
    inner: Arc<dyn BlobStorage>,
    /// Read path for chunks. Defaults to a `BlobStorageResolver`
    /// over `inner`; production may swap in a tiered resolver
    /// (see ADR 0008 Phase 2 — `TieredChunkResolver`).
    resolver: Arc<dyn ChunkResolver>,
    /// Optional local write-through cache. When set (host-agent), a
    /// `put_chunk` that durably writes a *new* chunk also populates this
    /// cache, so the producing host reads it back locally instead of
    /// re-fetching from `inner`. `None` on the coordinator (no local
    /// resume-serving cache) — there `put_chunk` is blob-only, unchanged.
    cache: Option<crate::cache::ChunkCache>,
    /// Optional host-global upload budget (ADR 0088 addendum): every
    /// chunk PUT body acquires one permit, so concurrent bulk-upload
    /// workloads (materialize / memory seed / disk flush) share the
    /// NIC instead of stacking on it. `None` (coordinator, tests) =
    /// unbudgeted, unchanged.
    upload_budget: Option<crate::budget::UploadBudget>,
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
            cache: None,
            upload_budget: None,
        }
    }

    /// Wire the host-global [`UploadBudget`](crate::budget::UploadBudget):
    /// every chunk PUT body (checked and unchecked) acquires one permit
    /// after the dedup HEAD short-circuit — deduped puts, HEADs, and
    /// manifest/trace PUTs consume nothing. Pass the SAME budget clone
    /// to every store on the host.
    pub fn with_upload_budget(mut self, budget: crate::budget::UploadBudget) -> Self {
        self.upload_budget = Some(budget);
        self
    }

    /// Wire a local [`ChunkCache`](crate::cache::ChunkCache) as a
    /// write-through tier: `put_chunk` will populate it after the durable
    /// blob write, so the producing host serves the chunk locally on the
    /// next read/resume. The host-agent passes the same cache its UFFD
    /// handler + disk daemon read from; the coordinator omits it. Cheap
    /// clone (the cache is `Arc`-backed).
    pub fn with_chunk_cache(mut self, cache: crate::cache::ChunkCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Replace the chunk-fetch resolver. The host-agent installs a
    /// `TieredChunkResolver` here for chunked-OCI images — it falls
    /// through `BlobStorage → OCI` with opportunistic write-through fill
    /// on miss. The global host `ChunkStore` keeps the default
    /// `BlobStorageResolver` (pre-ADR-0008 behavior).
    pub fn with_resolver(mut self, resolver: Arc<dyn ChunkResolver>) -> Self {
        self.resolver = resolver;
        self
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
    ///
    /// Keeps the `exists()` dedup HEAD: this is the entry point for
    /// **capture / enable-scale** uploads, where the HEAD is
    /// load-bearing dedup (a re-bake would otherwise re-upload tens of
    /// GiB). Flush paths that produce freshly re-chunked (by-construction
    /// new) data use [`Self::put_chunk_unchecked`] to skip the HEAD.
    ///
    /// Either way the local cache is warmed via [`Self::warm_local`] —
    /// including on the `exists()` short-circuit (ADR 0078 move 4): a
    /// chunk that is remotely present but locally evicted must still
    /// re-warm the producing host's cache, or every future resume on
    /// that host re-fetches from GCS what it already uploaded.
    pub async fn put_chunk(&self, body: &[u8]) -> Result<ChunkHash> {
        let hash = ChunkHash::of(body);
        let key = hash.storage_key();
        // Idempotency: if the chunk already exists, skip the PUT.
        // For S3/GCS this is a HEAD round-trip; for local fs it's
        // a stat. Both cheap relative to a multi-MB PUT.
        if self.inner.exists(&key).await? {
            // ADR 0078 move 4: warm the local cache even on the dedup
            // short-circuit. The chunk is durable in `inner`; if it is
            // remotely present but locally evicted, this is the only
            // thing that re-warms it on the producing host.
            self.warm_local(hash, body).await;
            metrics::counter!("engram_chunk_put_total", "mode" => "checked", "outcome" => "deduped")
                .increment(1);
            return Ok(hash);
        }
        let bytes = Bytes::copy_from_slice(body);
        {
            // Budget the PUT body only — the HEAD above and the local
            // warm below stay unbudgeted.
            let _permit = match &self.upload_budget {
                Some(b) => Some(b.acquire().await),
                None => None,
            };
            let _ = self.inner.put(&key, bytes).await?;
        }
        self.warm_local(hash, body).await;
        metrics::counter!("engram_chunk_put_total", "mode" => "checked", "outcome" => "uploaded")
            .increment(1);
        tracing::trace!(hash = %hash, bytes = body.len(), "chunk PUT");
        Ok(hash)
    }

    /// PUT a chunk WITHOUT the `exists()` dedup HEAD (ADR 0078 move 5).
    ///
    /// For the flush paths — the NBD disk flush and the teleport memory
    /// catch-up — whose input is freshly re-chunked dirty data that is
    /// **new by construction** (a content boundary shifted, so the hash
    /// changed). The HEAD there is pure waste: O(dirty) GCS round-trips
    /// per flush that always miss. Content-addressed PUTs are idempotent,
    /// so the rare identical-content revert re-uploads one 16 MiB chunk
    /// harmlessly. Unconditional PUT + unconditional local warm.
    ///
    /// Not for capture/enable uploads — see [`Self::put_chunk`], where
    /// the dedup HEAD saves re-uploading tens of GiB on a re-bake.
    pub async fn put_chunk_unchecked(&self, body: &[u8]) -> Result<ChunkHash> {
        let hash = ChunkHash::of(body);
        let key = hash.storage_key();
        let bytes = Bytes::copy_from_slice(body);
        {
            let _permit = match &self.upload_budget {
                Some(b) => Some(b.acquire().await),
                None => None,
            };
            let _ = self.inner.put(&key, bytes).await?;
        }
        self.warm_local(hash, body).await;
        metrics::counter!("engram_chunk_put_total", "mode" => "unchecked", "outcome" => "uploaded")
            .increment(1);
        tracing::trace!(hash = %hash, bytes = body.len(), "chunk PUT (unchecked)");
        Ok(hash)
    }

    /// Write-through the local cache (if wired): the host that produced
    /// this chunk should read it back locally (μs) rather than re-fetch
    /// it from the blob store (~110 ms) on the next resume — symmetric
    /// with the read path's populate-on-miss. `write_local` runs the
    /// cache's *debounced* budget sweep, so a burst of write-throughs
    /// (e.g. an idle-eviction re-chunk) enforces the free-space floor at
    /// most once per interval — it does NOT skip eviction the way
    /// `put_no_evict` would, which on a disk-pressured host could overshoot
    /// the floor. LRU then retains these fresh chunks and evicts stale
    /// ones, exactly the set we want warm for the resume. `hash` is
    /// pre-computed by the caller, so `write_local` skips a redundant
    /// re-hash. Best-effort: the chunk is already durable in `inner`, so
    /// a cache write failure is a missed optimization, never incorrect
    /// (reads fall back to the blob store).
    async fn warm_local(&self, hash: ChunkHash, body: &[u8]) {
        if let Some(cache) = &self.cache {
            if let Err(e) = cache.write_local(hash, body).await {
                tracing::debug!(
                    hash = %hash,
                    error = %e,
                    "chunk write-through to local cache failed (chunk durable in store; \
                     reads will fall back to the blob store)",
                );
            }
        }
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
    // tests drive a live system; wall clock/OS entropy here is input, not a
    // decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use crate::cache::{ChunkCache, ChunkCacheConfig};
    use crate::manifest::{ChunkRef, ManifestKind};
    use engram_storage_local::LocalBlobStorage;

    async fn store() -> (ChunkStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        (ChunkStore::new(blob), dir)
    }

    /// ADR 0088 addendum: the dedup short-circuit must not consume an
    /// upload-budget permit — a fully-deduped re-bake (the 40s prod
    /// case) must never queue behind other workloads' PUT traffic.
    #[tokio::test]
    async fn deduped_put_consumes_no_permit() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let budget = crate::budget::UploadBudget::new(1);
        let s = ChunkStore::new(blob).with_upload_budget(budget.clone());

        // First put uploads (consumes + releases the only permit).
        let body = b"budgeted-chunk";
        s.put_chunk(body).await.unwrap();

        // Hold the only permit hostage; the deduped re-put must still
        // return promptly via the exists() short-circuit.
        let _hostage = budget.acquire().await;
        tokio::time::timeout(std::time::Duration::from_secs(5), s.put_chunk(body))
            .await
            .expect("deduped put must not wait on the budget")
            .unwrap();
    }

    /// A store with a wired local write-through cache (the host-agent shape),
    /// returning a handle to the same cache so tests can assert residency.
    async fn store_with_cache() -> (ChunkStore, ChunkCache, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().join("blob")));
        // free_floor_pct = 0 → the write-through's debounced sweep never
        // evicts by disk pressure, so this test isolates "did put_chunk
        // populate the cache" without depending on the test host's free
        // space (eviction itself is covered by cache.rs's own tests).
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig::from_env_or_default(dir.path().join("cache")),
            0.0,
        );
        let store = ChunkStore::new(blob).with_chunk_cache(cache.clone());
        (store, cache, dir)
    }

    #[tokio::test]
    async fn put_chunk_write_throughs_to_wired_cache() {
        let (s, cache, _d) = store_with_cache().await;
        let body = b"a divergent memory page's bytes";
        let h = s.put_chunk(body).await.unwrap();
        // The producing host now serves this chunk locally on the next
        // resume instead of re-fetching it from the blob store.
        assert!(
            cache.contains_on_disk(h),
            "put_chunk must write-through to the wired local cache",
        );
    }

    #[tokio::test]
    async fn put_chunk_without_cache_stays_blob_only() {
        // The default store() (coordinator shape) has no wired cache —
        // put_chunk must still succeed and round-trip, just blob-only.
        let (s, _d) = store().await;
        let h = s.put_chunk(b"coord-side chunk").await.unwrap();
        assert_eq!(&s.get_chunk(h).await.unwrap()[..], b"coord-side chunk");
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
        let blob = s.blob_storage();
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
        let mut t = WorkingSetTrace::new(2, 5000, chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
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

    // ---- ADR 0078 Phase 1: write-through floor ----

    /// A `BlobStorage` that wraps a real local store and counts the
    /// `exists()` (HEAD) and `put()` (upload) calls the chunk-store
    /// makes, so tests can assert the flush-path HEAD is gone and the
    /// dedup HEAD is preserved.
    struct CountingBlob {
        inner: Arc<dyn BlobStorage>,
        exists_calls: std::sync::atomic::AtomicUsize,
        put_calls: std::sync::atomic::AtomicUsize,
    }
    impl CountingBlob {
        fn wrap(inner: Arc<dyn BlobStorage>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                exists_calls: std::sync::atomic::AtomicUsize::new(0),
                put_calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn exists_count(&self) -> usize {
            self.exists_calls.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn put_count(&self) -> usize {
            self.put_calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    #[async_trait::async_trait]
    impl BlobStorage for CountingBlob {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::traits::storage::ByteStream,
        ) -> std::result::Result<u64, engram_core::error::BlobError> {
            self.put_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> std::result::Result<
            engram_core::traits::storage::ByteStream,
            engram_core::error::BlobError,
        > {
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> std::result::Result<
            engram_core::traits::storage::BlobObjectMeta,
            engram_core::error::BlobError,
        > {
            self.exists_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.head(key).await
        }
        async fn delete(
            &self,
            key: &str,
        ) -> std::result::Result<(), engram_core::error::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(
            &self,
            prefix: &str,
        ) -> std::result::Result<Vec<String>, engram_core::error::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    /// A store over a `CountingBlob`, with a wired write-through cache.
    /// Returns the counting handle + cache so tests can assert both the
    /// blob-call counts and local residency.
    async fn counting_store_with_cache(
    ) -> (ChunkStore, Arc<CountingBlob>, ChunkCache, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().join("blob")));
        let counting = CountingBlob::wrap(inner);
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig::from_env_or_default(dir.path().join("cache")),
            0.0,
        );
        let store = ChunkStore::new(counting.clone()).with_chunk_cache(cache.clone());
        (store, counting, cache, dir)
    }

    /// ADR 0078 move 4: the `exists()` dedup short-circuit must STILL
    /// warm the local cache — a chunk that is remotely present but
    /// locally evicted is re-warmed by a put, closing the
    /// "write-through … failed"-adjacent re-miss class.
    #[tokio::test]
    async fn put_chunk_exists_arm_warms_local_cache() {
        let (s, _counting, cache, _d) = counting_store_with_cache().await;
        let body = b"a chunk that already lives in the blob store";
        // First put lands it in the blob store AND the cache.
        let h = s.put_chunk(body).await.unwrap();
        // Simulate a local eviction while the blob remains durable.
        cache.evict_on_disk_for_test(h);
        assert!(!cache.contains_on_disk(h), "precondition: locally evicted");
        // A second put hits the exists() short-circuit — but must
        // re-warm the local cache anyway.
        let h2 = s.put_chunk(body).await.unwrap();
        assert_eq!(h, h2);
        assert!(
            cache.contains_on_disk(h),
            "exists()-arm put must re-warm the locally-evicted chunk",
        );
    }

    /// ADR 0078 move 5: `put_chunk_unchecked` issues ZERO `exists()`
    /// HEADs, uploads unconditionally, and warms the local cache.
    #[tokio::test]
    async fn put_chunk_unchecked_skips_head_and_warms_cache() {
        let (s, counting, cache, _d) = counting_store_with_cache().await;
        let body = b"freshly re-chunked dirty flush data";
        let h = s.put_chunk_unchecked(body).await.unwrap();
        assert_eq!(counting.exists_count(), 0, "unchecked put issues no HEAD");
        assert_eq!(counting.put_count(), 1, "unchecked put uploads once");
        assert!(
            cache.contains_on_disk(h),
            "unchecked put must warm the local cache",
        );
        // Round-trips.
        assert_eq!(&s.get_chunk(h).await.unwrap()[..], body);
    }

    /// A flush of N dirty chunks via `put_chunk_unchecked` issues ZERO
    /// `exists()` HEADs (baseline was N HEADs) — the acceptance-criteria
    /// #2 property.
    #[tokio::test]
    async fn flush_of_n_dirty_chunks_issues_zero_heads() {
        let (s, counting, _cache, _d) = counting_store_with_cache().await;
        for i in 0..16u32 {
            let body = format!("dirty chunk {i}");
            s.put_chunk_unchecked(body.as_bytes()).await.unwrap();
        }
        assert_eq!(counting.exists_count(), 0, "no HEADs across the flush");
        assert_eq!(counting.put_count(), 16, "one PUT per dirty chunk");
    }

    /// The capture-path `put_chunk` STILL dedups via the `exists()`
    /// HEAD — a second put of identical bytes issues the HEAD and skips
    /// the upload (regression guard: move 5 must not change `put_chunk`).
    #[tokio::test]
    async fn put_chunk_still_dedups_on_second_identical_put() {
        let (s, counting, _cache, _d) = counting_store_with_cache().await;
        let body = b"capture-path chunk, deduped on re-put";
        s.put_chunk(body).await.unwrap();
        let after_first_puts = counting.put_count();
        s.put_chunk(body).await.unwrap();
        assert_eq!(
            counting.put_count(),
            after_first_puts,
            "second identical put_chunk must dedup (no re-upload)",
        );
        assert!(
            counting.exists_count() >= 2,
            "put_chunk keeps its dedup HEAD on every call",
        );
    }
}
