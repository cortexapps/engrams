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
//! consumed by adapters; this module's GETs go directly to
//! `BlobStorage`. Use the cache for hot-path read patterns.

use std::sync::Arc;

use bytes::Bytes;
use engram_core::traits::BlobStorage;
use uuid::Uuid;

use crate::error::{ChunkStoreError, Result};
use crate::manifest::{ChunkHash, Manifest, ManifestRef};
use crate::working_set::{TraceRef, WorkingSetTrace};

/// Front door to chunked-immutable storage. Cheap to clone (it's
/// an `Arc` internally); pass clones around freely.
#[derive(Clone)]
pub struct ChunkStore {
    inner: Arc<dyn BlobStorage>,
}

impl ChunkStore {
    /// Wrap any `BlobStorage` impl. Production uses
    /// `engram-storage-gcs::GcsBlobStorage`; tests +
    /// `--mode=all` use `engram-storage-local::LocalBlobStorage`.
    pub fn new(blob: Arc<dyn BlobStorage>) -> Self {
        Self { inner: blob }
    }

    /// Borrow the underlying blob storage. Crate-internal: GC and
    /// corruption tests need to bypass the typed surface for raw
    /// `list_prefix` / `head` / `delete`. Production callers go
    /// through `put_chunk`, `get_manifest`, etc.
    pub(crate) fn blob(&self) -> &Arc<dyn BlobStorage> {
        &self.inner
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

    /// GET a chunk's bytes. Verifies the returned content hashes
    /// to the requested hash — catches storage-layer corruption.
    pub async fn get_chunk(&self, hash: ChunkHash) -> Result<Bytes> {
        let key = hash.storage_key();
        let bytes = self.inner.get(&key).await?;
        let actual = ChunkHash::of(&bytes);
        if actual != hash {
            return Err(ChunkStoreError::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(bytes)
    }

    /// Existence check for a chunk. Useful for caching decisions.
    pub async fn chunk_exists(&self, hash: ChunkHash) -> Result<bool> {
        Ok(self.inner.exists(&hash.storage_key()).await?)
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
}
