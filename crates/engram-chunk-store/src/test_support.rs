//! Shared test doubles for this crate's unit tests.

use std::sync::Arc;

use engram_core::traits::storage::BlobStorage;

/// A `BlobStorage` that wraps a real local store and counts the
/// `head()` (HEAD/exists) and `put()` (upload) calls the chunk-store
/// makes, so tests can assert which round-trips a path performs — e.g.
/// that flush paths skip the dedup HEAD (ADR 0078 move 5) and that the
/// sparse re-chunk's unchanged-skip performs no store I/O at all
/// (ADR 0101 A).
pub(crate) struct CountingBlob {
    inner: Arc<dyn BlobStorage>,
    exists_calls: std::sync::atomic::AtomicUsize,
    put_calls: std::sync::atomic::AtomicUsize,
}

impl CountingBlob {
    pub(crate) fn wrap(inner: Arc<dyn BlobStorage>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            exists_calls: std::sync::atomic::AtomicUsize::new(0),
            put_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }
    pub(crate) fn exists_count(&self) -> usize {
        self.exists_calls.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) fn put_count(&self) -> usize {
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
    ) -> std::result::Result<engram_core::traits::storage::ByteStream, engram_core::error::BlobError>
    {
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
    async fn delete(&self, key: &str) -> std::result::Result<(), engram_core::error::BlobError> {
        self.inner.delete(key).await
    }
    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> std::result::Result<Vec<String>, engram_core::error::BlobError> {
        self.inner.list_prefix(prefix).await
    }
}
