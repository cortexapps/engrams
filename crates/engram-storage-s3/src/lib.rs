//! S3 / S3-compatible (MinIO, Cloudflare R2, etc.) BlobStorage
//! implementation backed by `aws-sdk-s3`.
//!
//! Credentials follow the standard AWS resolution chain
//! (environment variables, instance metadata, shared config) — no
//! custom credential plumbing here. For local dev against MinIO,
//! set `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` and pass the
//! MinIO URL via `with_endpoint`.
//!
//! Streaming note: `put` collects the inbound `ByteStream` into a
//! `Vec<u8>` before handing it to the S3 SDK as a single body. This
//! is fine for the dev-tier ProcessBackend snapshots (small
//! tarballs) but bounds memory at the snapshot size; multi-GB
//! Firecracker memory.bin uploads should switch to a streaming
//! `ByteStream::from_body_1_x` once we have enough wire-side
//! plumbing to know the content length up front.

use async_trait::async_trait;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::Client;
use chrono::{TimeZone, Utc};
use engram_core::traits::{BlobStorage, ByteStream, ObjectMetadata};
use engram_core::StorageError;
use futures::stream::StreamExt;

pub struct S3Storage {
    pub bucket: String,
    client: tokio::sync::OnceCell<Client>,
    region: Option<String>,
    endpoint: Option<String>,
}

impl S3Storage {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            client: tokio::sync::OnceCell::new(),
            region: None,
            endpoint: None,
        }
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Override the S3 endpoint URL. Used for MinIO / R2 / other
    /// S3-compatible servers. Standard AWS leaves this unset and
    /// the SDK derives the URL from the configured region.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Lazily build the S3 client on first use. Defers the AWS
    /// credential lookup (which can hit IMDSv2 etc.) until something
    /// actually needs to talk to S3, and shares the constructed
    /// client across calls.
    async fn client(&self) -> Result<&Client, StorageError> {
        self.client
            .get_or_try_init(|| async {
                // `defaults(latest())` opts in to the current
                // behaviour-version policy (the SDK warns loudly
                // when this isn't pinned). Region/endpoint over-
                // rides layer on top of the env defaults.
                let mut loader = aws_config::defaults(
                    aws_config::BehaviorVersion::latest(),
                );
                if let Some(region) = self.region.as_deref() {
                    loader = loader.region(aws_sdk_s3::config::Region::new(region.to_string()));
                }
                let shared = loader.load().await;
                let mut s3 = aws_sdk_s3::config::Builder::from(&shared);
                if let Some(endpoint) = self.endpoint.as_deref() {
                    s3 = s3.endpoint_url(endpoint).force_path_style(true);
                }
                Ok::<_, StorageError>(Client::from_conf(s3.build()))
            })
            .await
    }
}

#[async_trait]
impl BlobStorage for S3Storage {
    async fn put(&self, key: &str, mut data: ByteStream) -> Result<(), StorageError> {
        let client = self.client().await?;
        let mut buf = Vec::new();
        while let Some(chunk) = data.next().await {
            let chunk = chunk?;
            buf.extend_from_slice(&chunk);
        }
        client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from(buf))
            .send()
            .await
            .map_err(|e| sdk_err("put_object", e))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<ByteStream, StorageError> {
        let client = self.client().await?;
        let resp = client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| match &e {
                SdkError::ServiceError(svc) if svc.err().is_no_such_key() => {
                    StorageError::NotFound(key.to_string())
                }
                _ => sdk_err("get_object", e),
            })?;
        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| StorageError::Sdk(format!("collect body: {e}").into()))?
            .into_bytes();
        let stream = futures::stream::once(async move { Ok(body) });
        Ok(Box::pin(stream))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let client = self.client().await?;
        client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| sdk_err("delete_object", e))?;
        Ok(())
    }

    async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        let client = self.client().await?;
        match client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(svc)) => match svc.err() {
                HeadObjectError::NotFound(_) => Ok(false),
                other => Err(StorageError::Sdk(format!("head_object: {other}").into())),
            },
            Err(other) => Err(sdk_err("head_object", other)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMetadata>, StorageError> {
        let client = self.client().await?;
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(token) = continuation.as_deref() {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| sdk_err("list_objects_v2", e))?;
            for obj in resp.contents.unwrap_or_default() {
                out.push(ObjectMetadata {
                    key: obj.key.unwrap_or_default(),
                    size: obj.size.unwrap_or_default() as u64,
                    last_modified: obj
                        .last_modified
                        .and_then(|t| Utc.timestamp_opt(t.secs(), t.subsec_nanos()).single())
                        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap()),
                    etag: obj.e_tag,
                });
            }
            if resp.is_truncated.unwrap_or(false) {
                continuation = resp.next_continuation_token;
                if continuation.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }
}

fn sdk_err<E: std::error::Error + Send + Sync + 'static>(
    op: &'static str,
    err: SdkError<E>,
) -> StorageError {
    StorageError::Sdk(format!("S3 {op}: {err}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn builder_chain_compiles() {
        let _ = S3Storage::new("my-bucket")
            .with_region("us-east-1")
            .with_endpoint("http://localhost:9000");
    }

    /// Live test against a real S3 (or MinIO at the standard
    /// docker-compose port). Set ENGRAM_TEST_S3_BUCKET +
    /// ENGRAM_TEST_S3_ENDPOINT (optional) + AWS_* credentials to
    /// run. The test does a put / exists / get / list / delete
    /// round trip on a unique key and cleans up after itself.
    #[tokio::test]
    #[ignore = "requires live S3 / MinIO at ENGRAM_TEST_S3_BUCKET"]
    async fn live_round_trip() {
        let bucket = match std::env::var("ENGRAM_TEST_S3_BUCKET") {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut storage = S3Storage::new(bucket);
        if let Ok(endpoint) = std::env::var("ENGRAM_TEST_S3_ENDPOINT") {
            storage = storage.with_endpoint(endpoint);
        }
        if let Ok(region) = std::env::var("ENGRAM_TEST_S3_REGION") {
            storage = storage.with_region(region);
        }

        let key = format!("engram-test/{}.bin", uuid::Uuid::new_v4().simple());
        let payload = Bytes::from_static(b"engram-s3-roundtrip");

        let put_stream =
            futures::stream::once(async move { Ok::<_, StorageError>(payload.clone()) });
        storage.put(&key, Box::pin(put_stream)).await.expect("put");

        assert!(storage.exists(&key).await.expect("exists"));

        let mut got = storage.get(&key).await.expect("get");
        let mut buf = Vec::new();
        while let Some(chunk) = got.next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(buf, b"engram-s3-roundtrip");

        let listed = storage.list(&key).await.expect("list");
        assert!(listed.iter().any(|m| m.key == key));

        storage.delete(&key).await.expect("delete");
        assert!(!storage.exists(&key).await.expect("exists post-delete"));
    }
}
