//! Test doubles shared by this crate's unit tests.

use std::sync::atomic::{AtomicU64, Ordering};

use engram_core::error::BlobError;
use engram_core::traits::storage::{BlobObjectMeta, ByteStream};
use engram_core::traits::BlobStorage;
use engram_storage_local::LocalBlobStorage;

/// Counts the chunk GETs a stage makes; manifest reads do not count.
pub(crate) struct CountingBlob {
    inner: LocalBlobStorage,
    gets: AtomicU64,
}

impl CountingBlob {
    pub(crate) fn new(inner: LocalBlobStorage) -> Self {
        Self {
            inner,
            gets: AtomicU64::new(0),
        }
    }

    pub(crate) fn gets(&self) -> u64 {
        self.gets.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl BlobStorage for CountingBlob {
    async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError> {
        self.inner.put_streaming(key, body).await
    }
    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        if key.starts_with("chunks/") {
            self.gets.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.get_streaming(key).await
    }
    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(key).await
    }
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        self.inner.list_prefix(prefix).await
    }
}
