//! S3 / S3-compatible (MinIO) backend.
//!
//! Phase 2 will wire this up against `aws-sdk-s3`. For now it's a typed
//! stub so the workspace compiles without pulling the AWS SDK into
//! every build.

use async_trait::async_trait;
use engram_core::traits::{BlobStorage, ByteStream, ObjectMetadata};
use engram_core::StorageError;

pub struct S3Storage {
    pub bucket: String,
    pub region: Option<String>,
    /// For MinIO and other S3-compatible servers.
    pub endpoint: Option<String>,
}

impl S3Storage {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: None,
            endpoint: None,
        }
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }
}

#[async_trait]
impl BlobStorage for S3Storage {
    async fn put(&self, _key: &str, _data: ByteStream) -> Result<(), StorageError> {
        Err(stub("put"))
    }

    async fn get(&self, _key: &str) -> Result<ByteStream, StorageError> {
        Err(stub("get"))
    }

    async fn delete(&self, _key: &str) -> Result<(), StorageError> {
        Err(stub("delete"))
    }

    async fn exists(&self, _key: &str) -> Result<bool, StorageError> {
        Err(stub("exists"))
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError> {
        Err(stub("list"))
    }
}

fn stub(op: &'static str) -> StorageError {
    StorageError::Sdk(format!("S3 {op} not yet implemented; see Phase 2 in DESIGN.md").into())
}
