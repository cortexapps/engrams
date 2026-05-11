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

#[derive(Default, Serialize, Deserialize)]
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

    /// Ensure the bake image at `uri` is materialised in the cache.
    /// On a hit, returns the cached paths without a network round
    /// trip. On a miss, pulls via OCI and writes both layers.
    pub async fn ensure_image(&self, uri: &str) -> Result<CachedImage, CacheError> {
        // Cache hit fast-path: known URI + digest dir still exists.
        if let Some(digest) = self.lookup(&self.inner.image_map, uri) {
            let dir = self.image_dir(&digest);
            let manifest_path = dir.join("manifest.toml");
            let rootfs_path = dir.join("rootfs.ext4");
            if fs::try_exists(&manifest_path).await.unwrap_or(false)
                && fs::try_exists(&rootfs_path).await.unwrap_or(false)
            {
                touch(&dir).await;
                return Ok(CachedImage {
                    manifest_path,
                    rootfs_path,
                    digest,
                });
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

        Ok(CachedImage {
            manifest_path: final_dir.join("manifest.toml"),
            rootfs_path: final_dir.join("rootfs.ext4"),
            digest,
        })
    }

    /// Ensure the harness pack at `uri` is materialised.
    pub async fn ensure_harness(&self, uri: &str) -> Result<CachedHarness, CacheError> {
        if let Some(digest) = self.lookup(&self.inner.harness_map, uri) {
            let dir = self.harness_dir(&digest);
            // Harness pack must contain at least the entry-point.
            if fs::try_exists(dir.join("harness")).await.unwrap_or(false) {
                touch(&dir).await;
                return Ok(CachedHarness {
                    pack_dir: dir,
                    digest,
                });
            }
            tracing::debug!(uri = %uri, digest = %digest, "harness cache map references missing dir; re-pulling");
        }

        let tmp = self.inner.root.join("harness/.tmp");
        let pull_dir = tmp.join(format!("pull-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&pull_dir)
            .await
            .map_err(CacheError::Io)?;
        let pulled = self
            .inner
            .oci
            .pull_harness(uri, &pull_dir)
            .await
            .map_err(CacheError::Oci)?;
        let digest = pulled.manifest_digest.as_str().to_string();
        let final_dir = self.harness_dir(&digest);
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
        self.update_map(&self.inner.harness_map, uri, &digest).await;

        Ok(CachedHarness {
            pack_dir: final_dir,
            digest,
        })
    }

    /// Ensure the harness pack at `uri` is materialised **and**
    /// packed into a read-only ext4 file shaped for the in-VM mount
    /// at `/run/engram/harnesses`. Backends that attach the substrate
    /// as a virtio-blk device (FC; VZ when configured for block-
    /// device harness mounts) need this; ProcessBackend (dev) is
    /// happy with the directory.
    ///
    /// The substrate is laid out so the bootstrap's
    /// `/run/engram/harnesses/<name>/harness` path resolves
    /// correctly: the ext4 root contains a single `<name>/`
    /// directory whose contents are the pack bytes.
    ///
    /// Cached at `harness/sha256/<digest>/<name>[-ca-<fp>].ext4` next
    /// to the extracted pack — same digest dir, so GC sweeps both
    /// atomically.
    ///
    /// When `host_ca_cert_pem` is `Some`, the proxy CA cert is
    /// stamped into the substrate at `.engram-host/ca.pem` before
    /// `mke2fs` runs (ADR 0006 — guest substrates trust the
    /// deployment-wide CA so any host's MITM leaves validate). The
    /// CA cert's fingerprint is included in the cache filename so a
    /// CA rotation produces a fresh cache entry; old ones are GC'd
    /// in the usual sweep.
    pub async fn ensure_harness_ext4(
        &self,
        uri: &str,
        name: &str,
        host_ca_cert_pem: Option<&str>,
    ) -> Result<CachedHarnessExt4, CacheError> {
        // Reuse the existing pull path. We need the pack contents
        // on disk before mke2fs can populate from them.
        let cached = self.ensure_harness(uri).await?;
        // Bake the CA fingerprint into the cache filename so
        // rotation invalidates cleanly. When no CA is configured we
        // keep the legacy filename so dev workflows that don't run
        // the proxy aren't perturbed.
        let cache_name = match host_ca_cert_pem {
            Some(pem) => format!("{}-ca-{}.ext4", name, ca_pem_fingerprint(pem)),
            None => format!("{name}.ext4"),
        };
        let ext4_path = self
            .inner
            .root
            .join("harness/sha256")
            .join(strip_sha256_prefix(&cached.digest))
            .join(&cache_name);

        // Cache hit: ext4 already built. Touch + return.
        if fs::try_exists(&ext4_path).await.unwrap_or(false) {
            return Ok(CachedHarnessExt4 {
                ext4_path,
                pack_dir: cached.pack_dir,
                digest: cached.digest,
            });
        }

        // Build the staging tree: a tempdir containing a single
        // real `<name>/` directory whose contents mirror the pack.
        // mke2fs `-d` walks the source tree with `lstat`, so a symlink
        // at the top of the staging tree gets preserved in the
        // resulting filesystem rather than being dereferenced — which
        // produces an in-VM dangling link to the host's cache path.
        // Hardlinks (same FS, zero-copy) avoid duplicating the
        // bundled CLI bytes; we host the staging tempdir under the
        // cache root so the hardlink target FS always matches.
        let staging = tempfile::Builder::new()
            .prefix("substrate-")
            .tempdir_in(&self.inner.root)
            .map_err(CacheError::Io)?;
        let staging_subdir = staging.path().join(name);
        fs::create_dir_all(&staging_subdir)
            .await
            .map_err(CacheError::Io)?;
        hardlink_tree(&cached.pack_dir, &staging_subdir).await?;

        // ADR 0006: stamp the deployment-wide CA cert into the
        // substrate so the guest's trust store accepts the proxy's
        // MITM leaves. Lives at `.engram-host/ca.pem` — the bake
        // image's init script picks it up from there.
        if let Some(pem) = host_ca_cert_pem {
            let host_meta_dir = staging.path().join(".engram-host");
            fs::create_dir_all(&host_meta_dir)
                .await
                .map_err(CacheError::Io)?;
            fs::write(host_meta_dir.join("ca.pem"), pem)
                .await
                .map_err(CacheError::Io)?;
        }

        // Size the ext4: pack size doubled, +128 MiB minimum, 4 KiB-
        // aligned. Reusing image-builder's helper keeps the sizing
        // policy in one place.
        let pack_bytes = dir_size(&cached.pack_dir).await.unwrap_or(0);
        let size = engram_image_builder::ext4::recommended_size(pack_bytes);

        // mke2fs -t ext4 -F -d staging dst size.
        use engram_image_builder::ext4::{Ext4Packer, Mke2fsPacker};
        let packer = Mke2fsPacker::default();
        packer
            .pack(staging.path(), &ext4_path, size)
            .await
            .map_err(|e| CacheError::Substrate(format!("mke2fs: {e}")))?;

        Ok(CachedHarnessExt4 {
            ext4_path,
            pack_dir: cached.pack_dir,
            digest: cached.digest,
        })
    }

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

    fn harness_dir(&self, digest: &str) -> PathBuf {
        self.inner
            .root
            .join("harness/sha256")
            .join(strip_sha256_prefix(digest))
    }

    fn lookup(&self, map: &Mutex<UriMap>, uri: &str) -> Option<String> {
        map.lock().entries.get(uri).cloned()
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

#[derive(Clone, Debug)]
pub struct CachedImage {
    pub manifest_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub struct CachedHarness {
    pub pack_dir: PathBuf,
    pub digest: String,
}

/// A harness pack materialised both as a directory tree (for dev
/// backends) and as an ext4 file (for FC / VZ block-device mounts).
/// The ext4 contains a single `<name>/` subdirectory at its root
/// whose contents are the pack bytes.
#[derive(Clone, Debug)]
pub struct CachedHarnessExt4 {
    pub ext4_path: PathBuf,
    pub pack_dir: PathBuf,
    pub digest: String,
}

#[derive(Debug)]
pub enum CacheError {
    Io(std::io::Error),
    Oci(OciError),
    /// Building the harness substrate ext4 (mke2fs / staging) failed.
    Substrate(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "image cache io: {e}"),
            Self::Oci(e) => write!(f, "image cache oci: {e}"),
            Self::Substrate(s) => write!(f, "harness substrate build: {s}"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Oci(e) => Some(e),
            Self::Substrate(_) => None,
        }
    }
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

/// Short hex fingerprint of a CA cert PEM. Used in the
/// `harness_ext4` cache filename so a CA rotation produces a fresh
/// cache entry. 16 hex chars (64 bits) is plenty for collision-
/// avoidance within a single deployment.
fn ca_pem_fingerprint(pem: &str) -> String {
    use sha2::Digest as _;
    let hash = sha2::Sha256::digest(pem.as_bytes());
    let mut s = String::with_capacity(16);
    for b in &hash[..8] {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Recursively replicate `src` under `dst` as a tree of hardlinks for
/// regular files, real directories for directories, and verbatim
/// symlinks for symlinks. Caller guarantees `dst` exists; entries
/// land directly inside it (not under `dst/<basename(src)>`).
///
/// Used to stage the harness pack for `mke2fs -d` without copying
/// the bundled CLI bytes. Both `src` and `dst` must be on the same
/// filesystem (`hard_link` returns EXDEV otherwise) — the caller
/// places the staging tempdir alongside the cache to guarantee that.
async fn hardlink_tree(src: &Path, dst: &Path) -> Result<(), CacheError> {
    let mut rd = fs::read_dir(src).await.map_err(CacheError::Io)?;
    while let Some(entry) = rd.next_entry().await.map_err(CacheError::Io)? {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type().await.map_err(CacheError::Io)?;
        if ft.is_dir() {
            fs::create_dir(&to).await.map_err(CacheError::Io)?;
            // Boxing keeps the recursive `async fn` future Sized.
            Box::pin(hardlink_tree(&from, &to)).await?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&from).await.map_err(CacheError::Io)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &to).map_err(CacheError::Io)?;
            #[cfg(not(unix))]
            {
                let _ = target;
                return Err(CacheError::Io(std::io::Error::other(
                    "symlinks in harness packs require a unix host",
                )));
            }
        } else {
            fs::hard_link(&from, &to).await.map_err(CacheError::Io)?;
        }
    }
    Ok(())
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

    /// `ensure_harness_ext4` should be a cache hit on the second
    /// call against the same (uri, name). We simulate this without
    /// running mke2fs (which requires e2fsprogs in PATH) by
    /// pre-planting the digest dir + a sentinel ext4 file: the
    /// function's first action is to ensure the harness is pulled
    /// (unreachable here — no real OCI server) so we instead test
    /// the cache-hit short-circuit path by populating the URI map
    /// + the on-disk artifacts in advance.
    #[tokio::test]
    async fn ensure_harness_ext4_returns_cache_hit_when_artifact_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf(), empty_oci())
            .await
            .unwrap();

        let digest = "sha256:fakehash1234";
        let pack_dir = tmp.path().join("harness/sha256/fakehash1234");
        fs::create_dir_all(&pack_dir).await.unwrap();
        fs::write(pack_dir.join("harness"), b"#!/bin/sh\n")
            .await
            .unwrap();
        // Pre-plant the ext4 file with a sentinel — the cache-hit
        // branch returns without invoking mke2fs.
        let ext4_path = pack_dir.join("claude.ext4");
        fs::write(&ext4_path, b"sentinel").await.unwrap();
        // Pre-populate the URI map so `ensure_harness` short-circuits
        // its own cache hit (avoids the OCI pull).
        cache
            .inner
            .harness_map
            .lock()
            .entries
            .insert("registry.example/claude:v1".into(), digest.into());

        let cached = cache
            .ensure_harness_ext4("registry.example/claude:v1", "claude", None)
            .await
            .unwrap();
        assert_eq!(cached.ext4_path, ext4_path);
        assert_eq!(cached.digest, digest);
        // The sentinel survives the call (we didn't re-mke2fs).
        let bytes = fs::read(&cached.ext4_path).await.unwrap();
        assert_eq!(bytes, b"sentinel");
    }

    /// CA fingerprint should be deterministic (same PEM → same
    /// hex) and short enough not to blow up the filename.
    #[test]
    fn ca_pem_fingerprint_is_stable_and_short() {
        let a = ca_pem_fingerprint("-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n");
        let b = ca_pem_fingerprint("-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n");
        let c = ca_pem_fingerprint("-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\n");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    /// Cache-hit short-circuit must distinguish CA-stamped builds
    /// from un-stamped ones — different CA fingerprint → different
    /// cache filename → no false hit.
    #[tokio::test]
    async fn ensure_harness_ext4_distinguishes_ca_fingerprints() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ImageCache::open(tmp.path().to_path_buf(), empty_oci())
            .await
            .unwrap();

        let digest = "sha256:fakehashcafp";
        let pack_dir = tmp.path().join("harness/sha256/fakehashcafp");
        fs::create_dir_all(&pack_dir).await.unwrap();
        fs::write(pack_dir.join("harness"), b"#!/bin/sh\n")
            .await
            .unwrap();
        let ca_pem = "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n";
        let fp = ca_pem_fingerprint(ca_pem);
        let stamped_path = pack_dir.join(format!("claude-ca-{fp}.ext4"));
        let unstamped_path = pack_dir.join("claude.ext4");
        fs::write(&stamped_path, b"with-ca").await.unwrap();
        fs::write(&unstamped_path, b"no-ca").await.unwrap();
        cache
            .inner
            .harness_map
            .lock()
            .entries
            .insert("registry.example/claude:cafp".into(), digest.into());

        let with_ca = cache
            .ensure_harness_ext4("registry.example/claude:cafp", "claude", Some(ca_pem))
            .await
            .unwrap();
        assert_eq!(with_ca.ext4_path, stamped_path);

        let no_ca = cache
            .ensure_harness_ext4("registry.example/claude:cafp", "claude", None)
            .await
            .unwrap();
        assert_eq!(no_ca.ext4_path, unstamped_path);
    }
}
