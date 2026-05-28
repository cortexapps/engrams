//! Content-addressable cache for OCI-pulled bake images and harness
//! packs.
//!
//! # Layout
//!
//! ```text
//! <cache_dir>/
//!   images/
//!     by-uri.json                      # uri -> digest mapping (mtime-bumped on hit)
//!     sha256/
//!       <digest>/
//!         manifest.toml
//!         rootfs.ext4
//!   harness/
//!     by-uri.json
//!     sha256/
//!       <digest>/
//!         harness                       # entry-point + sidecars
//!         <sidecar files...>
//! ```
//!
//! # Cache hits
//!
//! `ensure_image(uri)` first looks up `uri` in the by-uri map. If
//! present and the digest's directory still exists on disk, we touch
//! its mtime (for LRU GC) and return its path. On miss we pull via
//! [`engram_oci::OciClient`], write to the digest dir, and update
//! the map.
//!
//! # GC
//!
//! `gc(disk_budget_bytes)` walks `sha256/`, sorts by mtime ascending,
//! and removes oldest entries until the total size is under budget.
//! Active sessions don't pin entries explicitly today — the touch on
//! every `ensure_*` call keeps in-use digests at the front of the
//! LRU list. A more rigorous refcount lands when GC starts evicting
//! live images.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_oci::{OciClient, OciError};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Clone)]
pub struct ImageCache {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    oci: OciClient,
    /// In-memory uri→digest cache. Persisted as `by-uri.json` so a
    /// host-agent restart skips re-pulls of recent tags.
    image_map: Mutex<UriMap>,
    harness_map: Mutex<UriMap>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct UriMap {
    /// Map of URI string to its OCI manifest digest, e.g.
    /// `gcr.io/cortex/api:warm-X` -> `sha256:abcd...`.
    entries: std::collections::HashMap<String, String>,
}

impl ImageCache {
    /// Build a cache rooted at `root`. Loads any persisted `by-uri.json`
    /// maps from previous runs (best-effort; corruption logs and
    /// reverts to an empty map).
    pub async fn open(root: PathBuf, oci: OciClient) -> Result<Self, CacheError> {
        fs::create_dir_all(root.join("images/sha256"))
            .await
            .map_err(CacheError::Io)?;
        fs::create_dir_all(root.join("harness/sha256"))
            .await
            .map_err(CacheError::Io)?;
        let image_map = load_uri_map(&root.join("images/by-uri.json")).await;
        let harness_map = load_uri_map(&root.join("harness/by-uri.json")).await;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                oci,
                image_map: Mutex::new(image_map),
                harness_map: Mutex::new(harness_map),
            }),
        })
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Clone of the inner `OciClient`. Useful for callers that need
    /// to reuse the same auth resolver for additional OCI verbs
    /// outside the cache's pull surface — e.g. ADR 0008 Phase 5's
    /// `OciChunkResolver`, which does Range GETs against the
    /// registry for chunk fetches. The client is cheap to clone
    /// (its internals are `Arc`s).
    pub fn oci_client(&self) -> OciClient {
        self.inner.oci.clone()
    }

    /// Ensure the bake image at `uri` is materialised in the cache.
    /// On a hit, returns the cached paths without a network round
    /// trip. On a miss, pulls via OCI and writes the layers the
    /// artifact actually carries.
    ///
    /// ADR 0007 Phase 6: `rootfs.ext4` is optional in the cache —
    /// chunked-storage pushes only ship `bundle.json` + `manifest.toml`,
    /// and disk bytes are resolved on demand through the chunk store.
    pub async fn ensure_image(&self, uri: &str) -> Result<CachedImage, CacheError> {
        // Cache hit fast-path: known URI + digest dir still exists.
        if let Some(digest) = self.lookup(&self.inner.image_map, uri) {
            let dir = self.image_dir(&digest);
            let manifest_path = dir.join("manifest.toml");
            let rootfs_path = dir.join("rootfs.ext4");
            let bundle_path = dir.join("bundle.json");
            if fs::try_exists(&manifest_path).await.unwrap_or(false) {
                let has_rootfs = fs::try_exists(&rootfs_path).await.unwrap_or(false);
                let has_bundle = fs::try_exists(&bundle_path).await.unwrap_or(false);
                // Cache entry is consumable iff at least one disk
                // source is present (rootfs file OR bundle pointing
                // at chunks).
                if has_rootfs || has_bundle {
                    touch(&dir).await;
                    let bundle = if has_bundle {
                        read_bundle(&bundle_path).await?
                    } else {
                        None
                    };
                    // ADR 0008 Phase 3: discover Nydus-shaped
                    // sidecars from the cache directory. The pull
                    // path writes them out; cache hits just
                    // re-probe their presence. Blob digests are
                    // stored in `chunks_disk.blob.digest` /
                    // `chunks_memory.blob.digest` (sidecar files
                    // written at pull time — see below).
                    let disk_bootstrap_path = optional_path(&dir.join("bootstrap.disk.json")).await;
                    let disk_chunks_blob_digest =
                        read_digest_sidecar(&dir.join("chunks.disk.blob.digest")).await;
                    return Ok(CachedImage {
                        manifest_path,
                        rootfs_path: has_rootfs.then_some(rootfs_path),
                        bundle,
                        disk_bootstrap_path,
                        disk_chunks_blob_digest,
                        digest,
                    });
                }
            }
            // Stale map entry: the dir was GC'd. Fall through to re-pull.
            tracing::debug!(uri = %uri, digest = %digest, "cache map references missing dir; re-pulling");
        }

        // Miss path: pull to a temp dir, then atomically rename into
        // sha256/<digest>/. This keeps half-pulled state out of the
        // cache when the process is killed mid-fetch.
        let tmp = self.inner.root.join("images/.tmp");
        // Use a unique tmp subdir per pull to avoid concurrent collisions.
        let pull_dir = tmp.join(format!("pull-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&pull_dir)
            .await
            .map_err(CacheError::Io)?;
        let pulled = self
            .inner
            .oci
            .pull_image(uri, &pull_dir)
            .await
            .map_err(CacheError::Oci)?;
        let digest = pulled.manifest_digest.as_str().to_string();
        let final_dir = self.image_dir(&digest);
        // If a concurrent puller landed first, drop ours rather than overwrite.
        if fs::try_exists(&final_dir).await.unwrap_or(false) {
            let _ = fs::remove_dir_all(&pull_dir).await;
        } else {
            if let Some(parent) = final_dir.parent() {
                fs::create_dir_all(parent).await.map_err(CacheError::Io)?;
            }
            fs::rename(&pull_dir, &final_dir)
                .await
                .map_err(CacheError::Io)?;
        }
        self.update_map(&self.inner.image_map, uri, &digest).await;

        let bundle = read_bundle(&final_dir.join("bundle.json")).await?;
        let rootfs_path = final_dir.join("rootfs.ext4");
        let rootfs_present = fs::try_exists(&rootfs_path).await.unwrap_or(false);

        // ADR 0008 Phase 3: PulledImage carries the bootstrap layer
        // paths and chunk-blob digests for Nydus-shaped artifacts.
        // The bootstrap files are already on disk (`pull_image`
        // writes them); we additionally persist the blob digests as
        // small sidecar files so cache-hit reads can recover them
        // without re-pulling.
        let disk_bootstrap_path = pulled
            .disk_bootstrap_path
            .as_ref()
            .map(|_| final_dir.join("bootstrap.disk.json"))
            .filter(|p| p.exists());
        let disk_chunks_blob_digest = pulled.disk_chunks_blob_digest.clone();
        if let Some(d) = &disk_chunks_blob_digest {
            fs::write(final_dir.join("chunks.disk.blob.digest"), d.as_bytes())
                .await
                .map_err(CacheError::Io)?;
        }
        Ok(CachedImage {
            manifest_path: final_dir.join("manifest.toml"),
            rootfs_path: rootfs_present.then_some(rootfs_path),
            bundle,
            disk_bootstrap_path,
            disk_chunks_blob_digest,
            digest,
        })
    }
    // ADR 0021 P1.5: `ensure_harness` + `ensure_harness_ext4` retired with the substrate.

    /// Sweep the cache and remove oldest entries until the total
    /// on-disk size of `images/sha256` + `harness/sha256` is below
    /// `budget_bytes`. Returns the number of bytes freed.
    ///
    /// Best-effort: skips entries it can't measure or remove
    /// (logged). Idempotent — running twice in a row is a no-op
    /// after the first pass converges under budget.
    pub async fn gc(&self, budget_bytes: u64) -> Result<u64, CacheError> {
        let mut entries = self.cache_entries().await?;
        let total: u64 = entries.iter().map(|e| e.size_bytes).sum();
        if total <= budget_bytes {
            return Ok(0);
        }
        // Oldest first.
        entries.sort_by_key(|e| e.mtime);
        let mut over = total - budget_bytes;
        let mut freed = 0u64;
        for entry in entries {
            if over == 0 {
                break;
            }
            tracing::info!(
                path = %entry.path.display(),
                size = entry.size_bytes,
                "GC: evicting cache entry"
            );
            if let Err(e) = fs::remove_dir_all(&entry.path).await {
                tracing::warn!(path = %entry.path.display(), error = %e, "GC: remove failed; skipping");
                continue;
            }
            over = over.saturating_sub(entry.size_bytes);
            freed += entry.size_bytes;
        }
        // Drop the in-memory map entries that point at gone digests.
        // (Cheaper to just clear and let the next `ensure_*` rebuild
        // — `update_map` rewrites on each insert.)
        self.inner.image_map.lock().entries.clear();
        self.inner.harness_map.lock().entries.clear();
        let _ = persist_uri_map(
            &self.inner.root.join("images/by-uri.json"),
            &self.inner.image_map.lock(),
        );
        let _ = persist_uri_map(
            &self.inner.root.join("harness/by-uri.json"),
            &self.inner.harness_map.lock(),
        );
        Ok(freed)
    }

    fn image_dir(&self, digest: &str) -> PathBuf {
        self.inner
            .root
            .join("images/sha256")
            .join(strip_sha256_prefix(digest))
    }

    fn lookup(&self, map: &Mutex<UriMap>, uri: &str) -> Option<String> {
        map.lock().entries.get(uri).cloned()
    }

    /// ADR 0015 M5: drop the `uri → digest` mapping so the next
    /// `ensure_image(uri)` call re-pulls from the registry. Used by
    /// the prefetch supervisor when the cached digest doesn't match
    /// the digest the coord advertised in `enabled_images` — the
    /// registry has rotated under the same tag (a common pattern
    /// for `:warm-<sha>` re-pushes). The on-disk artifacts at
    /// `images/sha256/<old>/` are left alone — touch-based LRU
    /// handles eventual eviction; an immediate delete would race
    /// any in-flight session still consuming the old digest.
    pub async fn invalidate_uri(&self, uri: &str) {
        let path = self.inner.root.join("images/by-uri.json");
        let snapshot = {
            let mut m = self.inner.image_map.lock();
            m.entries.remove(uri);
            m.clone()
        };
        let _ = persist_uri_map(&path, &snapshot);
    }

    /// Pre-plant a `(uri, digest)` association in the image map.
    /// Test-only — lets cross-module tests (e.g. pooled_backend's
    /// chunked-lifecycle test) drive the cache hit path without
    /// standing up a real OCI registry.
    #[cfg(test)]
    pub(crate) fn prime_image_for_test(&self, uri: &str, digest: &str) {
        self.inner
            .image_map
            .lock()
            .entries
            .insert(uri.to_string(), digest.to_string());
    }

    /// Where the cached artifacts for `digest` land on disk.
    /// Test-only — keeps the layout details inside this module.
    #[cfg(test)]
    pub(crate) fn image_dir_for_test(&self, digest: &str) -> PathBuf {
        self.image_dir(digest)
    }

    async fn update_map(&self, map: &Mutex<UriMap>, uri: &str, digest: &str) {
        let path = if std::ptr::eq(map, &self.inner.image_map) {
            self.inner.root.join("images/by-uri.json")
        } else {
            self.inner.root.join("harness/by-uri.json")
        };
        let snapshot = {
            let mut m = map.lock();
            m.entries.insert(uri.to_string(), digest.to_string());
            UriMap {
                entries: m.entries.clone(),
            }
        };
        if let Err(e) = persist_uri_map(&path, &snapshot) {
            tracing::warn!(path = %path.display(), error = %e, "uri-map persist failed");
        }
    }

    async fn cache_entries(&self) -> Result<Vec<GcCandidate>, CacheError> {
        let mut out = Vec::new();
        for sub in ["images/sha256", "harness/sha256"] {
            let root = self.inner.root.join(sub);
            let mut rd = match fs::read_dir(&root).await {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            while let Some(entry) = rd.next_entry().await.map_err(CacheError::Io)? {
                let path = entry.path();
                let meta = match entry.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if !meta.is_dir() {
                    continue;
                }
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size_bytes = dir_size(&path).await.unwrap_or(0);
                out.push(GcCandidate {
                    path,
                    mtime,
                    size_bytes,
                });
            }
        }
        Ok(out)
    }
}

impl CachedImage {
    /// Whether this cached image carries Nydus-shaped chunked OCI
    /// layers — i.e., disk chunks are reachable via Range GET into
    /// the OCI registry, not just via BlobStorage.
    ///
    /// True iff both the disk bootstrap sidecar and the chunk-blob
    /// layer digest are present. Phase 5 of ADR 0008's rollout uses
    /// this to dispatch between the legacy BlobStorage-only path
    /// and the new tiered fault path.
    pub fn is_disk_chunked_oci(&self) -> bool {
        self.disk_bootstrap_path.is_some() && self.disk_chunks_blob_digest.is_some()
    }

    /// Parse the bootstrap sidecar and build an
    /// [`engram_oci::OciChunkIndex`] suitable for constructing an
    /// [`engram_oci::OciChunkResolver`].
    ///
    /// Returns `Ok(None)` if the image isn't chunked-OCI shaped;
    /// otherwise reads the bootstrap and populates the index with
    /// one entry per chunk. `blob_digest = None` entries (chunks in
    /// this image's primary blob) resolve to
    /// `disk_chunks_blob_digest`; `Some(digest)` entries (Phase 4
    /// base/diff inheritance) keep their cross-blob references.
    ///
    /// ADR 0008 Phase 5 wiring: a future `PooledBackend::create`
    /// pass will call this on a chunked image, wrap the result in a
    /// `TieredChunkResolver` alongside a `BlobStorageResolver`, and
    /// install the tiered resolver onto a per-sandbox
    /// `ChunkStore::with_resolver` for chunk reads. The legacy
    /// BlobStorage-only path remains for non-chunked images.
    pub async fn build_oci_chunk_index(
        &self,
    ) -> Result<Option<engram_oci::OciChunkIndex>, CacheError> {
        let (Some(bs_path), Some(primary_blob_digest)) = (
            self.disk_bootstrap_path.as_ref(),
            self.disk_chunks_blob_digest.as_ref(),
        ) else {
            return Ok(None);
        };
        let bytes = fs::read(bs_path).await.map_err(CacheError::Io)?;
        let bs: engram_chunk_store::Bootstrap = serde_json::from_slice(&bytes).map_err(|e| {
            CacheError::Bundle(format!("{}: bootstrap parse: {e}", bs_path.display()))
        })?;
        let mut index = engram_oci::OciChunkIndex::new();
        for entry in &bs.entries {
            // Resolve `blob_digest = None` (self-blob) to this
            // image's primary chunks-blob layer digest. Phase 4
            // entries with `Some(parent_digest)` keep their
            // cross-blob reference unchanged.
            let blob_digest = entry
                .blob_digest
                .clone()
                .unwrap_or_else(|| primary_blob_digest.clone());
            index.insert(
                entry.sha256,
                engram_oci::OciBlobLocator {
                    blob_digest,
                    offset: entry.blob_offset,
                    length: entry.length as u64,
                },
            );
        }
        Ok(Some(index))
    }
}

#[derive(Clone, Debug)]
pub struct CachedImage {
    pub manifest_path: PathBuf,
    /// ADR 0007 Phase 6: `None` when the image artifact carried only
    /// the `bundle.json` (chunked-storage push path skips the
    /// `rootfs.ext4` layer to avoid duplicating chunks). Consumers
    /// that need raw rootfs bytes must read via `bundle.disk_manifest`
    /// through the chunk store. `Some(path)` for legacy artifacts
    /// (no bundle present).
    pub rootfs_path: Option<PathBuf>,
    /// ADR 0007: parsed bundle.json content, when the image carries
    /// one. Phase 4/5 wire this into FC's NBD disk + UFFD memory
    /// adapters; today it's plumbed through so adopters can find
    /// the chunk-store reference without re-reading the file.
    pub bundle: Option<ImageBundle>,
    /// ADR 0008 Phase 3: path to the disk-side bootstrap layer when
    /// the artifact is Nydus-shaped. `Some` enables the
    /// `TieredChunkResolver` (cache → OCI) fault path; `None` means
    /// the artifact predates ADR 0008 and consumers fall back to
    /// BlobStorage-only resolution via `bundle.disk_manifest`.
    pub disk_bootstrap_path: Option<PathBuf>,
    /// ADR 0008 Phase 3: OCI layer digest of the disk chunk blob.
    /// Paired with `disk_bootstrap_path`; required to Range-GET
    /// chunks. Not the per-chunk sha256 — that's per-entry in the
    /// bootstrap.
    pub disk_chunks_blob_digest: Option<String>,
    pub digest: String,
}

/// Parsed contents of the `bundle.json` sidecar shipped with an OCI
/// image artifact.
///
/// Three on-disk shapes, deserialized into one struct:
///
/// - **v1** (ADR 0007): `schema_version: 1`, carries
///   `disk_manifest` + `canonical_memory_manifest`. Disk bytes
///   resolve through BlobStorage via the chunk store.
/// - **v2** (ADR 0008 Phase 3): `schema_version: 2`, all v1 fields
///   plus `bootstrap_disk_available` / `bootstrap_memory_available`
///   flags signaling that Nydus-shaped sidecar layers exist in the
///   OCI artifact. The actual bootstrap file paths and chunk-blob
///   digests live on `CachedImage` (populated by the image_cache at
///   pull time from `PulledImage`).
/// - **v3** (ADR 0014 M1.11): `schema_version: 3`, all v2 fields plus
///   a top-level `canonical_snapshot` block carrying a portable
///   `SnapshotMetadata` (snapshot_id, memory_manifest, blob keys).
///   Coord's `enable_image` cascade reads this block to populate
///   the templates + snapshots tables; the host's image_cache
///   doesn't consume it directly, so v3 deserializes cleanly into
///   the v2 struct shape via `#[serde(default)]` ignores.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ImageBundle {
    pub schema_version: u32,
    pub disk_manifest: engram_chunk_store::ManifestRef,
    /// ADR 0008 Phase 3: bake produced Nydus-shaped disk layers
    /// alongside the chunked manifest. v1 readers ignore (defaults
    /// to false via `#[serde(default)]`); v2 readers pair this with
    /// the `CachedImage::disk_bootstrap_path` populated at pull.
    #[serde(default)]
    pub bootstrap_disk_available: bool,
}

// ADR 0021 P1.5: `CachedHarness` / `CachedHarnessExt4` structs
// deleted with the substrate; only their now-orphaned doc comments
// and `#[derive]` attributes are removed here.

#[derive(Debug)]
pub enum CacheError {
    Io(std::io::Error),
    Oci(OciError),
    /// Parsing the ADR 0007 bundle.json sidecar failed. The image
    /// itself is on disk; only the chunk-manifest plumbing is unusable.
    Bundle(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "image cache io: {e}"),
            Self::Oci(e) => write!(f, "image cache oci: {e}"),
            Self::Bundle(s) => write!(f, "image bundle parse: {s}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Oci(e) => Some(e),
            Self::Bundle(_) => None,
        }
    }
}

/// Read + parse `bundle.json` if it exists. Returns Ok(None) when
/// the file isn't present (older images don't carry one). Returns
/// `Err` on read/parse failure — we'd rather fail loudly than
/// silently fall back, since the bundle is the production-grade
/// path now.
async fn read_bundle(path: &Path) -> Result<Option<ImageBundle>, CacheError> {
    if !fs::try_exists(path).await.unwrap_or(false) {
        return Ok(None);
    }
    let bytes = fs::read(path).await.map_err(CacheError::Io)?;
    let bundle: ImageBundle = serde_json::from_slice(&bytes)
        .map_err(|e| CacheError::Bundle(format!("{}: {e}", path.display())))?;
    // ADR 0007: schema v1. ADR 0008 Phase 3: schema v2. ADR 0014
    // M1.11: schema v3 adds a top-level `canonical_snapshot` block
    // — the bake emits it whenever `capture_canonical_memory` ran.
    // Host-side image_cache doesn't consume that block directly
    // (coord's `enable_image` cascade does), so the extra field
    // deserializes cleanly into `ImageBundle` via the existing
    // `#[serde(default)]` ignore semantics; the schema_version
    // check just needs to allow it through.
    if bundle.schema_version > 3 {
        return Err(CacheError::Bundle(format!(
            "{}: unsupported schema_version {}",
            path.display(),
            bundle.schema_version,
        )));
    }
    Ok(Some(bundle))
}

/// Wrap a path in `Option<PathBuf>` based on existence — small
/// helper used by `ensure_image`'s cache-hit path to discover
/// optional Nydus-shaped sidecars without panicking.
async fn optional_path(path: &Path) -> Option<PathBuf> {
    if fs::try_exists(path).await.unwrap_or(false) {
        Some(path.to_path_buf())
    } else {
        None
    }
}

/// Read a digest sidecar file written by `ensure_image` at pull
/// time — a single line containing the `sha256:<hex>` digest of an
/// OCI chunk-blob layer. Returns `None` if the sidecar is absent
/// (the cache entry predates ADR 0008 Phase 3 or the artifact
/// wasn't Nydus-shaped).
async fn read_digest_sidecar(path: &Path) -> Option<String> {
    let bytes = fs::read(path).await.ok()?;
    let s = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[derive(Debug)]
struct GcCandidate {
    path: PathBuf,
    mtime: u64,
    size_bytes: u64,
}

fn strip_sha256_prefix(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

async fn touch(path: &Path) {
    // Update mtime so LRU GC sees this entry as recently used.
    // utimensat is the ideal call but tokio doesn't expose it; the
    // simplest portable trick is a no-op write to a sentinel inside
    // the dir, or just open+close. For our purposes, opening and
    // closing the dir's manifest is sufficient on Linux/macOS.
    let _ = fs::File::open(path).await;
    let _ = fs::OpenOptions::new()
        .read(true)
        .open(path.join(".touch"))
        .await;
}

async fn dir_size(path: &Path) -> Result<u64, std::io::Error> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

async fn load_uri_map(path: &Path) -> UriMap {
    match fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<UriMap>(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "by-uri.json corrupt; starting empty");
                UriMap::default()
            }
        },
        Err(_) => UriMap::default(),
    }
}

fn persist_uri_map(path: &Path, map: &UriMap) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("uri-map path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_oci::AnonymousResolver;

    fn empty_oci() -> OciClient {
        OciClient::new(Arc::new(AnonymousResolver))
    }

    #[tokio::test]
    async fn open_creates_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf(), empty_oci())
            .await
            .unwrap();
        assert!(cache.inner.root.join("images/sha256").is_dir());
        assert!(cache.inner.root.join("harness/sha256").is_dir());
    }

    #[tokio::test]
    async fn uri_map_round_trips_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("by-uri.json");
        let mut m = UriMap::default();
        m.entries.insert("a".into(), "sha256:1".into());
        m.entries.insert("b".into(), "sha256:2".into());
        persist_uri_map(&path, &m).unwrap();
        let loaded = load_uri_map(&path).await;
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries.get("a"), Some(&"sha256:1".to_string()));
    }

    #[tokio::test]
    async fn gc_under_budget_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf(), empty_oci())
            .await
            .unwrap();
        // Empty cache with a generous budget — nothing to free.
        let freed = cache.gc(1_000_000_000).await.unwrap();
        assert_eq!(freed, 0);
    }

    #[tokio::test]
    async fn gc_evicts_oldest_when_over_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf(), empty_oci())
            .await
            .unwrap();
        let img_root = tmp.path().join("images/sha256");

        // Plant two fake digest dirs, each 1 KiB, with distinct mtimes.
        for digest in ["aaaa", "bbbb"] {
            let dir = img_root.join(digest);
            fs::create_dir_all(&dir).await.unwrap();
            fs::write(dir.join("rootfs.ext4"), vec![0u8; 1024])
                .await
                .unwrap();
            // Stagger mtimes so `aaaa` is older than `bbbb`.
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
        // Budget 1500 bytes — must evict one (the older `aaaa`).
        let freed = cache.gc(1500).await.unwrap();
        assert!(freed >= 1024);
        assert!(!img_root.join("aaaa").exists());
        assert!(img_root.join("bbbb").exists());
    }

    #[test]
    fn strip_sha256_prefix_handles_both_forms() {
        assert_eq!(strip_sha256_prefix("sha256:abcd"), "abcd");
        assert_eq!(strip_sha256_prefix("abcd"), "abcd");
    }

    #[tokio::test]
    async fn read_bundle_returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("bundle.json");
        let got = read_bundle(&missing).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn read_bundle_parses_well_formed_v1() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bundle.json");
        let manifest_ref = engram_chunk_store::ManifestRef::new();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "disk_manifest": manifest_ref,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let got = read_bundle(&path).await.unwrap().expect("bundle present");
        assert_eq!(got.schema_version, 1);
        assert_eq!(got.disk_manifest, manifest_ref);
    }

    #[tokio::test]
    async fn read_bundle_rejects_unsupported_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bundle.json");
        // schema_version 99 — we only know v1/v2/v3.
        fs::write(
            &path,
            br#"{"schema_version":99,"disk_manifest":{"manifest_id":"00000000-0000-0000-0000-000000000000","version":1}}"#,
        )
        .await
        .unwrap();
        let err = read_bundle(&path).await.unwrap_err();
        match err {
            CacheError::Bundle(msg) => assert!(msg.contains("schema_version")),
            other => panic!("expected Bundle error, got {other:?}"),
        }
    }

    /// ADR 0014 M1.11 regression guard: schema v3 bundles carry the
    /// `canonical_snapshot` block. The host's image_cache doesn't
    /// consume that field directly (coord's `enable_image` cascade
    /// does), but it must still accept the bundle and produce a
    /// valid `ImageBundle` for the cold-create path.
    ///
    /// This test would have caught the prod regression where the
    /// host's bundle parser stayed at `> 2` after the bake started
    /// emitting v3 — session-create then errored "image bundle parse:
    /// unsupported schema_version 3" on every cold session, even
    /// though materialization + warm-pool refill had succeeded.
    #[tokio::test]
    async fn read_bundle_parses_well_formed_v2_with_bootstrap_flags() {
        // ADR 0008 Phase 3: v2 bundles carry bootstrap availability
        // flags. They must round-trip through the deserializer
        // alongside the v1 fields.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bundle.json");
        let manifest_ref = engram_chunk_store::ManifestRef::new();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 2,
                "disk_manifest": manifest_ref,
                "bootstrap_disk_available": true,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let got = read_bundle(&path).await.unwrap().expect("bundle present");
        assert_eq!(got.schema_version, 2);
        assert_eq!(got.disk_manifest, manifest_ref);
        assert!(got.bootstrap_disk_available);
    }

    // ---- ADR 0008 Phase 5: CachedImage chunked-OCI helpers ----

    fn cached_image_skeleton() -> CachedImage {
        CachedImage {
            manifest_path: PathBuf::from("/nonexistent/manifest.toml"),
            rootfs_path: None,
            bundle: None,
            disk_bootstrap_path: None,
            disk_chunks_blob_digest: None,
            digest: "sha256:test".into(),
        }
    }

    #[test]
    fn is_disk_chunked_oci_requires_both_bootstrap_and_digest() {
        let mut c = cached_image_skeleton();
        assert!(!c.is_disk_chunked_oci(), "skeleton: neither field set");

        c.disk_bootstrap_path = Some(PathBuf::from("/x/bootstrap.disk.json"));
        assert!(
            !c.is_disk_chunked_oci(),
            "bootstrap alone is insufficient — need digest"
        );

        c.disk_chunks_blob_digest = Some("sha256:abc".into());
        assert!(c.is_disk_chunked_oci(), "both set — chunked-OCI capable");

        c.disk_bootstrap_path = None;
        assert!(
            !c.is_disk_chunked_oci(),
            "digest alone is insufficient — need bootstrap"
        );
    }

    #[tokio::test]
    async fn build_oci_chunk_index_returns_none_for_non_chunked_image() {
        let c = cached_image_skeleton();
        assert!(c.build_oci_chunk_index().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn build_oci_chunk_index_resolves_self_blob_entries_to_primary_digest() {
        // Write a bootstrap with two entries: one with `blob_digest =
        // None` (resolves to the primary digest) and one with
        // `Some(parent_digest)` (stays cross-blob).
        let tmp = tempfile::tempdir().unwrap();
        let bs_path = tmp.path().join("bootstrap.disk.json");
        let self_hash = engram_chunk_store::ChunkHash::of(b"chunk-self");
        let parent_hash = engram_chunk_store::ChunkHash::of(b"chunk-parent");
        let bootstrap = engram_chunk_store::Bootstrap {
            schema_version: engram_chunk_store::BOOTSTRAP_SCHEMA_VERSION,
            kind: engram_chunk_store::ManifestKind::Disk,
            total_bytes: 32,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(16),
            entries: vec![
                engram_chunk_store::BootstrapEntry {
                    file_offset: 0,
                    blob_digest: None,
                    blob_offset: 0,
                    length: 16,
                    sha256: self_hash,
                },
                engram_chunk_store::BootstrapEntry {
                    file_offset: 16,
                    blob_digest: Some("sha256:parent_digest_xx".into()),
                    blob_offset: 4096, // arbitrary parent offset
                    length: 16,
                    sha256: parent_hash,
                },
            ],
        };
        fs::write(&bs_path, serde_json::to_vec(&bootstrap).unwrap())
            .await
            .unwrap();

        let mut c = cached_image_skeleton();
        c.disk_bootstrap_path = Some(bs_path);
        c.disk_chunks_blob_digest = Some("sha256:self_digest_yy".into());

        let index = c
            .build_oci_chunk_index()
            .await
            .unwrap()
            .expect("chunked image yields an index");
        assert_eq!(index.len(), 2);

        // Self-blob entry resolved to the primary digest.
        let loc_self = index.get(&self_hash).expect("self chunk in index");
        assert_eq!(loc_self.blob_digest, "sha256:self_digest_yy");
        assert_eq!(loc_self.offset, 0);
        assert_eq!(loc_self.length, 16);

        // Cross-blob entry kept its parent digest.
        let loc_parent = index.get(&parent_hash).expect("parent chunk in index");
        assert_eq!(loc_parent.blob_digest, "sha256:parent_digest_xx");
        assert_eq!(loc_parent.offset, 4096);
    }

    #[tokio::test]
    async fn read_bundle_v1_defaults_bootstrap_flags_to_false() {
        // A v1 bundle (no bootstrap fields) still deserializes —
        // serde defaults the new fields to `false`. Ensures forward
        // compat: older bakes don't break v2 readers.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bundle.json");
        let manifest_ref = engram_chunk_store::ManifestRef::new();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "disk_manifest": manifest_ref,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let got = read_bundle(&path).await.unwrap().expect("bundle present");
        assert_eq!(got.schema_version, 1);
        assert!(!got.bootstrap_disk_available);
    }
}
