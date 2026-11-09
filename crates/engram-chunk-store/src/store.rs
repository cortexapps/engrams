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
//! The local NVMe cache (`cache::ChunkCache`), when wired in via
//! [`ChunkStore::with_chunk_cache`], is a **write-through** tier for
//! `put_chunk`: a chunk written durably to `BlobStorage` is also populated
//! into the local cache, so the host that *produced* it reads it back
//! locally (μs) instead of re-fetching it from the blob store (~110 ms) on
//! the next resume. The fill engine (`engram-fill`) owns the reads of the
//! cache; the store reads through the configured `ChunkResolver`, which by
//! default reads `BlobStorage` directly. On a host, the store carries the
//! fill engine's [`Admission`]: every chunk GET of the store and every
//! chunk PUT body takes a permit from it, in class order.

use std::sync::Arc;

use bytes::Bytes;
use engram_core::traits::BlobStorage;
use uuid::Uuid;

use crate::admission::{Admission, Class, Permit};
use crate::error::{ChunkStoreError, Result};
use crate::manifest::{ChunkHash, Manifest, ManifestRef};
use crate::resolver::{BlobStorageResolver, ChunkResolver};

/// How a chunk upload learns whether the store already holds the chunk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PutMode {
    /// A dedup HEAD first.
    Checked,
    /// No HEAD: the chunk is new by construction.
    Unchecked,
}

impl PutMode {
    fn label(self) -> &'static str {
        match self {
            Self::Checked => "checked",
            Self::Unchecked => "unchecked",
        }
    }
}

/// Front door to chunked-immutable storage. Cheap to clone (it's
/// `Arc`s internally); pass clones around freely.
#[derive(Clone)]
pub struct ChunkStore {
    /// Durable write target for chunks, manifests, and traces —
    /// `BlobStorage`. Always written (the durability tier).
    inner: Arc<dyn BlobStorage>,
    /// Read path for chunks the cache does not hold. A
    /// `BlobStorageResolver` over `inner` unless replaced with
    /// [`Self::with_resolver`].
    resolver: Arc<dyn ChunkResolver>,
    /// The local tier (host-agent). Every chunk upload writes it; the
    /// fill engine reads it. `None` on the coordinator.
    cache: Option<crate::cache::ChunkCache>,
    /// The host's transfer admission (the fill engine's). When set, every
    /// chunk PUT body and every chunk GET of the store takes a permit
    /// at `class`. `None` (coordinator, tests) admits everything.
    admission: Option<Arc<Admission>>,
    /// The class this store's transfers are admitted at.
    class: Class,
    /// Shared by every clone, so the uploads of one process to `inner`
    /// see each other.
    uploads: Arc<crate::uploads::Uploads>,
    /// The manifests this process read last. A manifest version is written
    /// once, so a hit is current. A PUT does not fill it: the existence
    /// check before a PUT is not atomic, and only a read shows what the
    /// store holds.
    manifests: Arc<ManifestCache>,
}

/// The most recent manifests, newest last.
#[derive(Default)]
struct ManifestCache(parking_lot::Mutex<std::collections::VecDeque<(ManifestRef, Arc<Manifest>)>>);

impl ManifestCache {
    const CAPACITY: usize = 16;

    fn get(&self, r: ManifestRef) -> Option<Arc<Manifest>> {
        let entries = self.0.lock();
        entries
            .iter()
            .find(|(k, _)| *k == r)
            .map(|(_, m)| m.clone())
    }

    fn insert(&self, r: ManifestRef, m: Arc<Manifest>) {
        let mut entries = self.0.lock();
        entries.retain(|(k, _)| *k != r);
        if entries.len() == Self::CAPACITY {
            entries.pop_front();
        }
        entries.push_back((r, m));
    }
}

impl ChunkStore {
    /// Wrap any `BlobStorage` impl. Production uses
    /// `engram-storage-gcs::GcsBlobStorage`; tests +
    /// `--mode=all` use `engram-storage-local::LocalBlobStorage`.
    ///
    /// Chunk reads default to `BlobStorageResolver` over the same
    /// blob.
    pub fn new(blob: Arc<dyn BlobStorage>) -> Self {
        let resolver: Arc<dyn ChunkResolver> = Arc::new(BlobStorageResolver::new(blob.clone()));
        Self {
            inner: blob,
            resolver,
            cache: None,
            admission: None,
            class: Class::Prefix,
            uploads: Arc::default(),
            manifests: Arc::default(),
        }
    }

    /// Admit chunk transfers through `admission`: each chunk PUT body
    /// (after the dedup HEAD) and each chunk GET of the store takes a
    /// permit. HEADs and manifest or trace transfers take none. The fill
    /// engine wires its own admission here, so the host's uploads and
    /// downloads share one limit.
    pub fn with_admission(mut self, admission: Arc<Admission>) -> Self {
        self.admission = Some(admission);
        self
    }

    /// A clone of this store whose transfers are admitted at `class`
    /// (default [`Class::Prefix`]: a session waits for them). Periodic
    /// checkpoints and image staging take [`Class::Background`].
    pub fn with_class(mut self, class: Class) -> Self {
        self.class = class;
        self
    }

    /// The class this store's transfers are admitted at.
    pub fn class(&self) -> Class {
        self.class
    }

    async fn admit(&self) -> Option<Permit> {
        match &self.admission {
            Some(a) => Some(a.acquire(self.class).await),
            None => None,
        }
    }

    /// Wire a local [`ChunkCache`](crate::cache::ChunkCache): every chunk
    /// upload also writes it, so a chunk this host produced is served
    /// locally from then on. The fill engine over this store reads it.
    pub fn with_chunk_cache(mut self, cache: crate::cache::ChunkCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Replace the chunk-fetch resolver. Production keeps the default
    /// `BlobStorageResolver`; tests install a resolver that counts or
    /// delays reads.
    pub fn with_resolver(mut self, resolver: Arc<dyn ChunkResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// The BlobStorage backing this store.
    pub fn blob_storage(&self) -> Arc<dyn BlobStorage> {
        Arc::clone(&self.inner)
    }

    /// The local cache tier, when one is wired.
    pub fn chunk_cache(&self) -> Option<&crate::cache::ChunkCache> {
        self.cache.as_ref()
    }

    /// The read path behind the cache.
    pub fn resolver(&self) -> &Arc<dyn ChunkResolver> {
        &self.resolver
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
        self.put_chunk_bytes(Bytes::copy_from_slice(body)).await
    }

    /// [`Self::put_chunk`] for bytes the caller owns: no copy.
    pub async fn put_chunk_bytes(&self, body: Bytes) -> Result<ChunkHash> {
        let hash = crate::cache::hash_chunk(&body).await?;
        self.upload(hash, body, PutMode::Checked).await?;
        Ok(hash)
    }

    /// PUT a chunk WITHOUT the `exists()` dedup HEAD (ADR 0078 move 5).
    ///
    /// For the sparse dirty-range re-chunk, whose input is freshly
    /// re-chunked dirty data that is
    /// **new by construction** (a content boundary shifted, so the hash
    /// changed). The HEAD there is pure waste: O(dirty) GCS round-trips
    /// per flush that always miss. Content-addressed PUTs are idempotent,
    /// so the rare identical-content revert re-uploads one 16 MiB chunk
    /// harmlessly. A PUT unless an overlapping upload of the chunk in
    /// this process just sent one (see [`Self::upload`]), then the local
    /// warm.
    ///
    /// Not for capture/enable uploads — see [`Self::put_chunk`], where
    /// the dedup HEAD saves re-uploading tens of GiB on a re-bake.
    pub async fn put_chunk_unchecked(&self, body: &[u8]) -> Result<ChunkHash> {
        let body = Bytes::copy_from_slice(body);
        let hash = crate::cache::hash_chunk(&body).await?;
        self.put_chunk_unchecked_prehashed(hash, body).await?;
        Ok(hash)
    }

    /// [`Self::put_chunk_unchecked`] for a caller that already hashed
    /// `body` (the sparse re-chunk hashes first to decide its
    /// unchanged-skip). The caller guarantees `hash` is its hash.
    pub(crate) async fn put_chunk_unchecked_prehashed(
        &self,
        hash: ChunkHash,
        body: Bytes,
    ) -> Result<()> {
        self.upload(hash, body, PutMode::Unchecked).await
    }

    /// Make chunk `hash` durable, then warm the local cache with it. In
    /// one process, one upload of a chunk runs at a time: concurrent PUTs
    /// of one object hit the store's per-object write limit. An upload
    /// that waited while another one's PUT succeeded sends nothing (see
    /// [`crate::uploads`]); after a HEAD hit or a failure, each waiter
    /// asks the store itself.
    async fn upload(&self, hash: ChunkHash, body: Bytes, mode: PutMode) -> Result<()> {
        let mut flight = self.uploads.one_at_a_time(hash).await;
        let key = hash.storage_key();
        let outcome = if flight.put_landed() {
            "joined"
        } else if mode == PutMode::Checked && self.inner.exists(&key).await? {
            "deduped"
        } else if self
            .put_body(hash, &key, body.clone(), mode.label())
            .await?
        {
            flight.landed();
            "uploaded"
        } else {
            "rescued_existing"
        };
        drop(flight);
        if outcome == "joined" || outcome == "deduped" {
            metrics::counter!(
                "engram_chunk_put_total",
                "mode" => mode.label(), "outcome" => outcome
            )
            .increment(1);
        }
        self.warm_local(hash, body).await;
        Ok(())
    }

    /// One chunk body PUT, under one admission permit. `true` when the
    /// PUT succeeded; `false` when it failed but the object exists (see
    /// [`Self::resolve_failed_put`]).
    async fn put_body(
        &self,
        hash: ChunkHash,
        key: &str,
        body: Bytes,
        mode: &'static str,
    ) -> Result<bool> {
        let len = body.len() as u64;
        let permit = self.admit().await;
        let put = self.inner.put(key, body).await;
        match put {
            Ok(_) => {
                if let Some(p) = permit {
                    p.done(len);
                }
                metrics::counter!("engram_chunk_put_total", "mode" => mode, "outcome" => "uploaded")
                    .increment(1);
                tracing::trace!(hash = %hash, bytes = len, mode, "chunk PUT");
                Ok(true)
            }
            Err(e) => {
                let overload = crate::admission::is_overload(&e);
                let resolved = self.resolve_failed_put(hash, key, mode, e).await;
                // A PUT that another writer's object rescues moved no
                // bytes but is no sign of overload.
                if let (Some(p), Err(_), true) = (permit, &resolved, overload) {
                    p.failed();
                }
                resolved.map(|()| false)
            }
        }
    }

    /// Last resort for a failed chunk PUT: chunk keys are
    /// content-addressed and write-once, so "the object exists" IS
    /// success — some writer (a concurrent duplicate in this process,
    /// another host, an earlier attempt whose response was lost)
    /// landed these exact bytes. The 2026-08-04 chain-poison alert was
    /// this shape: concurrent same-content PUTs tripped GCS's
    /// per-object mutation-rate cap (~1/s) and the retry ladder's
    /// backoff sat entirely inside the window — while the object was
    /// durable the whole time. One HEAD, spent only on the
    /// already-failed path; if the HEAD fails or the object is absent,
    /// the original put error surfaces.
    async fn resolve_failed_put(
        &self,
        hash: ChunkHash,
        key: &str,
        mode: &'static str,
        err: engram_core::error::BlobError,
    ) -> Result<()> {
        match self.inner.exists(key).await {
            Ok(true) => {
                metrics::counter!(
                    "engram_chunk_put_total",
                    "mode" => mode, "outcome" => "rescued_existing"
                )
                .increment(1);
                tracing::warn!(
                    hash = %hash,
                    error = %err,
                    "chunk PUT failed but the object exists — write-once \
                     content-addressed key, so a concurrent writer already \
                     landed these bytes; treating the put as success",
                );
                Ok(())
            }
            _ => Err(err.into()),
        }
    }

    /// Write-through the local cache (if wired): the host that produced
    /// this chunk reads it back locally on the next resume instead of
    /// fetching it from the blob store. `put_trusted` runs the cache's
    /// debounced budget sweep, so a burst of write-throughs enforces the
    /// free-space floor at most once per interval. Best-effort: the chunk
    /// is already durable in `inner`, so a failed cache write only costs
    /// a later fetch.
    async fn warm_local(&self, hash: ChunkHash, body: Bytes) {
        if let Some(cache) = &self.cache {
            if let Err(e) = cache.put_trusted(hash, body, self.class).await {
                tracing::debug!(
                    hash = %hash,
                    error = %e,
                    "chunk write-through to local cache failed (chunk durable in store; \
                     reads will fall back to the blob store)",
                );
            }
        }
    }

    /// A chunk's bytes from the resolver, hash-checked, under one
    /// admission permit. It neither reads nor writes the local cache, and
    /// has no hedge and no peer: on a host, the fill engine does all
    /// three. Callers outside this crate read through a
    /// [`crate::ChunkSource`]: the fill engine on a host, the store's own
    /// impl in a process without one.
    pub(crate) async fn get_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        let permit = self.admit().await;
        let got = self.resolver.fetch_chunk(hash).await;
        if let Some(p) = permit {
            p.finish(got.as_ref().map(|b| b.len() as u64));
        }
        got
    }

    /// Existence check for a chunk, routed through the resolver.
    /// With the default `BlobStorageResolver` this is a
    /// `BlobStorage::exists` call.
    pub async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        self.resolver.chunk_exists(hash).await
    }

    /// Whether the blob store holds every chunk `manifest` lists. A HEAD
    /// per chunk, 32 at a time, stopping at the first miss. A local
    /// cache or a peer does not count: other hosts read from the blob
    /// store.
    pub async fn all_chunks_stored(&self, manifest: &Manifest) -> Result<bool> {
        use futures::stream::{self, StreamExt as _, TryStreamExt as _};
        let keys: Vec<String> = manifest
            .chunks
            .iter()
            .map(|c| c.hash.storage_key())
            .collect();
        let mut checks = stream::iter(keys.into_iter().map(|key| {
            let blob = self.inner.clone();
            async move { blob.exists(&key).await }
        }))
        .buffer_unordered(32);
        while let Some(present) = checks.try_next().await? {
            if !present {
                return Ok(false);
            }
        }
        Ok(true)
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

    /// GET a specific manifest version, from this process's recent
    /// manifests when it holds it. Returns `NotFound` if no such version
    /// was ever written, or if it was GC'd and is not held here.
    pub async fn get_manifest(&self, r: ManifestRef) -> Result<Manifest> {
        if let Some(m) = self.manifests.get(r) {
            return Ok((*m).clone());
        }
        let bytes = self.inner.get(&r.storage_key()).await?;
        let m: Manifest = serde_json::from_slice(&bytes)?;
        m.validate()?;
        self.manifests.insert(r, Arc::new(m.clone()));
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
        let new_ref = ManifestRef::new();
        self.put_manifest(new_ref, &m).await?;
        Ok(new_ref)
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

    /// The dedup short-circuit takes no permit: a fully deduped re-bake
    /// never queues behind other transfers.
    #[tokio::test]
    async fn deduped_put_consumes_no_permit() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let admission = Admission::fixed("test", 1, 0);
        let s = ChunkStore::new(blob).with_admission(admission.clone());

        let body = b"admitted-chunk";
        s.put_chunk(body).await.unwrap();
        assert_eq!(admission.in_flight(), 0, "the upload released its permit");

        let _hostage = admission.acquire(Class::Demand).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), s.put_chunk(body))
            .await
            .expect("deduped put must not wait for a permit")
            .unwrap();
    }

    /// A Background upload waits while a Prefix transfer waits for the
    /// same admission: uploads and downloads share one class order.
    #[tokio::test]
    async fn a_background_upload_waits_behind_a_waiting_prefix_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let admission = Admission::fixed("test", 1, 0);
        let store = ChunkStore::new(blob).with_admission(admission.clone());
        let held = admission.acquire(Class::Hydrate).await;

        let background = store.clone().with_class(Class::Background);
        let upload = tokio::spawn(async move { background.put_chunk(b"bulk").await });
        tokio::task::yield_now().await;
        let prefix = tokio::spawn({
            let admission = admission.clone();
            async move { admission.acquire(Class::Prefix).await }
        });
        tokio::task::yield_now().await;
        drop(held);

        let prefix = tokio::time::timeout(std::time::Duration::from_secs(5), prefix)
            .await
            .expect("prefix granted")
            .unwrap();
        assert!(
            !upload.is_finished(),
            "the upload waits for the prefix slot"
        );
        drop(prefix);
        upload.await.unwrap().unwrap();
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

    // ---- ChunkResolver indirection ----

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
            async fn fetch_chunk_unverified(&self, _hash: ChunkHash) -> Result<Bytes> {
                self.calls.fetch_add(1, Ordering::Relaxed);
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

    // The counting `BlobStorage` double lives in `crate::test_support`
    // (shared with `file.rs`'s sparse re-chunk tests since ADR 0101 A).
    use crate::test_support::CountingBlob;

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

    /// The capture-path `put_chunk` dedups with the `exists()` HEAD on
    /// every call: a chunk this process uploaded earlier is asked for
    /// again, because chunk GC can have deleted it since.
    #[tokio::test]
    async fn put_chunk_still_dedups_on_second_identical_put() {
        let (s, counting, _cache, _d) = counting_store_with_cache().await;
        let body = b"capture-path chunk, deduped on re-put";
        s.put_chunk(body).await.unwrap();
        assert_eq!((counting.exists_count(), counting.put_count()), (1, 1));

        s.put_chunk(body).await.unwrap();
        assert_eq!(
            (counting.exists_count(), counting.put_count()),
            (2, 1),
            "a later put asks the store and skips the upload"
        );
    }

    /// A later unchecked put of a chunk sends its PUT again: it re-creates
    /// an object GC can have deleted since.
    #[tokio::test]
    async fn a_later_unchecked_put_sends_its_own_put() {
        let (s, counting, _cache, _d) = counting_store_with_cache().await;
        s.put_chunk_unchecked(b"flushed twice").await.unwrap();
        s.put_chunk_unchecked(b"flushed twice").await.unwrap();
        assert_eq!(counting.put_count(), 2);
    }

    /// A blob store whose PUTs wait for the test to open a gate.
    struct GatedPuts {
        inner: Arc<CountingBlob>,
        gate: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl BlobStorage for GatedPuts {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::traits::storage::ByteStream,
        ) -> std::result::Result<u64, engram_core::error::BlobError> {
            drop(self.gate.acquire().await.expect("never closed"));
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

    /// Uploads of one chunk that overlap one PUT send no PUT of their
    /// own: the store limits writes to one object to about one per second.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_of_one_chunk_send_one_put() {
        for unchecked in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let local: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path()));
            let counting = CountingBlob::wrap(local);
            let gated = Arc::new(GatedPuts {
                inner: counting.clone(),
                gate: tokio::sync::Semaphore::new(0),
            });
            let s = ChunkStore::new(gated.clone());
            let body = Bytes::from_static(b"the same chunk from many writers");
            let mut puts = tokio::task::JoinSet::new();
            for _ in 0..16 {
                let (s, body) = (s.clone(), body.clone());
                puts.spawn(async move {
                    if unchecked {
                        s.put_chunk_unchecked(&body).await
                    } else {
                        s.put_chunk_bytes(body).await
                    }
                });
            }
            // Every upload is now waiting: one for the gate, the others
            // for the first.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            gated.gate.add_permits(16);
            while let Some(done) = puts.join_next().await {
                assert_eq!(done.unwrap().unwrap(), ChunkHash::of(&body));
            }
            assert_eq!(counting.put_count(), 1, "unchecked={unchecked}");
            assert_eq!(counting.exists_count(), usize::from(!unchecked));
        }
    }

    /// A failed upload serves no one: the next upload of the chunk sends
    /// its own PUT.
    #[tokio::test]
    async fn an_upload_after_a_failed_one_sends_its_own_put() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let plan = FaultPlan::new().fail_nth_put(1, InjectedError::Sdk("rate limit".into()));
        let (faulty, counters) = FaultyBlobStorage::arc(inner, plan);
        let s = ChunkStore::new(faulty);
        let body = b"the first PUT fails";

        assert!(s.put_chunk_unchecked(body).await.is_err());
        s.put_chunk_unchecked(body).await.unwrap();

        assert_eq!(counters.puts_attempted(), 2);
        assert_eq!(&s.get_chunk(ChunkHash::of(body)).await.unwrap()[..], body);
    }

    // ---- 2026-08-04 chain-poison alert: failed-PUT exists() resolve ----

    use engram_testkit::storage::{
        FaultPlan, FaultyBlobStorage, InjectedError, KeyMatch, PutFault, PutFaultKind, When,
    };

    fn always_fail_puts() -> FaultPlan {
        FaultPlan::new().with_put(PutFault {
            key: KeyMatch::Any,
            when: When::Always,
            kind: PutFaultKind::FailBeforeWrite(InjectedError::Sdk(
                "simulated per-object mutation rate limit (gcs429)".into(),
            )),
        })
    }

    /// A PUT that fails while the object is already durable (a
    /// concurrent same-content writer landed it — the GCS per-object
    /// mutation-429 shape) resolves as success: chunk keys are
    /// write-once, so existence IS success.
    #[tokio::test]
    async fn failed_put_resolves_as_success_when_object_exists() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let body = b"landed by a concurrent duplicate writer";
        let hash = ChunkHash::of(body);
        // The "concurrent writer": the object is durable before our put.
        inner
            .put(&hash.storage_key(), Bytes::copy_from_slice(body))
            .await
            .unwrap();
        let (faulty, _counters) = FaultyBlobStorage::arc(inner, always_fail_puts());
        let s = ChunkStore::new(faulty);
        let h = s
            .put_chunk_unchecked(body)
            .await
            .expect("existing object resolves the failed put");
        assert_eq!(h, hash);
        assert_eq!(&s.get_chunk(h).await.unwrap()[..], body);
    }

    /// When the object is genuinely absent, the original put error
    /// surfaces — the resolve is strictly "existence proves success",
    /// never "swallow the failure".
    #[tokio::test]
    async fn failed_put_with_absent_object_surfaces_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let (faulty, _counters) = FaultyBlobStorage::arc(inner, always_fail_puts());
        let s = ChunkStore::new(faulty);
        let err = s
            .put_chunk_unchecked(b"never landed anywhere")
            .await
            .expect_err("no durable object to resolve against");
        assert!(
            matches!(err, ChunkStoreError::Blob(_)),
            "the original blob error surfaces, got {err:?}",
        );
    }
}
