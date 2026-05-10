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
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use futures::stream;
    use tempfile::tempdir;

    fn small_body() -> ByteStream {
        ByteStream::from_bytes(Bytes::from_static(b"hello cold tier"))
    }

    #[tokio::test]
    async fn put_then_get_round_trips_small_body() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());

        let n = store
            .put_streaming("snapshots/abc/123.tar.zst", small_body())
            .await
            .unwrap();
        assert_eq!(n, b"hello cold tier".len() as u64);

        let body = store.get("snapshots/abc/123.tar.zst").await.unwrap();
        assert_eq!(&body[..], b"hello cold tier");
    }

    #[tokio::test]
    async fn head_returns_size_and_etag() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());

        store.put("k", Bytes::from_static(b"abc")).await.unwrap();
        let meta = store.head("k").await.unwrap();
        assert_eq!(meta.size_bytes, 3);
        assert!(meta.etag.is_some());
    }

    #[tokio::test]
    async fn missing_key_yields_not_found_uniformly() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());

        assert!(matches!(
            store.head("missing").await,
            Err(BlobError::NotFound)
        ));
        assert!(matches!(
            store.get_streaming("missing").await,
            Err(BlobError::NotFound)
        ));
        assert!(!store.exists("missing").await.unwrap());
    }

    #[tokio::test]
    async fn delete_is_idempotent_on_missing() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());
        // Never written; delete still succeeds.
        store.delete("never-existed").await.unwrap();
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
    async fn streaming_put_handles_multi_chunk_body() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());

        // Body in three chunks; tests that the writer advances
        // properly and the size accumulates correctly.
        let chunks: Vec<Result<Bytes, BlobError>> = vec![
            Ok(Bytes::from_static(b"part-1-")),
            Ok(Bytes::from_static(b"part-2-")),
            Ok(Bytes::from_static(b"part-3")),
        ];
        let body = ByteStream::new(stream::iter(chunks));
        let n = store.put_streaming("multi", body).await.unwrap();
        assert_eq!(n, b"part-1-part-2-part-3".len() as u64);

        let got = store.get("multi").await.unwrap();
        assert_eq!(&got[..], b"part-1-part-2-part-3");
    }

    #[tokio::test]
    async fn put_with_partial_body_does_not_replace_existing_good_blob() {
        let dir = tempdir().unwrap();
        let store = LocalBlobStorage::new(dir.path());

        // First, write a valid blob.
        store.put("k", Bytes::from_static(b"good")).await.unwrap();

        // Now, attempt a put whose stream errors mid-way. The
        // tempfile-then-rename strategy means `k` should still hold
        // the original bytes after the failed put.
        let chunks: Vec<Result<Bytes, BlobError>> = vec![
            Ok(Bytes::from_static(b"new-partial")),
            Err(BlobError::Protocol("simulated mid-stream failure".into())),
        ];
        let body = ByteStream::new(stream::iter(chunks));
        let res = store.put_streaming("k", body).await;
        assert!(res.is_err(), "partial body must surface as error");

        let got = store.get("k").await.unwrap();
        assert_eq!(&got[..], b"good", "good blob preserved on failed retry");
    }
}
