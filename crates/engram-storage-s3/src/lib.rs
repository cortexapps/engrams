//! S3-backed [`BlobStorage`] (stub).
//!
//! Stage 0 ships this as a typed placeholder so the workspace
//! compiles, the trait surface is reachable, and downstream callers
//! can route through `Arc<dyn BlobStorage>` without conditional
//! compilation. Every method returns
//! `BlobError::Config("engram-storage-s3 is a Stage 0 stub; wire
//! aws-sdk-s3 in Stage 4")`.
//!
//! Stage 4 replaces this body with a real `aws-sdk-s3` v1 client:
//! - `put_streaming` via `ByteStream::from_body_1_x` (no
//!   `Vec<u8>` collection on a 1.5 GB memory.bin).
//! - Region/endpoint env-var override (`ENGRAM_S3_REGION`,
//!   `ENGRAM_S3_ENDPOINT_URL`) so the local MinIO emulator + R2 +
//!   Spaces all work without code changes.
//! - Default credential chain (IAM role > env > shared config).
//! - 404 / `NoSuchKey` → `BlobError::NotFound`; idempotent delete.

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream};

const STUB_REASON: &str = "engram-storage-s3 is a Stage 0 stub; wire aws-sdk-s3 in Stage 4";

/// S3-backed storage. Stage 0 stub; do not use.
pub struct S3BlobStorage {
    /// Bucket name. Read but unused until Stage 4.
    pub bucket: String,
    /// Optional endpoint override (MinIO etc.).
    pub endpoint_url: Option<String>,
    /// Optional region override.
    pub region: Option<String>,
}

impl S3BlobStorage {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            endpoint_url: None,
            region: None,
        }
    }
}

#[async_trait]
impl BlobStorage for S3BlobStorage {
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

    async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>, BlobError> {
        Err(BlobError::Config(STUB_REASON.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn stub_methods_return_typed_config_error() {
        let store = S3BlobStorage::new("bucket");
        let err = store.head("k").await.unwrap_err();
        match err {
            BlobError::Config(msg) => assert!(msg.contains("Stage 0")),
            other => panic!("expected Config, got {other}"),
        }
    }
}
