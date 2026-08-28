//! Filesystem-backed [`BlobStorage`].
//!
//! Bytes live under `<root>/<key>` (key path components segregate via
//! `/`). Writes go through a tempfile + `rename` so a partial upload
//! never replaces a good blob with garbage. Reads stream the file
//! back as `Bytes` chunks.
//!
//! Used as:
//! - The default `ENGRAM_BLOB_BACKEND=local` in dev (no emulator
//!   needed).
//! - The integration-test backend for `engram-coordinator` and
//!   `engram-host-agent` — fast, hermetic, no docker.
//!
//! **Not for production.** Hosts come and go; cold-tier blobs need to
//! survive that. Use `engram-storage-s3` or `engram-storage-gcs` in
//! deployments.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream};
use futures::StreamExt;
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Filesystem-backed storage rooted at `root`. Created lazily on the
/// first `put`.
pub struct LocalBlobStorage {
    root: PathBuf,
}

impl LocalBlobStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        // Keys are slash-separated; the join handles segments. Reject
        // absolute keys / path-traversal segments at the boundary.
        self.root.join(key)
    }

    fn validate_key(key: &str) -> Result<(), BlobError> {
        if key.is_empty() {
            return Err(BlobError::Config("empty blob key".into()));
        }
        if key.starts_with('/') {
            return Err(BlobError::Config(format!("absolute blob key: {key}")));
        }
        for seg in key.split('/') {
            if seg == ".." || seg == "." {
                return Err(BlobError::Config(format!(
                    "key contains traversal segment: {key}"
                )));
            }
        }
        Ok(())
    }

    async fn ensure_parent(path: &Path) -> Result<(), BlobError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl BlobStorage for LocalBlobStorage {
    async fn put_streaming(&self, key: &str, mut body: ByteStream) -> Result<u64, BlobError> {
        Self::validate_key(key)?;
        let dst = self.path_for(key);
        Self::ensure_parent(&dst).await?;

        // Tempfile + rename so a partial body never lands at `dst`.
        // Suffix is process-id + nanos to keep concurrent puts to the
        // same key from clobbering each other's tempfiles.
        let tmp = dst.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut file = fs::File::create(&tmp).await?;
        let mut total: u64 = 0;
        while let Some(chunk) = body.next().await {
            let bytes = chunk?;
            file.write_all(&bytes).await?;
            total += bytes.len() as u64;
        }
        file.flush().await?;
        // Drop the file handle before rename so windows-y filesystems
        // don't choke; a no-op on POSIX but defensible.
        drop(file);
        fs::rename(&tmp, &dst).await?;
        tracing::debug!(key = %key, bytes = total, "local blob put");
        Ok(total)
    }

    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        Self::validate_key(key)?;
        let path = self.path_for(key);
        let file = match fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound);
            }
            Err(e) => return Err(BlobError::Io(e)),
        };
        // Wrap the tokio file in a futures::Stream of Bytes via
        // tokio-util's ReaderStream. Default capacity is 8 KiB chunks
        // — fine for a dev backend.
        let reader = tokio_util::io::ReaderStream::new(file);
        let mapped = reader.map(|chunk| chunk.map_err(BlobError::Io));
        Ok(ByteStream::new(mapped))
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        Self::validate_key(key)?;
        let path = self.path_for(key);
        let meta = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BlobError::NotFound);
            }
            Err(e) => return Err(BlobError::Io(e)),
        };
        let etag = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| format!("{}-{}", d.as_secs(), d.subsec_nanos()));
        Ok(BlobObjectMeta {
            size_bytes: meta.len(),
            etag,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        Self::validate_key(key)?;
        let path = self.path_for(key);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Idempotent on missing keys.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        // Empty prefix means "the entire root." Validate non-empty
        // prefixes the same way we validate keys, then walk the
        // matching subtree.
        if !prefix.is_empty() {
            Self::validate_key(prefix)?;
        }
        let start = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(prefix)
        };
        // If the prefix points at a file, return just that key.
        // If it points at a directory, walk and collect file keys.
        // If it doesn't exist, return empty.
        let metadata = match fs::metadata(&start).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(BlobError::Io(e)),
        };
        let mut out = Vec::new();
        if metadata.is_file() {
            out.push(prefix.to_string());
            return Ok(out);
        }
        let mut stack: Vec<PathBuf> = vec![start];
        while let Some(dir) = stack.pop() {
            let mut rd = fs::read_dir(&dir).await?;
            while let Some(entry) = rd.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    // Strip the root prefix to reconstruct the key.
                    let rel = path
                        .strip_prefix(&self.root)
                        .map_err(|_| {
                            BlobError::Io(std::io::Error::other("listed path escaped root"))
                        })?
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push(rel);
                }
            }
        }
        // GCS/S3 `list` returns keys in lexicographic order; `fs::read_dir`
        // yields them in filesystem-dependent order (differs across
        // filesystems AND platforms). Sort so this local backend matches the
        // production object-store contract — dev/sim behavior mirrors prod,
        // and no consumer can pick up a `read_dir`-order determinism leak
        // (ADR 0098 D5).
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use engram_testkit::blob_conformance;
    use tempfile::tempdir;

    // The shared trait-contract scenarios (round-trip, streaming,
    // zero-byte put, failed-put atomicity, prefix listing) live in
    // `engram_testkit::blob_conformance` and run against every
    // backend. Tests below the conformance block are local-specific:
    // key validation and the lexicographic-ordering guarantee this
    // backend adds on top of the contract.

    #[tokio::test]
    async fn conformance_round_trip() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        blob_conformance::round_trip(&store).await;
    }

    #[tokio::test]
    async fn conformance_streaming_round_trip() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        // A handful of chunks proves writer advancement; volume is
        // the wire backends' concern.
        blob_conformance::streaming_round_trip(&store, 1024, 3).await;
    }

    #[tokio::test]
    async fn conformance_zero_byte_put() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        blob_conformance::zero_byte_put(&store).await;
    }

    #[tokio::test]
    async fn conformance_failed_put_preserves_prior_blob() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        blob_conformance::failed_streaming_put_preserves_prior_blob(&store).await;
    }

    #[tokio::test]
    async fn conformance_list_prefix_scoped() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        blob_conformance::list_prefix_scoped(&store, 30).await;
    }

    /// Local storage takes the trait's DEFAULT `list_prefix_page`, so
    /// this is the coverage for that default — the walk every backend
    /// without native pagination inherits.
    #[tokio::test]
    async fn conformance_list_prefix_page_walks_whole_prefix() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        blob_conformance::list_prefix_page_walks_whole_prefix(&store, 30, 7).await;
    }

    #[tokio::test]
    async fn keys_with_traversal_segments_rejected() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        for bad in ["", "/abs/key", "../escape", "a/../b"] {
            let res = store.head(bad).await;
            assert!(matches!(res, Err(BlobError::Config(_))), "bad key {bad:?}");
        }
    }

    #[tokio::test]
    async fn list_prefix_walks_matching_subtree() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        store.put("a/x", Bytes::from_static(b"1")).await.unwrap();
        store.put("a/y", Bytes::from_static(b"1")).await.unwrap();
        store
            .put("a/nested/z", Bytes::from_static(b"1"))
            .await
            .unwrap();
        store.put("b/q", Bytes::from_static(b"1")).await.unwrap();

        // list_prefix returns keys already in lexicographic order (the
        // GCS/S3 contract), NOT filesystem `read_dir` order — assert the
        // returned Vec directly, with no test-side re-sort to mask a leak.
        let keys = store.list_prefix("a").await.unwrap();
        assert_eq!(keys, vec!["a/nested/z", "a/x", "a/y"]);

        let nested = store.list_prefix("a/nested").await.unwrap();
        assert_eq!(nested, vec!["a/nested/z"]);

        let everything = store.list_prefix("").await.unwrap();
        assert_eq!(everything, vec!["a/nested/z", "a/x", "a/y", "b/q"]);
    }

    #[tokio::test]
    async fn list_prefix_returns_empty_for_missing_subtree() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        let keys = store.list_prefix("never-existed").await.unwrap();
        assert!(keys.is_empty());
    }
}
