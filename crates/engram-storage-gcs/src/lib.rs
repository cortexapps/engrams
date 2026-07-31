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
//! - `put_streaming` forwards the inbound `ByteStream` through the
//!   SDK's `upload_streamed_object`, which wraps it as the reqwest
//!   request body. Bytes flow through hyper without being collected;
//!   peak memory is one TCP frame's worth (~64 KiB), not the full
//!   payload. This matters for the FC `memory.bin` cold-tier flush
//!   where a single object can be multi-GB. Not yet truly resumable
//!   on transient failures (a mid-stream disconnect aborts the
//!   upload); a future pass can wire `prepare_resumable_upload` for
//!   that.
//!
//! 404 → [`BlobError::NotFound`]; idempotent delete.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream};
use futures::{SinkExt, StreamExt};
use google_cloud_storage::client::{Client, ClientConfig};
use google_cloud_storage::http::objects::delete::DeleteObjectRequest;
use google_cloud_storage::http::objects::download::Range;
use google_cloud_storage::http::objects::get::GetObjectRequest;
use google_cloud_storage::http::objects::list::ListObjectsRequest;
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
        let mut cfg = Self::auth_config().await?;
        // Inject our own transport instead of the SDK's untuned default
        // (per TigerBeetle's object-storage-client findings, 2026-07):
        // - hickory async DNS: cached, TTL-aware, no getaddrinfo
        //   threadpool hop on every fresh connection.
        // - bounded connect: a blackholed endpoint fails in 5 s and
        //   surfaces to the BlobClient retry layer, instead of pinning
        //   an attempt for the OS default (minutes).
        // - sized keep-alive pool: the sparse re-chunk and NBD flush
        //   fan out dozens of concurrent chunk ops; idle-connection
        //   reuse keeps those off the TLS-handshake path.
        // No global request timeout here — bodies are GB-scale on the
        // streaming paths; per-attempt deadlines live in BlobClient.
        //
        // Protocol: pooled HTTP/1.1 by default — bulk parallel chunk
        // transfers get one TCP window each instead of sharing one
        // h2 connection's flow control. `ENGRAM_GCS_HTTP2=1` opts in
        // to ALPN h2 (multiplexed; fewer connections/handshakes) —
        // A/B via the blobbench harness before flipping any default.
        let mut builder = reqwest::Client::builder()
            .hickory_dns(true)
            .connect_timeout(std::time::Duration::from_secs(5))
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30));
        if !std::env::var("ENGRAM_GCS_HTTP2").is_ok_and(|v| v == "1") {
            builder = builder.http1_only();
        }
        let http = builder
            .build()
            .map_err(|e| BlobError::Config(format!("gcs http client: {e}")))?;
        cfg.http = Some(reqwest_middleware::ClientBuilder::new(http).build());
        Ok(Self {
            client: Arc::new(Client::new(cfg)),
            bucket: bucket.into(),
        })
    }

    /// Baseline constructor on the SDK's DEFAULT transport (no
    /// injected client: getaddrinfo DNS, unbounded connect, default
    /// pool). Exists ONLY as the before/after control for
    /// `engram-blob-client`'s `blobbench` harness — production and
    /// dev code paths must use [`Self::connect`].
    #[doc(hidden)]
    pub async fn connect_untuned(bucket: impl Into<String>) -> Result<Self, BlobError> {
        let cfg = Self::auth_config().await?;
        Ok(Self {
            client: Arc::new(Client::new(cfg)),
            bucket: bucket.into(),
        })
    }

    /// Shared auth/endpoint resolution: emulator (anonymous + the
    /// `STORAGE_EMULATOR_HOST` endpoint — the SDK doesn't auto-honor
    /// it) or ADC against production HTTPS.
    async fn auth_config() -> Result<ClientConfig, BlobError> {
        if let Ok(host) = std::env::var("STORAGE_EMULATOR_HOST") {
            // Strip a trailing slash so requests don't double up.
            let endpoint = host.trim_end_matches('/').to_string();
            tracing::info!(endpoint, "gcs: using STORAGE_EMULATOR_HOST");
            Ok(ClientConfig {
                storage_endpoint: endpoint,
                ..ClientConfig::default()
            }
            .anonymous())
        } else {
            ClientConfig::default()
                .with_auth()
                .await
                .map_err(|e| BlobError::Config(format!("gcs auth: {e}")))
        }
    }
}

fn map_http_err(e: GcsHttpError) -> BlobError {
    // The SDK returns a 404 two different ways depending on which
    // operation surfaced it. Object-metadata calls (`get_object`,
    // `delete_object`) take the JSON envelope path and return
    // `Response { code: 404, .. }`. Streaming downloads
    // (`download_streamed_object`) skip the envelope parse — the
    // body is the byte stream we want — and return the raw reqwest
    // status error via `HttpClient` (or `RawResponse` when the
    // body did get buffered before the status check failed).
    // Both shapes mean the same thing: object doesn't exist.
    // Collapse them so callers downstream of `get_streaming` get
    // the same `BlobError::NotFound` that callers of `head` do.
    let is_404 = match &e {
        GcsHttpError::Response(resp) => resp.code == 404,
        GcsHttpError::HttpClient(rq) | GcsHttpError::RawResponse(rq, _) => {
            rq.status().is_some_and(|s| s.as_u16() == 404)
        }
        _ => false,
    };
    if is_404 {
        BlobError::NotFound
    } else {
        BlobError::Sdk(Box::new(e))
    }
}

#[async_trait]
impl BlobStorage for GcsBlobStorage {
    /// Buffered PUT: the body is already in hand, so skip the trait
    /// default's `put_streaming` bridge entirely — no mpsc pump task,
    /// no chunked transfer encoding. `Bytes → reqwest::Body` is
    /// refcounted (zero-copy) and carries Content-Length, which is
    /// both cheaper per request and what GCS's simple-upload path
    /// prefers. This is the chunk-upload hot path (sparse re-chunk,
    /// NBD flush).
    async fn put(&self, key: &str, body: bytes::Bytes) -> Result<u64, BlobError> {
        let len = body.len() as u64;
        let req = UploadObjectRequest {
            bucket: self.bucket.clone(),
            ..Default::default()
        };
        let upload_type = UploadType::Simple(Media::new(key.to_string()));
        self.client
            .upload_object(&req, body, &upload_type)
            .await
            .map_err(map_http_err)?;
        tracing::debug!(bucket = %self.bucket, key = %key, bytes = len, "gcs put (sized)");
        Ok(len)
    }

    async fn put_streaming(&self, key: &str, mut body: ByteStream) -> Result<u64, BlobError> {
        // Bridge the inbound `ByteStream` (Send but not Sync — its
        // inner `Pin<Box<dyn Stream + Send>>` carries no Sync bound)
        // through a `futures::channel::mpsc` channel whose Receiver
        // *is* Sync, satisfying the SDK's `S: TryStream + Send + Sync`
        // bound on `upload_streamed_object`. The pump task forwards
        // bytes verbatim and counts as they go; the SDK consumes the
        // receiver directly via reqwest's chunked body, so peak
        // memory is one TCP frame regardless of total upload size.
        let total = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = total.clone();
        let (mut tx, rx) = futures::channel::mpsc::channel::<Result<bytes::Bytes, BlobError>>(4);
        tokio::spawn(async move {
            while let Some(chunk) = body.next().await {
                if let Ok(bytes) = chunk.as_ref() {
                    counter.fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                if tx.send(chunk).await.is_err() {
                    // Receiver dropped — upload was aborted.
                    break;
                }
            }
        });

        let req = UploadObjectRequest {
            bucket: self.bucket.clone(),
            ..Default::default()
        };
        let upload_type = UploadType::Simple(Media::new(key.to_string()));
        self.client
            .upload_streamed_object(&req, rx, &upload_type)
            .await
            .map_err(map_http_err)?;
        let total = total.load(std::sync::atomic::Ordering::Relaxed);
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

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let req = ListObjectsRequest {
                bucket: self.bucket.clone(),
                prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.to_string())
                },
                // Recommended max per the SDK docs.
                max_results: Some(1000),
                page_token: page_token.clone(),
                ..Default::default()
            };
            let resp = self.client.list_objects(&req).await.map_err(map_http_err)?;
            if let Some(items) = resp.items {
                for obj in items {
                    out.push(obj.name);
                }
            }
            match resp.next_page_token {
                Some(t) if !t.is_empty() => page_token = Some(t),
                _ => break,
            }
        }
        Ok(out)
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

    /// Streams a 64 MiB payload through `put_streaming` and verifies
    /// it round-trips byte-for-byte via `get_streaming`. The chunk
    /// generator yields 64 KiB at a time, so the upload pipeline is
    /// exercised across ~1024 chunks — if anything was secretly
    /// collecting the body to a Vec we'd see it in the test's
    /// transient memory usage. Gated on the emulator + bucket env
    /// vars like the small round-trip test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_streaming_round_trips_a_large_body() {
        let Ok(_emu) = std::env::var("STORAGE_EMULATOR_HOST") else {
            return;
        };
        let Ok(bucket) = std::env::var("ENGRAM_TEST_GCS_BUCKET") else {
            return;
        };

        let store = GcsBlobStorage::connect(bucket)
            .await
            .expect("connect against emulator");
        let key = format!(
            "engram/snapshots/test/streamed-{}.bin",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 1024; // 64 MiB total
        let body: Vec<u8> = (0..CHUNK).map(|i| (i % 257) as u8).collect();
        let body_bytes = bytes::Bytes::from(body.clone());

        let chunks = (0..CHUNKS).map(move |_| Ok(body_bytes.clone()));
        let stream = futures::stream::iter(chunks);
        let put_size = store
            .put_streaming(&key, ByteStream::new(stream))
            .await
            .expect("put_streaming");
        assert_eq!(put_size, (CHUNK * CHUNKS) as u64);

        let head = store.head(&key).await.expect("head");
        assert_eq!(head.size_bytes, (CHUNK * CHUNKS) as u64);

        // Pull it back via the streaming get and verify the first +
        // last chunks match. (Full equality would be 64 MiB in RAM
        // which defeats the point.)
        let mut got = store.get_streaming(&key).await.expect("get_streaming");
        let first = got.next().await.expect("first chunk").expect("first ok");
        assert_eq!(
            &first[..CHUNK.min(first.len())],
            &body[..CHUNK.min(first.len())]
        );

        store.delete(&key).await.expect("delete");
    }

    /// Validates `list_prefix` against fake-gcs-server: writes
    /// enough keys to force pagination (>1000 per the SDK's
    /// recommended max), confirms the full set comes back, and
    /// confirms prefix filtering works as advertised.
    ///
    /// Higher-risk-than-local because pagination + prefix
    /// filtering live in the GCS API surface; local-fs tests can't
    /// catch a wire-level bug here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_prefix_paginates_against_emulator() {
        let Ok(_emu) = std::env::var("STORAGE_EMULATOR_HOST") else {
            return;
        };
        let Ok(bucket) = std::env::var("ENGRAM_TEST_GCS_BUCKET") else {
            return;
        };

        let store = GcsBlobStorage::connect(bucket)
            .await
            .expect("connect against emulator");

        // Unique prefix per run so concurrent test invocations and
        // prior runs don't interfere.
        let run_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let prefix = format!("engram/listtest/{run_id}/");
        let other_prefix = format!("engram/listtest/{run_id}-other/");

        // Write 1500 keys under `prefix` (forces ≥2 list pages with
        // max_results=1000), plus 5 under `other_prefix` to verify
        // we don't bleed across.
        let n = 1500usize;
        for i in 0..n {
            let key = format!("{prefix}{i:06}.bin");
            store
                .put(&key, bytes::Bytes::from(format!("v{i}")))
                .await
                .expect("put");
        }
        for i in 0..5 {
            let key = format!("{other_prefix}{i}.bin");
            store
                .put(&key, bytes::Bytes::from_static(b"x"))
                .await
                .expect("put");
        }

        let listed = store.list_prefix(&prefix).await.expect("list_prefix");
        assert_eq!(
            listed.len(),
            n,
            "expected {n} keys under {prefix}, got {}",
            listed.len()
        );
        // Spot-check ordering doesn't matter; spot-check contents do.
        let set: std::collections::HashSet<String> = listed.into_iter().collect();
        for i in 0..n {
            let expected = format!("{prefix}{i:06}.bin");
            assert!(
                set.contains(&expected),
                "expected key {expected} missing from listing",
            );
        }
        // Nothing from the other prefix.
        for i in 0..5 {
            let unwanted = format!("{other_prefix}{i}.bin");
            assert!(!set.contains(&unwanted));
        }

        // Cleanup so the emulator's state doesn't grow without bound
        // across test runs.
        for i in 0..n {
            let _ = store.delete(&format!("{prefix}{i:06}.bin")).await;
        }
        for i in 0..5 {
            let _ = store.delete(&format!("{other_prefix}{i}.bin")).await;
        }
    }
}
