//! Cold-tier blob storage for snapshot durability.
//!
//! Hot-tier snapshots (FC `memory.bin` + state file on Linux, APFS
//! rootfs clone on VZ) live on local NVMe and serve same-host hot
//! resume — sub-second via UFFD lazy paging. Cold-tier snapshots are
//! the same payload tar+zstd-compressed and pushed to a `BlobStorage`
//! backend; they survive host loss, and resume is a download +
//! `SandboxBackend::restore` on whatever host the scheduler picks.
//!
//! The trait is **streaming on both directions** because FC `memory.bin`
//! is GB-scale. Collecting a multi-GB body into `Vec<u8>` would OOM on
//! multi-session hosts, so `put_streaming` and `get_streaming` model
//! the body as a `ByteStream` newtype over
//! `Stream<Item = Result<Bytes, BlobError>>`.
//!
//! Backends:
//! - `engram-storage-local` — fs-backed, used for dev/test.
//! - `engram-storage-s3` — `aws-sdk-s3` v1, region/endpoint env-var
//!   overridable so it works against MinIO + R2 + the local emulator.
//! - `engram-storage-gcs` — `google-cloud-storage`, ADC + resumable
//!   uploads. Honors `STORAGE_EMULATOR_HOST` for `fake-gcs-server`.
//!
//! See [ADR 0005](../../../../../docs/adr/0005-disk-pressure-blob-tier.md)
//! for the design + the ADR 0001 supersession context.

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::Stream;

use crate::error::BlobError;

/// A streaming body — used both as the ingest path for `put_streaming`
/// and the egress path of `get_streaming`. Newtype so the trait shape
/// stays readable and so the `Send + 'static` bounds are stated once.
pub struct ByteStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, BlobError>> + Send + 'static>>,
}

impl ByteStream {
    /// Wrap any compatible stream.
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, BlobError>> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
        }
    }

    /// Wrap an in-memory body. Convenient for small payloads + tests.
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self::new(futures::stream::once(async move { Ok(bytes) }))
    }

    /// Wrap a `Vec<u8>` body.
    pub fn from_vec(v: Vec<u8>) -> Self {
        Self::from_bytes(Bytes::from(v))
    }
}

impl Stream for ByteStream {
    type Item = Result<Bytes, BlobError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Object metadata returned by `head` — analogous to `S3 HeadObject`
/// or `GCS Objects: get` without body. Cheap; no body transfer.
#[derive(Clone, Debug)]
pub struct BlobObjectMeta {
    pub size_bytes: u64,
    /// Backend-specific entity tag (S3 ETag, GCS generation, fs mtime
    /// stringified). Opaque; callers compare for equality only.
    pub etag: Option<String>,
}

/// Pluggable blob/object-storage backend for cold-tier snapshots.
///
/// Implementations are responsible for:
/// - Streaming both directions; never materialize bodies in RAM.
/// - Mapping backend NotFound (404 / NoSuchKey) into
///   `BlobError::NotFound`; idempotent delete on missing keys.
/// - All other errors surface as `BlobError::Sdk` with the underlying
///   error in the source chain.
#[async_trait]
pub trait BlobStorage: Send + Sync {
    /// Upload a body. Returns the size written. The implementation
    /// **drains** the entire `body` stream before returning success;
    /// partial uploads must surface as errors and not leave the
    /// destination half-written (best-effort: S3/GCS multipart
    /// failures abort the upload; local fs writes to a tempfile and
    /// renames).
    async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError>;

    /// Convenience for small in-memory payloads (sealed blob refs,
    /// manifests, test bodies). Implementations forward to
    /// `put_streaming`.
    async fn put(&self, key: &str, body: Bytes) -> Result<u64, BlobError> {
        self.put_streaming(key, ByteStream::from_bytes(body)).await
    }

    /// Fetch a body as a streaming response. Errors with
    /// `BlobError::NotFound` if the key doesn't exist.
    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError>;

    /// Convenience for small payloads. Drains the streaming body.
    /// Don't use on snapshot-sized bodies — it OOMs at scale.
    ///
    /// Collects frames first and coalesces once at exact size (a
    /// single-frame body is returned as-is, zero-copy) — the naive
    /// `BytesMut::new()` + extend loop paid amortized-doubling
    /// realloc copies on every 16 MiB chunk GET, measurable CPU on
    /// the blob tier's hottest read path.
    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        use futures::StreamExt;
        let mut stream = self.get_streaming(key).await?;
        let mut frames: Vec<Bytes> = Vec::new();
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            let frame = chunk?;
            if !frame.is_empty() {
                total += frame.len();
                frames.push(frame);
            }
        }
        if frames.len() == 1 {
            return Ok(frames.remove(0));
        }
        let mut buf = bytes::BytesMut::with_capacity(total);
        for frame in frames {
            buf.extend_from_slice(&frame);
        }
        Ok(buf.freeze())
    }

    /// Object metadata without body transfer.
    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError>;

    /// Existence check. Defaults to `head` + ignore-NotFound.
    async fn exists(&self, key: &str) -> Result<bool, BlobError> {
        match self.head(key).await {
            Ok(_) => Ok(true),
            Err(BlobError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Delete a key. Idempotent — succeeds if the key was already
    /// gone. Implementations map backend 404 into success.
    async fn delete(&self, key: &str) -> Result<(), BlobError>;

    /// List keys under a prefix. Returns the full keys (not
    /// relative to the prefix). The order is backend-specific —
    /// callers that need a stable order should sort.
    ///
    /// Used by:
    /// - `engram-chunk-store::gc` to enumerate live chunks and
    ///   manifest versions during the sweep.
    /// - `engram-chunk-store::store::latest_manifest_version` to
    ///   find the highest version of a manifest without the
    ///   exponential-probe fallback.
    /// - Image-builder + image-cache utilities that need to walk
    ///   over a repo's tags.
    ///
    /// Implementations should paginate transparently — the result
    /// is the full set, not a single page. Empty prefix is
    /// allowed (lists everything); callers should be cautious
    /// about using that against large buckets.
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError>;
}
