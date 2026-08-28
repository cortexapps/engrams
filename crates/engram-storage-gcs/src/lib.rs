//! GCS-backed [`BlobStorage`].
//!
//! Wires `gcloud-storage` v1.3 against the trait — the crate that was
//! published as `google-cloud-storage` up to 0.24, before that name moved
//! to Google's official SDK (see the manifest note on the rename).
//! Application Default Credentials (ADC) by default; emulator support via
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
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream, ListPage};
use futures::{SinkExt, StreamExt};
use gcloud_storage::client::{Client, ClientConfig};
use gcloud_storage::http::objects::delete::DeleteObjectRequest;
use gcloud_storage::http::objects::download::Range;
use gcloud_storage::http::objects::get::GetObjectRequest;
use gcloud_storage::http::objects::list::ListObjectsRequest;
use gcloud_storage::http::objects::upload::{Media, UploadObjectRequest, UploadType};
use gcloud_storage::http::Error as GcsHttpError;

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
    /// Note: `gcloud-storage` does not auto-honor
    /// `STORAGE_EMULATOR_HOST` (`grep -rn STORAGE_EMULATOR_HOST` in
    /// the SDK turns up only a `// TODO emulator support` note). We
    /// thread it through explicitly here.
    pub async fn connect(bucket: impl Into<String>) -> Result<Self, BlobError> {
        // `gcloud-auth` builds its own client inside `auth_config` to fetch
        // ADC tokens, so the process provider has to be installed before
        // that call — not just before the tuned client below.
        engram_tls::install_provider();
        let mut cfg = Self::auth_config().await?;
        // Inject our own transport instead of the SDK's untuned default
        // (per TigerBeetle's object-storage-client findings, 2026-07):
        // - bounded connect: a blackholed endpoint fails in 5 s and
        //   surfaces to the BlobClient retry layer, instead of pinning
        //   an attempt for the OS default (minutes).
        // - sized keep-alive pool: the sparse re-chunk and NBD flush
        //   fan out dozens of concurrent chunk ops; idle-connection
        //   reuse keeps those off the TLS-handshake path — and keeps
        //   DNS off the hot path entirely, which is why hickory async
        //   DNS was tried and RETIRED here: reqwest's `hickory-dns`
        //   feature unifies workspace-wide and flips the DEFAULT
        //   resolver for every reqwest client off getaddrinfo
        //   (different ndots/search/hosts semantics under the k8s
        //   fleet's resolv.conf) — the same silent-global reach as
        //   the rejected `http2` feature below.
        // No global request timeout here — bodies are GB-scale on the
        // streaming paths; per-attempt deadlines live in BlobClient.
        //
        // Protocol: pooled HTTP/1.1, unconditionally — bulk parallel
        // chunk transfers get one TCP window each instead of sharing
        // one h2 connection's flow control. HTTP/2 was measured and
        // REJECTED: blobbench (2026-07-31, n2-standard-16 → us-west2
        // GCS, 16 MiB objects, c=32) put ALPN h2 at 4-5x worse on
        // bulk transfer (GET 1914 → 378 MiB/s, PUT 1497 → 474
        // MiB/s), so reqwest's `http2` feature is deliberately NOT
        // enabled anywhere in the workspace — feature unification
        // would silently flip every unpinned reqwest client
        // (engram-oci's registry pulls included) to h2. The
        // `http1_only()` here is belt-and-braces against the feature
        // ever arriving transitively; to re-test h2, re-run blobbench
        // with the feature enabled rather than trusting this number.
        let http = engram_tls::client_builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .http1_only()
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
        // No tuned client here, so the SDK builds its own — same reason as
        // `connect` that this has to happen first.
        engram_tls::install_provider();
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

    /// Native single-page listing: one `list_objects` round-trip, the
    /// GCS `pageToken` carried through verbatim as the cursor. This is
    /// what lets a caller walk a prefix of unbounded size — the whole-
    /// listing `list_prefix` above cannot, because every page shares one
    /// client deadline.
    async fn list_prefix_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ListPage, BlobError> {
        let req = ListObjectsRequest {
            bucket: self.bucket.clone(),
            prefix: if prefix.is_empty() {
                None
            } else {
                Some(prefix.to_string())
            },
            // GCS caps a page at 1000 regardless of what we ask for.
            max_results: Some(limit.clamp(1, 1000) as i32),
            page_token: cursor.map(str::to_string),
            ..Default::default()
        };
        let resp = self.client.list_objects(&req).await.map_err(map_http_err)?;
        let keys = resp
            .items
            .unwrap_or_default()
            .into_iter()
            .map(|obj| obj.name)
            .collect();
        let next = match resp.next_page_token {
            Some(t) if !t.is_empty() => Some(t),
            _ => None,
        };
        Ok(ListPage { keys, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use engram_testkit::blob_conformance;

    /// Pins the rustls CryptoProvider contract for this crate's clients.
    ///
    /// `connect()` reaches TLS twice — the tuned client it builds, and the
    /// one `gcloud-auth` builds internally — and the workspace compiles
    /// reqwest with `rustls-no-provider`, under which `build()` panics
    /// until a process-level provider is installed (see `engram-tls`).
    /// Without the install this fails with "No rustls crypto provider is
    /// configured" and every GCS operation fails at construction.
    ///
    /// `engram-tls` owns the install and has its own unit pin, but this
    /// test is what covers THIS crate's two entry points: `connect` calls
    /// `install_provider` explicitly for the SDK's internal client, and a
    /// future edit could drop that line without any other test noticing.
    /// The emulator tests below cannot catch it — they return early when
    /// `STORAGE_EMULATOR_HOST` is unset, which is the case on any host
    /// without docker. This one sets the variable itself, so it runs
    /// everywhere and needs no network: anonymous auth against a dead port
    /// never dials.
    #[tokio::test(flavor = "current_thread")]
    async fn connect_installs_a_crypto_provider() {
        // nextest runs one process per test, so this cannot race a
        // sibling's view of the environment.
        std::env::set_var("STORAGE_EMULATOR_HOST", "http://127.0.0.1:1");

        GcsBlobStorage::connect("engram-provider-contract")
            .await
            .expect("connect must build a TLS client without a preinstalled provider");
    }

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

    /// Connects to the live emulator, or returns `None` when the
    /// gating env is absent so the caller no-ops on hosts without
    /// docker. Run the gated tests locally via
    /// `bash deploy/dev/seed-buckets.sh && \
    ///  ENGRAM_TEST_GCS_BUCKET=engram-snapshots-test \
    ///  STORAGE_EMULATOR_HOST=http://localhost:4443 \
    ///  cargo nextest run -p engram-storage-gcs`.
    async fn emulator_store() -> Option<GcsBlobStorage> {
        std::env::var("STORAGE_EMULATOR_HOST").ok()?;
        let bucket = std::env::var("ENGRAM_TEST_GCS_BUCKET").ok()?;
        Some(
            GcsBlobStorage::connect(bucket)
                .await
                .expect("connect against emulator"),
        )
    }

    // The scenarios below are the shared trait-contract suite
    // (`engram_testkit::blob_conformance`), run here against the GCS
    // wire path. Higher-risk than the local backend's runs because
    // pagination, resumable uploads, and NotFound mapping live in the
    // GCS API surface — a local-fs test can't catch a wire-level bug.

    #[tokio::test(flavor = "current_thread")]
    async fn conformance_round_trip_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::round_trip(&store).await;
    }

    /// 64 MiB in 64 KiB chunks: exercises the upload pipeline across
    /// ~1024 chunks — if anything secretly collected the body into a
    /// Vec we'd see it in the test's transient memory usage.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conformance_streaming_large_body_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::streaming_round_trip(&store, 64 * 1024, 1024).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conformance_zero_byte_put_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::zero_byte_put(&store).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conformance_failed_put_preserves_prior_blob_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::failed_streaming_put_preserves_prior_blob(&store).await;
    }

    /// 1500 keys forces ≥2 list pages with `max_results = 1000`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conformance_list_prefix_paginates_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::list_prefix_scoped(&store, 1500).await;
    }

    /// The paged walk must agree with the whole listing. This is the
    /// surface the chunk-GC mark pass uses, and GCS is the backend whose
    /// native `pageToken` pagination it exercises.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conformance_list_prefix_page_walks_whole_prefix_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::list_prefix_page_walks_whole_prefix(&store, 350, 100).await;
    }
}
