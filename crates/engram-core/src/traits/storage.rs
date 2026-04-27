use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::Stream;

use crate::error::StorageError;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send + 'static>>;

#[derive(Clone, Debug)]
pub struct ObjectMetadata {
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
    pub etag: Option<String>,
}

/// Pluggable object store: GCS, S3/MinIO, or local FS. Used for snapshot
/// cold tier and (eventually) warm-image artifacts.
#[async_trait]
pub trait BlobStorage: Send + Sync {
    async fn put(&self, key: &str, data: ByteStream) -> Result<(), StorageError>;
    async fn get(&self, key: &str) -> Result<ByteStream, StorageError>;
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
    async fn exists(&self, key: &str) -> Result<bool, StorageError>;
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError>;
}
