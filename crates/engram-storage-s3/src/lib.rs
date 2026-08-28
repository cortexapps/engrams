//! S3-backed [`BlobStorage`] (ADR 0122).
//!
//! Wires `aws-sdk-s3` v1 against the trait. The `SdkConfig` comes from
//! `engram-aws` — ring-backed TLS, 5 s connect, SDK retries disabled
//! (the `BlobClient` layer owns blob retries) — so this crate holds
//! only the S3 semantics.
//!
//! Endpoint resolution:
//! - `ENGRAM_S3_ENDPOINT_URL=http://localhost:9000` → that endpoint,
//!   with path-style addressing (MinIO and other S3-compatibles
//!   don't resolve virtual-host buckets). Region defaults to
//!   `us-east-1` when the chain resolves none — emulators accept any
//!   region string, but SigV4 needs one.
//! - Anywhere else → the service's production endpoint;
//!   `ENGRAM_S3_REGION` (or the SDK chain: `AWS_REGION` → profile →
//!   IMDS) must resolve a region, and `connect` fails closed if none
//!   does.
//!
//! Streaming:
//! - `put` (body in hand — the chunk hot path) is one sized
//!   `PutObject`.
//! - `put_streaming`: the trait's `ByteStream` carries no length and
//!   S3 requires sized bodies, so frames buffer into ~16 MiB parts. A
//!   stream that ends inside the first part becomes a single
//!   `PutObject`; a longer one becomes a multipart upload with
//!   best-effort abort on any error, so no half-written destination
//!   remains (the trait contract) and no orphaned parts accumulate
//!   (belt-and-braces: deployments also carry a bucket lifecycle rule
//!   aborting stale incomplete uploads). Peak memory is one part plus
//!   one in-flight frame, never the full body. Parts are ≥ the 5 MiB
//!   S3 minimum and the scheme extends past the 5 GB single-PUT limit
//!   for free.
//! - `get_streaming` adapts the SDK's `ByteStream` frames straight
//!   into ours — no re-buffering.
//!
//! Error mapping: `NoSuchKey` (GetObject) and HeadObject's CODE-LESS
//! 404 (S3 sends no error body on HEAD, so the modeled error is
//! `NotFound` without a code — missing either breaks `exists()`) →
//! [`BlobError::NotFound`]. `DeleteObject` is natively idempotent.
//! Etags are opaque per the trait: multipart etags carry a `-N` suffix
//! and are NOT an MD5 — callers compare for equality only.

use async_trait::async_trait;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;

use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream, ListPage};

/// Multipart flush threshold. Parts are `[PART_SIZE, PART_SIZE +
/// last-frame)` bytes — comfortably above S3's 5 MiB part minimum and
/// small enough that peak buffering stays bounded. 16 MiB matches the
/// chunk store's chunk size, so a chunked upload's parts align with
/// its frames.
const PART_SIZE: usize = 16 * 1024 * 1024;

/// S3-backed storage. One client per instance, cheap to clone.
#[derive(Debug)]
pub struct S3BlobStorage {
    client: Client,
    bucket: String,
}

impl S3BlobStorage {
    /// Construct against a bucket. Honors `ENGRAM_S3_ENDPOINT_URL`
    /// (MinIO / R2 / Spaces) and `ENGRAM_S3_REGION`; otherwise the SDK
    /// default chains. Fails closed when no region can be resolved and
    /// no endpoint override is present — a mis-configured process must
    /// refuse to come up, not fail every session create.
    pub async fn connect(bucket: impl Into<String>) -> Result<Self, BlobError> {
        // Empty-but-set counts as absent: templated env
        // (`value: "{{ .region }}"` with the variable unset) renders
        // "", and `env::var(..).ok()` returns `Some("")` — which
        // would sail past the `is_none()` fail-closed guards below
        // and build a client with an empty SigV4 region (or force
        // path-style at an empty endpoint) that fails every request
        // instead of failing startup.
        let non_empty = |v: Result<String, std::env::VarError>| v.ok().filter(|s| !s.is_empty());
        let endpoint_url = non_empty(std::env::var("ENGRAM_S3_ENDPOINT_URL"));
        let region = non_empty(std::env::var("ENGRAM_S3_REGION"));
        let has_endpoint_override = endpoint_url.is_some();

        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region,
            endpoint_url,
        })
        .await;

        let mut builder = aws_sdk_s3::config::Builder::from(&cfg);
        if has_endpoint_override {
            // Emulators and S3-compatibles serve buckets by path, not
            // by virtual host (`localhost:9000/bucket/key`, not
            // `bucket.localhost:9000`).
            builder = builder.force_path_style(true);
            if cfg.region().is_none() {
                builder = builder.region(aws_sdk_s3::config::Region::new("us-east-1"));
            }
        } else if cfg.region().is_none() {
            return Err(BlobError::Config(
                "no AWS region resolved; set ENGRAM_S3_REGION or AWS_REGION".into(),
            ));
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: bucket.into(),
        })
    }

    /// Upload one buffered part and return its completed-part record.
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        body: Bytes,
    ) -> Result<CompletedPart, BlobError> {
        let out = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(body.into())
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(CompletedPart::builder()
            .part_number(part_number)
            .set_e_tag(out.e_tag)
            .build())
    }

    /// Best-effort abort so a failed multipart upload leaves neither a
    /// half-written destination nor billable orphaned parts.
    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        if let Err(e) = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            tracing::warn!(key = %key, error = %DisplayErrorContext(&e), "s3: abort_multipart_upload failed");
        }
    }
}

/// Accumulates stream frames and cuts a part once the threshold is
/// reached. Pure buffering math, kept SDK-free so the boundary cases
/// (exact threshold, threshold+1, empty) unit-test without a wire.
struct PartAccumulator {
    pending: Vec<Bytes>,
    pending_len: usize,
    threshold: usize,
}

impl PartAccumulator {
    fn new(threshold: usize) -> Self {
        Self {
            pending: Vec::new(),
            pending_len: 0,
            threshold,
        }
    }

    /// Buffer one frame; returns a full part when the threshold is
    /// reached. A returned part is at least `threshold` bytes and at
    /// most `threshold + frame.len() - 1`.
    fn push(&mut self, frame: Bytes) -> Option<Bytes> {
        if !frame.is_empty() {
            self.pending_len += frame.len();
            self.pending.push(frame);
        }
        if self.pending_len >= self.threshold {
            Some(self.take())
        } else {
            None
        }
    }

    /// Drain whatever remains (possibly empty — the zero-byte object).
    fn finish(&mut self) -> Bytes {
        self.take()
    }

    fn take(&mut self) -> Bytes {
        if self.pending.len() == 1 {
            self.pending_len = 0;
            return self.pending.pop().expect("len checked");
        }
        let mut buf = BytesMut::with_capacity(self.pending_len);
        for frame in self.pending.drain(..) {
            buf.extend_from_slice(&frame);
        }
        self.pending_len = 0;
        buf.freeze()
    }
}

/// Wrap any SDK error as [`BlobError::Sdk`], keeping the full context
/// chain (`DisplayErrorContext` renders service code + message).
fn sdk_err<E, R>(err: SdkError<E, R>) -> BlobError
where
    E: std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    BlobError::Sdk(err.into_service_error_or_self())
}

/// The SDK's error Display shows only the outermost layer; this
/// mirrors `aws_smithy_types::error::display::DisplayErrorContext`
/// without pulling the extra import at every call site.
struct DisplayErrorContext<E>(E);

impl<E: std::error::Error> std::fmt::Display for DisplayErrorContext<&E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(s) = source {
            write!(f, ": {s}")?;
            source = s.source();
        }
        Ok(())
    }
}

/// Extension: flatten an `SdkError` to its modeled service error when
/// there is one (retaining the useful message), or keep the transport
/// error otherwise.
trait IntoServiceErrorOrSelf<E, R> {
    fn into_service_error_or_self(self) -> Box<dyn std::error::Error + Send + Sync>;
}

impl<E, R> IntoServiceErrorOrSelf<E, R> for SdkError<E, R>
where
    E: std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    fn into_service_error_or_self(self) -> Box<dyn std::error::Error + Send + Sync> {
        match self {
            SdkError::ServiceError(ctx) => Box::new(ctx.into_err()),
            other => Box::new(other),
        }
    }
}

#[async_trait]
impl BlobStorage for S3BlobStorage {
    async fn put_streaming(&self, key: &str, mut body: ByteStream) -> Result<u64, BlobError> {
        let mut acc = PartAccumulator::new(PART_SIZE);
        let mut total: u64 = 0;
        let mut upload_id: Option<String> = None;
        let mut parts: Vec<CompletedPart> = Vec::new();

        // Drive the stream, flushing a part whenever the accumulator
        // fills. The multipart upload is created lazily at the first
        // flush so short bodies stay on the single-PUT path.
        loop {
            let frame = match body.next().await {
                Some(Ok(frame)) => frame,
                Some(Err(e)) => {
                    if let Some(id) = &upload_id {
                        self.abort_multipart(key, id).await;
                    }
                    return Err(e);
                }
                None => break,
            };
            total += frame.len() as u64;
            if let Some(part) = acc.push(frame) {
                let id = match &upload_id {
                    Some(id) => id.clone(),
                    None => {
                        let out = self
                            .client
                            .create_multipart_upload()
                            .bucket(&self.bucket)
                            .key(key)
                            .send()
                            .await
                            .map_err(sdk_err)?;
                        let id = out.upload_id.ok_or_else(|| {
                            BlobError::Protocol(
                                "CreateMultipartUpload returned no upload id".into(),
                            )
                        })?;
                        upload_id = Some(id.clone());
                        id
                    }
                };
                let part_number = parts.len() as i32 + 1;
                match self.upload_part(key, &id, part_number, part).await {
                    Ok(completed) => parts.push(completed),
                    Err(e) => {
                        self.abort_multipart(key, &id).await;
                        return Err(e);
                    }
                }
            }
        }

        let tail = acc.finish();
        match upload_id {
            // Everything fit below one part: single sized PutObject
            // (also the zero-byte-object path).
            None => {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(tail.into())
                    .send()
                    .await
                    .map_err(sdk_err)?;
            }
            Some(id) => {
                if !tail.is_empty() {
                    let part_number = parts.len() as i32 + 1;
                    match self.upload_part(key, &id, part_number, tail).await {
                        Ok(completed) => parts.push(completed),
                        Err(e) => {
                            self.abort_multipart(key, &id).await;
                            return Err(e);
                        }
                    }
                }
                let complete = self
                    .client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts))
                            .build(),
                    )
                    .send()
                    .await;
                if let Err(e) = complete {
                    self.abort_multipart(key, &id).await;
                    return Err(sdk_err(e));
                }
            }
        }
        tracing::debug!(key = %key, bytes = total, "s3 blob put");
        Ok(total)
    }

    /// Sized single `PutObject` — the chunk hot path skips the
    /// accumulator entirely.
    async fn put(&self, key: &str, body: Bytes) -> Result<u64, BlobError> {
        let len = body.len() as u64;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body.into())
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(len)
    }

    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| match &e {
                SdkError::ServiceError(ctx) if ctx.err().is_no_such_key() => BlobError::NotFound,
                SdkError::ServiceError(ctx) if ctx.raw().status().as_u16() == 404 => {
                    BlobError::NotFound
                }
                _ => sdk_err(e),
            })?;
        // Adapt the SDK body stream frame-by-frame; no re-buffering.
        let stream = futures::stream::unfold(out.body, |mut body| async move {
            match body.try_next().await {
                Ok(Some(frame)) => Some((Ok(frame), body)),
                Ok(None) => None,
                Err(e) => Some((Err(BlobError::Sdk(Box::new(e))), body)),
            }
        });
        Ok(ByteStream::new(stream))
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| match &e {
                // HeadObject's 404 has NO error body, so the modeled
                // error is `NotFound` (code-less), never `NoSuchKey`.
                SdkError::ServiceError(ctx) if ctx.err().is_not_found() => BlobError::NotFound,
                SdkError::ServiceError(ctx) if ctx.raw().status().as_u16() == 404 => {
                    BlobError::NotFound
                }
                _ => sdk_err(e),
            })?;
        let size_bytes = out
            .content_length
            .and_then(|l| u64::try_from(l).ok())
            .ok_or_else(|| BlobError::Protocol("HeadObject returned no content length".into()))?;
        Ok(BlobObjectMeta {
            size_bytes,
            etag: out.e_tag,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        // DeleteObject is natively idempotent: a missing key returns
        // 204, not an error.
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(sdk_err)?;
        Ok(())
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self.client.list_objects_v2().bucket(&self.bucket);
            if !prefix.is_empty() {
                req = req.prefix(prefix);
            }
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req.send().await.map_err(sdk_err)?;
            for obj in resp.contents() {
                if let Some(key) = obj.key() {
                    out.push(key.to_string());
                }
            }
            match resp.next_continuation_token {
                Some(t) if !t.is_empty() => continuation = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Native single-page listing: one `ListObjectsV2` round-trip, the
    /// S3 continuation token carried through verbatim as the cursor.
    /// The whole-listing `list_prefix` above puts every page under one
    /// client deadline, which does not survive an unbounded prefix —
    /// the GCS deployment hit exactly that on the chunk space.
    async fn list_prefix_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ListPage, BlobError> {
        let mut req = self.client.list_objects_v2().bucket(&self.bucket);
        if !prefix.is_empty() {
            req = req.prefix(prefix);
        }
        if let Some(token) = cursor {
            req = req.continuation_token(token);
        }
        // S3 caps a page at 1000 regardless of what we ask for.
        req = req.max_keys(limit.clamp(1, 1000) as i32);
        let resp = req.send().await.map_err(sdk_err)?;
        let keys = resp
            .contents()
            .iter()
            .filter_map(|obj| obj.key().map(str::to_string))
            .collect();
        let next = match resp.next_continuation_token {
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

    /// Pins the rustls CryptoProvider contract for this crate: the
    /// mirror of `engram-storage-gcs::connect_installs_a_crypto_provider`
    /// and `engram-aws`'s own pin. Constructs the full client stack —
    /// which eagerly builds the ring-backed TLS connector — with no
    /// preinstalled provider. Endpoint at a dead port + static env
    /// creds: never dials, runs everywhere.
    #[tokio::test(flavor = "current_thread")]
    async fn connect_installs_a_crypto_provider() {
        // nextest runs one process per test, so this cannot race a
        // sibling's view of the environment.
        std::env::set_var("ENGRAM_S3_ENDPOINT_URL", "http://127.0.0.1:1");
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");

        S3BlobStorage::connect("engram-provider-contract")
            .await
            .expect("connect must build a TLS client without a preinstalled provider");
    }

    /// Without an endpoint override, a process that resolves no region
    /// must refuse to come up (fail-closed, mirroring `from_env`).
    /// The second half pins the empty-but-SET case: templated env
    /// renders `""` when the source variable is unset, and
    /// `Some("")` must collapse to absent — not slip past the
    /// `is_none()` guards into an empty SigV4 region.
    #[tokio::test(flavor = "current_thread")]
    async fn connect_without_region_fails_closed() {
        std::env::remove_var("ENGRAM_S3_ENDPOINT_URL");
        std::env::remove_var("ENGRAM_S3_REGION");
        std::env::remove_var("AWS_REGION");
        std::env::remove_var("AWS_DEFAULT_REGION");
        // No profile/IMDS in the test environment resolves one either:
        // point the profile chain at an empty file to be sure.
        std::env::set_var("AWS_CONFIG_FILE", "/dev/null");
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", "/dev/null");
        std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");

        let err = S3BlobStorage::connect("bucket")
            .await
            .expect_err("no region must fail closed");
        assert!(matches!(err, BlobError::Config(_)));

        // Empty-but-set region + endpoint behave exactly like unset.
        std::env::set_var("ENGRAM_S3_REGION", "");
        std::env::set_var("ENGRAM_S3_ENDPOINT_URL", "");
        let err = S3BlobStorage::connect("bucket")
            .await
            .expect_err("empty-string region must fail closed too");
        assert!(matches!(err, BlobError::Config(_)));
    }

    // ---- PartAccumulator boundary math (SDK-free) ----

    fn frame(len: usize) -> Bytes {
        Bytes::from(vec![0xAB; len])
    }

    #[test]
    fn accumulator_below_threshold_returns_nothing() {
        let mut acc = PartAccumulator::new(10);
        assert!(acc.push(frame(9)).is_none());
        assert_eq!(acc.finish().len(), 9);
    }

    #[test]
    fn accumulator_exact_threshold_cuts_a_part() {
        let mut acc = PartAccumulator::new(10);
        let part = acc.push(frame(10)).expect("exact threshold flushes");
        assert_eq!(part.len(), 10);
        assert!(acc.finish().is_empty(), "nothing left after the cut");
    }

    #[test]
    fn accumulator_overshoot_rides_in_one_part() {
        let mut acc = PartAccumulator::new(10);
        assert!(acc.push(frame(6)).is_none());
        let part = acc.push(frame(11)).expect("threshold crossed");
        assert_eq!(part.len(), 17, "part carries the whole overshoot frame");
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn accumulator_empty_stream_finishes_empty() {
        let mut acc = PartAccumulator::new(10);
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn accumulator_ignores_empty_frames() {
        let mut acc = PartAccumulator::new(10);
        assert!(acc.push(Bytes::new()).is_none());
        assert!(acc.push(frame(4)).is_none());
        assert_eq!(acc.finish().len(), 4);
    }

    // ---- Emulator-gated conformance (MinIO) ----

    /// Connects to the MinIO emulator, or returns `None` when the
    /// gating env is absent so the caller no-ops. Creates the bucket
    /// idempotently — no seed-script dependency. Run locally via
    /// `docker compose -f deploy/docker-compose.dev.yml --profile local-s3 up -d minio && \
    ///  ENGRAM_TEST_S3_ENDPOINT=http://localhost:9000 \
    ///  ENGRAM_TEST_S3_BUCKET=engram-snapshots-test \
    ///  cargo nextest run -p engram-storage-s3`.
    async fn emulator_store() -> Option<S3BlobStorage> {
        let endpoint = std::env::var("ENGRAM_TEST_S3_ENDPOINT").ok()?;
        let bucket = std::env::var("ENGRAM_TEST_S3_BUCKET").ok()?;
        std::env::set_var("ENGRAM_S3_ENDPOINT_URL", &endpoint);
        // Static credentials so the chain never probes IMDS from CI;
        // minioadmin/minioadmin is the MinIO default root user.
        if std::env::var("AWS_ACCESS_KEY_ID").is_err() {
            std::env::set_var("AWS_ACCESS_KEY_ID", "minioadmin");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "minioadmin");
        }
        let store = S3BlobStorage::connect(bucket.clone())
            .await
            .expect("connect against emulator");
        match store.client.create_bucket().bucket(&bucket).send().await {
            Ok(_) => {}
            Err(SdkError::ServiceError(ctx))
                if ctx.err().is_bucket_already_owned_by_you()
                    || ctx.err().is_bucket_already_exists() => {}
            Err(e) => panic!("create test bucket: {e}"),
        }
        Some(store)
    }

    // The shared trait-contract suite (`engram_testkit::blob_conformance`)
    // against the real SDK→MinIO wire path. The large-body run crosses
    // the 16 MiB part threshold, so multipart assembly (create → parts
    // → complete) is exercised end to end; the small runs stay on the
    // single-PUT path.

    #[tokio::test(flavor = "current_thread")]
    async fn conformance_round_trip_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::round_trip(&store).await;
    }

    /// 64 MiB in 64 KiB chunks → 4 multipart parts. Verifies size
    /// accounting and reassembly across the part boundary.
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

    /// The failing stream errors before the part threshold, so this
    /// exercises the single-PUT abort path (nothing sent). The
    /// multipart abort path is covered by
    /// `mid_stream_failure_past_part_boundary_aborts_multipart` below.
    #[tokio::test(flavor = "current_thread")]
    async fn conformance_failed_put_preserves_prior_blob_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::failed_streaming_put_preserves_prior_blob(&store).await;
    }

    /// 1500 keys forces ≥2 ListObjectsV2 pages (max 1000/page).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conformance_list_prefix_paginates_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::list_prefix_scoped(&store, 1500).await;
    }

    /// The paged walk must agree with the whole listing — the surface
    /// the chunk-GC mark pass uses, over S3's native continuation token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conformance_list_prefix_page_walks_whole_prefix_against_emulator() {
        let Some(store) = emulator_store().await else {
            return;
        };
        blob_conformance::list_prefix_page_walks_whole_prefix(&store, 350, 100).await;
    }

    /// S3-specific atomicity: fail the stream AFTER a part has been
    /// uploaded, so the in-flight multipart upload must be aborted and
    /// the prior object must survive. (The shared conformance scenario
    /// fails before the first part and never reaches multipart.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mid_stream_failure_past_part_boundary_aborts_multipart() {
        let Some(store) = emulator_store().await else {
            return;
        };
        let key = format!(
            "{}/mid-stream.bin",
            blob_conformance::unique_prefix("s3-abort")
        );

        store
            .put(&key, Bytes::from_static(b"prior"))
            .await
            .expect("initial put");

        // One full part (17 MiB > PART_SIZE), then an error.
        let big = Bytes::from(vec![0x5A; PART_SIZE + 1024 * 1024]);
        let chunks: Vec<Result<Bytes, BlobError>> = vec![
            Ok(big),
            Err(BlobError::Protocol("simulated failure after part 1".into())),
        ];
        let res = store
            .put_streaming(&key, ByteStream::new(futures::stream::iter(chunks)))
            .await;
        assert!(res.is_err(), "mid-stream failure must surface");

        let got = store.get(&key).await.expect("get after failed put");
        assert_eq!(&got[..], b"prior", "prior object must survive the abort");

        // The upload was aborted: no in-progress multipart uploads
        // remain for this key.
        let uploads = store
            .client
            .list_multipart_uploads()
            .bucket(&store.bucket)
            .prefix(&key)
            .send()
            .await
            .expect("list_multipart_uploads");
        assert!(
            uploads.uploads().is_empty(),
            "aborted upload must not linger: {:?}",
            uploads.uploads()
        );

        store.delete(&key).await.expect("delete");
    }
}
