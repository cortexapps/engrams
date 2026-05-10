//! GCS-backed [`BlobStorage`].
//!
//! Wires `google-cloud-storage` v0.24 against the trait. Application
//! Default Credentials (ADC) by default; emulator support via
//! `STORAGE_EMULATOR_HOST` (the standard convention `fake-gcs-server`
//! honors). Returning to ADC after an emulator override is just
//! unsetting the env var.
//!
//! Endpoint resolution:
//! - `STORAGE_EMULATOR_HOST=http://localhost:4443` → anonymous auth +
//!   plaintext to that endpoint (used by `just dev`).
//! - Anywhere else → ADC + production HTTPS.
//!
//! Streaming:
//! - `get_streaming` returns the SDK's native `Stream<Item =
//!   Result<Bytes, ...>>` directly — no re-buffering, GB-scale memory
//!   files don't materialize in RAM.
//! - `put_streaming` currently collects the inbound body to a
//!   `Vec<u8>` and uses the SDK's `Multipart` upload. Acceptable for
//!   the fake-gcs-server local tests + modest-sized payloads; Stage 5
//!   may switch to resumable-upload semantics if profiling shows it's
//!   worth the complexity for the FC `memory.bin` flush path.
//!
//! 404 → [`BlobError::NotFound`]; idempotent delete.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream};
use futures::StreamExt;
use google_cloud_storage::client::{Client, ClientConfig};
use google_cloud_storage::http::objects::delete::DeleteObjectRequest;
use google_cloud_storage::http::objects::download::Range;
use google_cloud_storage::http::objects::get::GetObjectRequest;
use google_cloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use google_cloud_storage::http::Error as GcsHttpError;

/// GCS-backed storage. One client per instance, cheap to clone.
pub struct GcsBlobStorage {
    client: Arc<Client>,
    bucket: String,
}

impl GcsBlobStorage {
    /// Construct against a bucket. When `STORAGE_EMULATOR_HOST` is
    /// set, points the client at that endpoint with anonymous auth
    /// — `fake-gcs-server` for local dev. Otherwise uses ADC against
    /// production GCS.
    ///
    /// Note: `google-cloud-storage` v0.24 does not auto-honor
    /// `STORAGE_EMULATOR_HOST` (`grep -rn STORAGE_EMULATOR_HOST` in
    /// the SDK turns up only a `// TODO emulator support` note). We
    /// thread it through explicitly here.
    pub async fn connect(bucket: impl Into<String>) -> Result<Self, BlobError> {
        let cfg = if let Ok(host) = std::env::var("STORAGE_EMULATOR_HOST") {
            // Emulator mode: anonymous auth + the override endpoint.
            // Strip a trailing slash so requests don't double up.
            let endpoint = host.trim_end_matches('/').to_string();
            tracing::info!(endpoint, "gcs: using STORAGE_EMULATOR_HOST");
            ClientConfig {
                storage_endpoint: endpoint,
                ..ClientConfig::default()
            }
            .anonymous()
        } else {
            ClientConfig::default()
                .with_auth()
                .await
                .map_err(|e| BlobError::Config(format!("gcs auth: {e}")))?
        };
        Ok(Self {
            client: Arc::new(Client::new(cfg)),
            bucket: bucket.into(),
        })
    }
}

fn map_http_err(e: GcsHttpError) -> BlobError {
    match &e {
        GcsHttpError::Response(resp) if resp.code == 404 => BlobError::NotFound,
        _ => BlobError::Sdk(Box::new(e)),
    }
}

#[async_trait]
impl BlobStorage for GcsBlobStorage {
    async fn put_streaming(&self, key: &str, mut body: ByteStream) -> Result<u64, BlobError> {
        // Drain into a Vec<u8>. See the module docstring — fine for
        // emulator tests and small payloads; the FC flush path may
        // want resumable uploads in a follow-up.
        let mut buf = Vec::new();
        while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk?);
        }
        let total = buf.len() as u64;
        let req = UploadObjectRequest {
            bucket: self.bucket.clone(),
            ..Default::default()
        };
        let upload_type = UploadType::Simple(Media::new(key.to_string()));
        self.client
            .upload_object(&req, buf, &upload_type)
            .await
            .map_err(map_http_err)?;
        tracing::debug!(bucket = %self.bucket, key = %key, bytes = total, "gcs put");
        Ok(total)
    }

    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        let req = GetObjectRequest {
            bucket: self.bucket.clone(),
            object: key.to_string(),
            ..Default::default()
        };
        let stream = self
            .client
            .download_streamed_object(&req, &Range::default())
            .await
            .map_err(map_http_err)?;
        // Re-emit via our ByteStream newtype, mapping the SDK's error
        // type into our BlobError. The body keeps streaming directly
        // off the wire — no re-buffering.
        let mapped = stream.map(|chunk| chunk.map_err(|e| BlobError::Sdk(Box::new(e))));
        Ok(ByteStream::new(mapped))
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        let req = GetObjectRequest {
            bucket: self.bucket.clone(),
            object: key.to_string(),
            ..Default::default()
        };
        let obj = self.client.get_object(&req).await.map_err(map_http_err)?;
        Ok(BlobObjectMeta {
            size_bytes: obj.size as u64,
            // GCS exposes both an etag and a generation number; etag
            // is the standard cross-bucket caller-friendly handle.
            etag: Some(obj.etag),
        })
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        let req = DeleteObjectRequest {
            bucket: self.bucket.clone(),
            object: key.to_string(),
            ..Default::default()
        };
        match self.client.delete_object(&req).await {
            Ok(()) => Ok(()),
            // Idempotent: missing key is a successful delete.
            Err(e) => match map_http_err(e) {
                BlobError::NotFound => Ok(()),
                other => Err(other),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that `connect` doesn't panic when `STORAGE_EMULATOR_HOST`
    /// is set and the emulator is reachable. Gated on the env var being
    /// present so it's a no-op in CI environments without docker.
    #[tokio::test(flavor = "current_thread")]
    async fn connect_against_emulator_smoke() {
        let Ok(_emu) = std::env::var("STORAGE_EMULATOR_HOST") else {
            // No emulator configured; nothing to verify here.
            return;
        };
        // The bucket itself doesn't have to exist for `connect()` to
        // succeed — it just has to be a valid string. Bucket creation
        // is `deploy/dev/seed-buckets.sh`'s job.
        let _store = GcsBlobStorage::connect("engram-snapshots-test")
            .await
            .expect("connect against emulator");
    }

    /// End-to-end round-trip against the live emulator: put a body,
    /// get it back, verify byte-for-byte equality, exists/head agree
    /// on size, delete is idempotent. Gated on
    /// `STORAGE_EMULATOR_HOST` + `ENGRAM_TEST_GCS_BUCKET` so it
    /// no-ops on hosts without docker. Run locally via
    /// `bash deploy/dev/seed-buckets.sh && \
    ///  ENGRAM_TEST_GCS_BUCKET=engram-snapshots-test \
    ///  STORAGE_EMULATOR_HOST=http://localhost:4443 \
    ///  cargo nextest run -p engram-storage-gcs`.
    #[tokio::test(flavor = "current_thread")]
    async fn round_trip_against_emulator() {
        let Ok(_emu) = std::env::var("STORAGE_EMULATOR_HOST") else {
            return;
        };
        let Ok(bucket) = std::env::var("ENGRAM_TEST_GCS_BUCKET") else {
            return;
        };

        let store = GcsBlobStorage::connect(bucket)
            .await
            .expect("connect against emulator");

        // Use a unique key per run so concurrent test invocations
        // don't collide on the shared emulator state.
        let key = format!(
            "engram/snapshots/test/round-trip-{}.bin",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        // Body designed to surface common SDK bugs:
        //   - Larger than one TCP frame (1 KiB+) so streaming, not
        //     a single sync flush, is what's exercised.
        //   - Bytes that include 0x00 + 0xFF + every-other-byte to
        //     surface any text-mode mangling.
        let body: Vec<u8> = (0..4096_u16).map(|i| (i % 257) as u8).collect();

        let put_size = store
            .put(&key, bytes::Bytes::from(body.clone()))
            .await
            .expect("put");
        assert_eq!(put_size, body.len() as u64);

        let head = store.head(&key).await.expect("head");
        assert_eq!(head.size_bytes, body.len() as u64);
        assert!(head.etag.is_some(), "GCS responses always include etag");

        let got = store.get(&key).await.expect("get");
        assert_eq!(&got[..], &body[..], "round-tripped bytes must match");

        // Delete + idempotent re-delete.
        store.delete(&key).await.expect("delete");
        assert!(!store.exists(&key).await.unwrap());
        store
            .delete(&key)
            .await
            .expect("delete on missing key must be idempotent");

        // After delete, get/head should surface NotFound.
        assert!(matches!(store.head(&key).await, Err(BlobError::NotFound)));
    }
}
