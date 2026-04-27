//! Filesystem-backed [`BlobStorage`] implementation. Used for single-host
//! development and CI; not intended for production durability.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use engram_core::traits::{BlobStorage, ByteStream, ObjectMetadata};
use engram_core::StorageError;
use futures::stream::StreamExt;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, key: &str) -> Result<PathBuf, StorageError> {
        if key.is_empty() || key.contains("..") || key.starts_with('/') {
            return Err(StorageError::InvalidKey(key.to_string()));
        }
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl BlobStorage for LocalStorage {
    async fn put(&self, key: &str, mut data: ByteStream) -> Result<(), StorageError> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = fs::File::create(&path).await?;
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<ByteStream, StorageError> {
        let path = self.resolve(key)?;
        if !fs::try_exists(&path).await? {
            return Err(StorageError::NotFound(key.to_string()));
        }
        let mut file = fs::File::open(&path).await?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        let stream = futures::stream::once(async move { Ok(Bytes::from(buf)) });
        Ok(Box::pin(stream))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let path = self.resolve(key)?;
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let path = self.resolve(key)?;
        Ok(fs::try_exists(&path).await?)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError> {
        let prefix_path = self.resolve(prefix)?;
        let mut out = Vec::new();
        list_recursive(&self.root, &prefix_path, &mut out).await?;
        Ok(out)
    }
}

async fn list_recursive(
    root: &Path,
    prefix: &Path,
    out: &mut Vec<ObjectMetadata>,
) -> Result<(), StorageError> {
    // Walk the directory containing `prefix` (or `prefix` itself if it's
    // a directory), collecting files whose path starts with `prefix`.
    let walk_root = if fs::try_exists(prefix).await.unwrap_or(false)
        && fs::metadata(prefix)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
    {
        prefix.to_path_buf()
    } else {
        prefix.parent().unwrap_or(root).to_path_buf()
    };
    let mut stack = vec![walk_root];
    while let Some(dir) = stack.pop() {
        let mut entries = match fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && path.starts_with(prefix) {
                let key = path
                    .strip_prefix(root)
                    .map_err(|_| StorageError::InvalidKey(path.display().to_string()))?
                    .to_string_lossy()
                    .into_owned();
                out.push(ObjectMetadata {
                    key,
                    size: meta.len(),
                    last_modified: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .and_then(|d| {
                            Utc.timestamp_opt(d.as_secs() as i64, d.subsec_nanos())
                                .single()
                        })
                        .unwrap_or_else(default_time),
                    etag: None,
                });
            }
        }
    }
    Ok(())
}

fn default_time() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0).single().expect("epoch is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let payload = Bytes::from_static(b"hello engram");
        let stream = futures::stream::once(async move { Ok::<_, StorageError>(payload.clone()) });
        storage.put("a/b/c.bin", Box::pin(stream)).await.unwrap();
        assert!(storage.exists("a/b/c.bin").await.unwrap());

        let mut s = storage.get("a/b/c.bin").await.unwrap();
        let mut buf = Vec::new();
        while let Some(chunk) = s.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, b"hello engram");
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        match storage.get("nope").await {
            Err(StorageError::NotFound(_)) => {}
            Err(other) => panic!("expected NotFound, got error: {other}"),
            Ok(_) => panic!("expected NotFound, got Ok"),
        }
    }

    #[tokio::test]
    async fn invalid_keys_rejected() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        for bad in ["", "../escape", "/abs", "nested/../escape"] {
            assert!(
                matches!(storage.exists(bad).await, Err(StorageError::InvalidKey(_))),
                "key {bad:?} should be rejected",
            );
        }
    }

    #[tokio::test]
    async fn put_concatenates_multi_chunk_stream() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let chunks: Vec<Result<Bytes, StorageError>> = vec![
            Ok(Bytes::from_static(b"alpha-")),
            Ok(Bytes::from_static(b"beta-")),
            Ok(Bytes::from_static(b"gamma")),
        ];
        let stream = futures::stream::iter(chunks);
        storage.put("multi.bin", Box::pin(stream)).await.unwrap();

        let mut s = storage.get("multi.bin").await.unwrap();
        let mut buf = Vec::new();
        while let Some(chunk) = s.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, b"alpha-beta-gamma");
    }

    #[tokio::test]
    async fn put_overwrites_existing_value() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let put = |bytes: &'static [u8]| {
            let stream =
                futures::stream::once(
                    async move { Ok::<_, StorageError>(Bytes::from_static(bytes)) },
                );
            storage.put("key.bin", Box::pin(stream))
        };
        put(b"first").await.unwrap();
        put(b"second-and-longer").await.unwrap();

        let mut s = storage.get("key.bin").await.unwrap();
        let mut buf = Vec::new();
        while let Some(chunk) = s.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, b"second-and-longer");
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        // First delete on a missing key must succeed (idempotency contract).
        storage.delete("never-existed").await.unwrap();

        // Now write then delete then delete again; both must succeed.
        let stream =
            futures::stream::once(async { Ok::<_, StorageError>(Bytes::from_static(b"x")) });
        storage.put("doomed", Box::pin(stream)).await.unwrap();
        storage.delete("doomed").await.unwrap();
        storage.delete("doomed").await.unwrap();
        assert!(!storage.exists("doomed").await.unwrap());
    }

    #[tokio::test]
    async fn list_filters_by_prefix() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let put = |key: &'static str| {
            let stream =
                futures::stream::once(async { Ok::<_, StorageError>(Bytes::from_static(b"data")) });
            storage.put(key, Box::pin(stream))
        };
        put("snapshots/sess-a/0.bin").await.unwrap();
        put("snapshots/sess-a/1.bin").await.unwrap();
        put("snapshots/sess-b/0.bin").await.unwrap();
        put("images/foo.bin").await.unwrap();

        let mut listed: Vec<String> = storage
            .list("snapshots/sess-a")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.key)
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                "snapshots/sess-a/0.bin".to_string(),
                "snapshots/sess-a/1.bin".to_string(),
            ],
            "list must include only files under the given prefix",
        );
    }

    #[tokio::test]
    async fn list_records_metadata() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let stream =
            futures::stream::once(async { Ok::<_, StorageError>(Bytes::from_static(b"hello")) });
        storage.put("only.bin", Box::pin(stream)).await.unwrap();
        let listed = storage.list("only.bin").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "only.bin");
        assert_eq!(listed[0].size, 5, "size must be the file length on disk");
    }

    #[tokio::test]
    async fn list_returns_empty_for_nonexistent_prefix() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let listed = storage.list("does/not/exist").await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn nested_subdirectories_are_created() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let stream =
            futures::stream::once(async { Ok::<_, StorageError>(Bytes::from_static(b"deep")) });
        storage
            .put("a/b/c/d/e.bin", Box::pin(stream))
            .await
            .unwrap();
        assert!(storage.exists("a/b/c/d/e.bin").await.unwrap());
    }

    #[tokio::test]
    async fn put_propagates_stream_errors() {
        let dir = tmpdir();
        let storage = LocalStorage::new(dir.path());
        let chunks: Vec<Result<Bytes, StorageError>> = vec![
            Ok(Bytes::from_static(b"good")),
            Err(StorageError::Truncated {
                expected: 100,
                got: 4,
            }),
        ];
        let stream = futures::stream::iter(chunks);
        let res = storage.put("partial.bin", Box::pin(stream)).await;
        assert!(matches!(res, Err(StorageError::Truncated { .. })));
    }
}
