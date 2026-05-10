//! GCS-backed [`BlobStorage`] (stub).
//!
//! Stage 0 ships this as a typed placeholder so the workspace
//! compiles, the trait surface is reachable, and downstream callers
//! can route through `Arc<dyn BlobStorage>` without conditional
//! compilation. Every method returns
//! `BlobError::Config("engram-storage-gcs is a Stage 0 stub; wire
//! google-cloud-storage in Stage 4")`.
//!
//! Stage 4 replaces this body with a real `google-cloud-storage` v0.24
//! client:
//! - ADC (Application Default Credentials) by default; honors
//!   `GOOGLE_APPLICATION_CREDENTIALS`.
//! - Endpoint override via `STORAGE_EMULATOR_HOST` (the standard GCS
//!   convention, honored by `fake-gcs-server`).
//! - Resumable uploads above ~5 MB; single-shot below.
//! - 404 → `BlobError::NotFound`; idempotent delete.

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream};

const STUB_REASON: &str =
    "engram-storage-gcs is a Stage 0 stub; wire google-cloud-storage in Stage 4";

/// GCS-backed storage. Stage 0 stub; do not use.
pub struct GcsBlobStorage {
    /// Bucket name. Read but unused until Stage 4.
    pub bucket: String,
    /// Optional emulator host (e.g. `http://localhost:4443` for
    /// `fake-gcs-server`).
    pub emulator_host: Option<String>,
}

impl GcsBlobStorage {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            emulator_host: None,
        }
    }
}

#[async_trait]
impl BlobStorage for GcsBlobStorage {
    async fn put_streaming(&self, _key: &str, _body: ByteStream) -> Result<u64, BlobError> {
        Err(BlobError::Config(STUB_REASON.into()))
    }

    async fn put(&self, _key: &str, _body: Bytes) -> Result<u64, BlobError> {
        Err(BlobError::Config(STUB_REASON.into()))
    }

    async fn get_streaming(&self, _key: &str) -> Result<ByteStream, BlobError> {
        Err(BlobError::Config(STUB_REASON.into()))
    }

    async fn head(&self, _key: &str) -> Result<BlobObjectMeta, BlobError> {
        Err(BlobError::Config(STUB_REASON.into()))
    }

    async fn delete(&self, _key: &str) -> Result<(), BlobError> {
        Err(BlobError::Config(STUB_REASON.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn stub_methods_return_typed_config_error() {
        let store = GcsBlobStorage::new("bucket");
        let err = store.head("k").await.unwrap_err();
        match err {
            BlobError::Config(msg) => assert!(msg.contains("Stage 0")),
            other => panic!("expected Config, got {other}"),
        }
    }
}
