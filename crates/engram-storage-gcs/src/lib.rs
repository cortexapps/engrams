//! Google Cloud Storage backend.
//!
//! Phase 2 will wire this up against the `google-cloud-storage` crate.
//! For now this is a typed stub that returns `StorageError::Sdk` so the
//! workspace compiles cleanly without pulling the GCS SDK into every
//! build.

use async_trait::async_trait;
use engram_core::traits::{BlobStorage, ByteStream, ObjectMetadata};
use engram_core::StorageError;

pub struct GcsStorage {
    pub bucket: String,
    pub project: Option<String>,
}

impl GcsStorage {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            project: None,
        }
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }
}

#[async_trait]
impl BlobStorage for GcsStorage {
    async fn put(&self, _key: &str, _data: ByteStream) -> Result<(), StorageError> {
        Err(unimplemented_stub("put"))
    }

    async fn get(&self, _key: &str) -> Result<ByteStream, StorageError> {
        Err(unimplemented_stub("get"))
    }

    async fn delete(&self, _key: &str) -> Result<(), StorageError> {
        Err(unimplemented_stub("delete"))
    }

    async fn exists(&self, _key: &str) -> Result<bool, StorageError> {
        Err(unimplemented_stub("exists"))
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError> {
        Err(unimplemented_stub("list"))
    }
}

fn unimplemented_stub(op: &'static str) -> StorageError {
    StorageError::Sdk(format!("GCS {op} not yet implemented; see Phase 2 in DESIGN.md").into())
}
