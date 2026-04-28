//! Google Cloud Storage backend backed by `google-cloud-storage`.
//!
//! Credentials follow Application Default Credentials (ADC): GCE
//! metadata server when running on GCE, otherwise
//! `GOOGLE_APPLICATION_CREDENTIALS` pointing at a service-account JSON.
//! For local dev set the env var; for production rely on the
//! attached service account.
//!
//! Streaming note: `put` collects the inbound `ByteStream` into a
//! `Vec<u8>` before uploading. Same trade-off as the S3 backend —
//! fine for dev-tier ProcessBackend tarballs, bounds memory at
//! snapshot size for Firecracker memory.bin uploads. Streaming
//! resumable uploads land with the broader Phase 4 cloud work.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::traits::{BlobStorage, ByteStream, ObjectMetadata};
use engram_core::StorageError;
use futures::stream::StreamExt;
use google_cloud_storage::client::{Client, ClientConfig};
use google_cloud_storage::http::objects::delete::DeleteObjectRequest;
use google_cloud_storage::http::objects::download::Range;
use google_cloud_storage::http::objects::get::GetObjectRequest;
use google_cloud_storage::http::objects::list::ListObjectsRequest;
use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use google_cloud_storage::http::Error as GcsHttpError;

pub struct GcsStorage {
    pub bucket: String,
    pub project: Option<String>,
    client: tokio::sync::OnceCell<Client>,
}

impl GcsStorage {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            project: None,
            client: tokio::sync::OnceCell::new(),
        }
    }

    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Lazily build the GCS client on first use. Defers ADC resolution
    /// (which can hit the GCE metadata server) until something
    /// actually needs to talk to GCS.
    async fn client(&self) -> Result<&Client, StorageError> {
        self.client
            .get_or_try_init(|| async {
                let config = ClientConfig::default()
                    .with_auth()
                    .await
                    .map_err(|e| StorageError::Sdk(format!("gcs auth: {e}").into()))?;
                Ok::<_, StorageError>(Client::new(config))
            })
            .await
    }
}

#[async_trait]
impl BlobStorage for GcsStorage {
    async fn put(&self, key: &str, mut data: ByteStream) -> Result<(), StorageError> {
        let client = self.client().await?;
        let mut buf = Vec::new();
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);
        }
        let req = UploadObjectRequest {
            bucket: self.bucket.clone(),
            ..Default::default()
        };
        let upload = UploadType::Simple(Media::new(key.to_string()));
        client
            .upload_object(&req, buf, &upload)
            .await
            .map_err(|e| http_err("upload_object", e))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<ByteStream, StorageError> {
        let client = self.client().await?;
        let req = GetObjectRequest {
            bucket: self.bucket.clone(),
            object: key.to_string(),
            ..Default::default()
        };
        let bytes = client
            .download_object(&req, &Range::default())
            .await
            .map_err(|e| match &e {
                GcsHttpError::Response(r) if r.code == 404 => {
                    StorageError::NotFound(key.to_string())
                }
                _ => http_err("download_object", e),
            })?;
        let chunk = bytes::Bytes::from(bytes);
        let stream = futures::stream::once(async move { Ok(chunk) });
        Ok(Box::pin(stream))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let client = self.client().await?;
        let req = DeleteObjectRequest {
            bucket: self.bucket.clone(),
            object: key.to_string(),
            ..Default::default()
        };
        match client.delete_object(&req).await {
            Ok(()) => Ok(()),
            // GCS returns 404 if the object doesn't exist; preserve
            // BlobStorage's idempotent-delete contract.
            Err(GcsHttpError::Response(r)) if r.code == 404 => Ok(()),
            Err(e) => Err(http_err("delete_object", e)),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let client = self.client().await?;
        let req = GetObjectRequest {
            bucket: self.bucket.clone(),
            object: key.to_string(),
            ..Default::default()
        };
        match client.get_object(&req).await {
            Ok(_) => Ok(true),
            Err(GcsHttpError::Response(r)) if r.code == 404 => Ok(false),
            Err(e) => Err(http_err("get_object", e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError> {
        let client = self.client().await?;
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let req = ListObjectsRequest {
                bucket: self.bucket.clone(),
                prefix: Some(prefix.to_string()),
                page_token: page_token.clone(),
                ..Default::default()
            };
            let resp = client
                .list_objects(&req)
                .await
                .map_err(|e| http_err("list_objects", e))?;
            for obj in resp.items.unwrap_or_default() {
                // google-cloud-storage's `updated` is a
                // `time::OffsetDateTime`; convert through unix
                // seconds so we don't take a hard `time` dep.
                let last_modified: DateTime<Utc> = obj
                    .updated
                    .and_then(|odt| {
                        chrono::DateTime::from_timestamp(odt.unix_timestamp(), 0)
                    })
                    .unwrap_or_else(Utc::now);
                out.push(ObjectMetadata {
                    key: obj.name,
                    size: obj.size as u64,
                    last_modified,
                    etag: Some(obj.etag),
                });
            }
            match resp.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(out)
    }
}

fn http_err(op: &'static str, err: GcsHttpError) -> StorageError {
    StorageError::Sdk(format!("GCS {op}: {err}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn builder_chain_compiles() {
        let _ = GcsStorage::new("my-bucket").with_project("my-project");
    }

    /// Live test against a real GCS bucket. Set
    /// ENGRAM_TEST_GCS_BUCKET + GOOGLE_APPLICATION_CREDENTIALS to
    /// run. Round-trips put/exists/get/list/delete on a unique key.
    #[tokio::test]
    #[ignore = "requires live GCS at ENGRAM_TEST_GCS_BUCKET"]
    async fn live_round_trip() {
        let bucket = match std::env::var("ENGRAM_TEST_GCS_BUCKET") {
            Ok(v) => v,
            Err(_) => return,
        };
        let storage = GcsStorage::new(bucket);

        let key = format!("engram-test/{}.bin", uuid::Uuid::new_v4().simple());
        let payload = Bytes::from_static(b"engram-gcs-roundtrip");

        let put_stream =
            futures::stream::once(async move { Ok::<_, StorageError>(payload.clone()) });
        storage.put(&key, Box::pin(put_stream)).await.expect("put");

        assert!(storage.exists(&key).await.expect("exists"));

        let mut got = storage.get(&key).await.expect("get");
        let mut buf = Vec::new();
        while let Some(chunk) = got.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, b"engram-gcs-roundtrip");

        let listed = storage.list(&key).await.expect("list");
        assert!(listed.iter().any(|m| m.key == key));

        storage.delete(&key).await.expect("delete");
        assert!(!storage.exists(&key).await.expect("exists post-delete"));
    }
}
