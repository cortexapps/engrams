//! THE blob-tier client.
//!
//! Every binary that touches the blob tier (coordinator, host-agent,
//! uffd-handler) constructs it through [`from_env`], and every blob
//! round-trip flows through [`BlobClient`] — one encapsulated place
//! for backend selection, bounded retry, per-attempt deadlines, and
//! metrics. Call sites keep programming against the
//! [`BlobStorage`] trait; the client IS a `BlobStorage`.
//!
//! ## Why (2026-07-22 incident + prior art)
//!
//! A single reqwest `error decoding response body` (connection reset
//! mid-body) on ONE chunk GET during a diff capture's sparse re-chunk
//! surfaced raw to the checkpoint driver — after FC had already
//! consumed the KVM dirty bitmap — so the checkpoint chain was
//! poisoned and the next capture paid a Full snapshot. The
//! `google-cloud-storage` SDK performs no retries and imposes no
//! deadlines of its own; every durability leg and guest-blocking read
//! was one network blip away from its worst-case failure path.
//!
//! The shape here follows TigerBeetle's object-storage-client work
//! (July 2026): don't scatter transport policy across call sites and
//! SDK internals — own one client, make every wait bounded, and keep
//! the whole stack simulable. We adopt what pays in a Rust/tokio
//! deployment (single seam, bounded retry + deadlines, tuned
//! connection pool, fault-injection tests against the same trait
//! the simulator drives) and skip what doesn't (an
//! own-TLS/HTTP/io_uring stack is not where this system's blob-tier
//! ceiling is; hickory async DNS and h2 were both measured/reasoned
//! out — see `engram-storage-gcs`).
//!
//! ## Layering
//!
//! - **Deadline**: each attempt of a unary op runs under
//!   [`ClientPolicy::attempt_timeout`] (`list_prefix` gets the
//!   separate, generous [`ClientPolicy::list_timeout`] — one attempt
//!   is a full paginated enumeration, and GC sweeps list large
//!   prefixes). A hung TCP connection becomes a retryable error, not
//!   a wedged capture leg.
//! - **Bounded retry**: transient failures ([`BlobError::Sdk`] /
//!   [`BlobError::Protocol`] / a deadline) are retried with
//!   deterministic exponential backoff. `NotFound`, `Config`, and
//!   `Io` (local-fs ENOSPC and friends) surface immediately. Every
//!   blob op is idempotent (content-addressed chunks, versioned
//!   manifests), so retry is unconditionally safe.
//! - **Streaming caveats**: `put_streaming` is forwarded verbatim —
//!   the inbound stream is consumed by the first attempt and cannot
//!   be replayed here (streaming callers own their resumability, as
//!   `engram-oci` does with ranged layer pulls). `get_streaming`
//!   retries the initial call only; the returned body is the
//!   caller's. The buffered `get`/`put` — the chunk path — get full
//!   coverage: `get` re-issues from scratch, so a mid-body reset
//!   inside the collect loop (the incident shape) is absorbed.
//! - **Metrics**: `engram_blob_op_seconds{op,outcome}` (whole-op wall
//!   time including retries), `engram_blob_retry_total{op}`,
//!   `engram_blob_retry_exhausted_total{op}`,
//!   `engram_blob_attempt_timeout_total{op}`.
//!
//! ## Testing
//!
//! The client wraps `Arc<dyn BlobStorage>`, so
//! `engram_testkit::storage::FaultyBlobStorage` scripts the exact
//! failure shapes underneath it and engram-sim's blob can stand in
//! for the backend — the retry/deadline layer itself stays under
//! deterministic test, per the same discipline as the rest of the
//! stack (ADR 0098/0099).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream};

/// Whether an error is worth another attempt. `Sdk` carries the SDK
/// backends' transport failures (they fold IO errors into it);
/// `Protocol` is a garbled response (an LB hiccup). A permanent error
/// wearing `Sdk` clothing (403, 412) costs at most `attempts - 1`
/// extra round-trips before surfacing — cheap next to the Full
/// snapshot a poisoned chain costs.
fn is_transient(e: &BlobError) -> bool {
    matches!(e, BlobError::Sdk(_) | BlobError::Protocol(_))
}

/// Attempt budget, backoff shape, and per-attempt deadlines.
/// `attempts` is the TOTAL number of tries (`3` = 1 initial + 2
/// retries); the delay before retry `n` is `base_backoff * 2^(n-1)`.
/// Backoff is deterministic (no jitter) so behavior is identical
/// under simulation; at this fleet's blob-op rate herd effects are
/// not a factor.
#[derive(Clone, Copy, Debug)]
pub struct ClientPolicy {
    pub attempts: u32,
    pub base_backoff: Duration,
    /// Deadline per attempt for unary ops (`get`/`put`/`head`/
    /// `delete` and `get_streaming`'s initial call). Sized for the
    /// worst honest case — a cold 16 MiB chunk on a congested NIC —
    /// not the median (~100 ms).
    pub attempt_timeout: Duration,
    /// Deadline per `list_prefix` attempt. One attempt paginates the
    /// entire prefix (GC sweeps enumerate thousands of keys), so this
    /// bounds a *sweep*, not a round-trip.
    pub list_timeout: Duration,
}

impl Default for ClientPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_backoff: Duration::from_millis(100),
            attempt_timeout: Duration::from_secs(30),
            list_timeout: Duration::from_secs(300),
        }
    }
}

impl ClientPolicy {
    fn backoff_for(&self, retry: u32) -> Duration {
        self.base_backoff
            .saturating_mul(1u32 << (retry - 1).min(16))
    }
}

/// The encapsulated blob-tier client. Construct via [`from_env`] in
/// binaries, [`BlobClient::wrap`]/[`BlobClient::with_policy`] in
/// tests and the simulator.
pub struct BlobClient {
    inner: Arc<dyn BlobStorage>,
    policy: ClientPolicy,
}

impl BlobClient {
    /// Wrap `inner` with the default policy.
    pub fn wrap(inner: Arc<dyn BlobStorage>) -> Self {
        Self::with_policy(inner, ClientPolicy::default())
    }

    /// Wrap `inner` with an explicit policy (tests use ~0 backoff and
    /// short deadlines).
    pub fn with_policy(inner: Arc<dyn BlobStorage>, policy: ClientPolicy) -> Self {
        Self { inner, policy }
    }

    /// Run one deadline-bounded attempt. `deadline = None` forwards
    /// unbounded (streaming bodies).
    async fn attempt<T, Fut>(
        op: &'static str,
        deadline: Option<Duration>,
        fut: Fut,
    ) -> Result<T, BlobError>
    where
        Fut: Future<Output = Result<T, BlobError>>,
    {
        match deadline {
            None => fut.await,
            Some(d) => match tokio::time::timeout(d, fut).await {
                Ok(r) => r,
                Err(_) => {
                    metrics::counter!("engram_blob_attempt_timeout_total", "op" => op).increment(1);
                    Err(BlobError::Sdk(
                        format!("attempt deadline {}s exceeded", d.as_secs_f64()).into(),
                    ))
                }
            },
        }
    }

    async fn with_retry<T, Fut>(
        &self,
        op: &'static str,
        deadline: Option<Duration>,
        mut call: impl FnMut() -> Fut,
    ) -> Result<T, BlobError>
    where
        Fut: Future<Output = Result<T, BlobError>>,
    {
        let started = std::time::Instant::now();
        let mut attempt = 1u32;
        let result = loop {
            match Self::attempt(op, deadline, call()).await {
                Ok(v) => break Ok(v),
                Err(e) if is_transient(&e) && attempt < self.policy.attempts.max(1) => {
                    let delay = self.policy.backoff_for(attempt);
                    metrics::counter!("engram_blob_retry_total", "op" => op).increment(1);
                    tracing::warn!(
                        op,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "transient blob error; retrying",
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => {
                    if is_transient(&e) {
                        metrics::counter!("engram_blob_retry_exhausted_total", "op" => op)
                            .increment(1);
                        tracing::warn!(
                            op,
                            attempts = attempt,
                            error = %e,
                            "transient blob error persisted through every attempt; giving up",
                        );
                    }
                    break Err(e);
                }
            }
        };
        let outcome = if result.is_ok() { "ok" } else { "error" };
        metrics::histogram!("engram_blob_op_seconds", "op" => op, "outcome" => outcome)
            .record(started.elapsed().as_secs_f64());
        result
    }
}

#[async_trait]
impl BlobStorage for BlobClient {
    async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError> {
        // The inbound stream is consumed by the first attempt — not
        // replayable and not deadline-bounded here (multi-GB bodies).
        self.inner.put_streaming(key, body).await
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<u64, BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("put", deadline, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            let body = body.clone();
            async move { inner.put(&key, body).await }
        })
        .await
    }

    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        // Retries + deadline on the initial call only; the returned
        // body is the caller's (see module docs).
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("get_streaming", deadline, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.get_streaming(&key).await }
        })
        .await
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        // Goes through `inner.get` (not the trait default over our own
        // `get_streaming`) so the body collect runs INSIDE the retried,
        // deadline-bounded attempt — a mid-body connection reset
        // re-issues the whole GET. The 2026-07-22 failure shape.
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("get", deadline, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.get(&key).await }
        })
        .await
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("head", deadline, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.head(&key).await }
        })
        .await
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("delete", deadline, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.delete(&key).await }
        })
        .await
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        let deadline = Some(self.policy.list_timeout);
        self.with_retry("list_prefix", deadline, || {
            let inner = self.inner.clone();
            let prefix = prefix.to_owned();
            async move { inner.list_prefix(&prefix).await }
        })
        .await
    }
}

/// Pick and fully assemble the blob tier from `ENGRAM_BLOB_BACKEND`.
/// THE construction path for every binary — there is deliberately no
/// way to reach a raw backend from here.
///
/// - `local` (default) — `<root>/blobs/` under `ENGRAM_LOCAL_PATH`
///   (default `./var/engram`); the directory is created. Dev/test.
/// - `gcs` — production. Requires `ENGRAM_GCS_BUCKET`; honors
///   `STORAGE_EMULATOR_HOST` for fake-gcs-server.
/// - `s3` — reserved (the SDK retries internally; wire it through
///   here when it lands).
///
/// Both arms come back wrapped in [`BlobClient`] — one code path, so
/// dev exercises the same retry/deadline/metrics layer prod runs.
/// Fails closed at startup on misconfiguration: a process that can't
/// reach its chunk backend must refuse to come up, not fail every
/// session create.
pub async fn from_env() -> Result<Arc<dyn BlobStorage>, String> {
    let backend = std::env::var("ENGRAM_BLOB_BACKEND")
        .unwrap_or_else(|_| "local".to_string())
        .to_lowercase();
    let inner: Arc<dyn BlobStorage> = match backend.as_str() {
        "local" => {
            let root =
                std::env::var("ENGRAM_LOCAL_PATH").unwrap_or_else(|_| "./var/engram".to_string());
            let blobs_dir = std::path::PathBuf::from(root).join("blobs");
            tokio::fs::create_dir_all(&blobs_dir)
                .await
                .map_err(|e| format!("create local blob root {}: {e}", blobs_dir.display()))?;
            tracing::info!(path = %blobs_dir.display(), "blob backend: local");
            Arc::new(engram_storage_local::LocalBlobStorage::new(blobs_dir))
        }
        "gcs" => {
            let bucket = std::env::var("ENGRAM_GCS_BUCKET")
                .map_err(|_| "ENGRAM_BLOB_BACKEND=gcs requires ENGRAM_GCS_BUCKET".to_string())?;
            tracing::info!(bucket = %bucket, "blob backend: gcs");
            let store = engram_storage_gcs::GcsBlobStorage::connect(bucket)
                .await
                .map_err(|e| format!("gcs connect: {e}"))?;
            Arc::new(store)
        }
        "s3" => {
            return Err(
                "ENGRAM_BLOB_BACKEND=s3 is reserved; only `local` and `gcs` are supported today"
                    .into(),
            )
        }
        other => {
            return Err(format!(
                "unknown ENGRAM_BLOB_BACKEND={other}; expected `local` or `gcs`"
            ))
        }
    };
    Ok(Arc::new(BlobClient::wrap(inner)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_testkit::storage::{
        FaultPlan, FaultyBlobStorage, GetFault, GetFaultKind, InjectedError, KeyMatch, When,
    };

    fn test_policy() -> ClientPolicy {
        ClientPolicy {
            attempts: 3,
            base_backoff: Duration::from_millis(1),
            attempt_timeout: Duration::from_secs(5),
            list_timeout: Duration::from_secs(5),
        }
    }

    async fn local_store(dir: &tempfile::TempDir) -> Arc<dyn BlobStorage> {
        Arc::new(engram_storage_local::LocalBlobStorage::new(
            dir.path().join("blobs"),
        ))
    }

    /// The 2026-07-22 shape: the first GET starts streaming bytes and
    /// dies mid-body with an SDK error; the retry re-issues the GET
    /// from scratch and returns the full body.
    #[tokio::test]
    async fn get_retries_a_mid_body_transient_error() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        inner
            .put("chunks/aa/bb", Bytes::from_static(b"chunk-bytes"))
            .await
            .unwrap();

        let (faulty, counters) = FaultyBlobStorage::arc(
            inner,
            FaultPlan::new().with_get(GetFault {
                key: KeyMatch::Any,
                when: When::Nth(1),
                kind: GetFaultKind::TruncateThenError {
                    after_bytes: 4,
                    error: InjectedError::Sdk("error decoding response body".into()),
                },
            }),
        );
        let client = BlobClient::with_policy(faulty, test_policy());

        let got = client.get("chunks/aa/bb").await.expect("retried get");
        assert_eq!(&got[..], b"chunk-bytes");
        assert_eq!(counters.gets_attempted(), 2, "one failure + one retry");
    }

    #[tokio::test]
    async fn put_retries_a_transient_error_and_lands_durably() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        let (faulty, counters) = FaultyBlobStorage::arc(
            inner,
            FaultPlan::new().fail_nth_put(1, InjectedError::Sdk("connection reset".into())),
        );
        let client = BlobClient::with_policy(faulty, test_policy());

        client
            .put("chunks/cc/dd", Bytes::from_static(b"dirty-chunk"))
            .await
            .expect("retried put");
        assert_eq!(counters.puts_attempted(), 2);
        let got = client.get("chunks/cc/dd").await.unwrap();
        assert_eq!(&got[..], b"dirty-chunk");
    }

    /// NotFound is a real answer, not a fault — exactly one attempt.
    #[tokio::test]
    async fn not_found_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        let (faulty, counters) = FaultyBlobStorage::arc(inner, FaultPlan::new());
        let client = BlobClient::with_policy(faulty, test_policy());

        match client.get("chunks/absent").await {
            Err(BlobError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert_eq!(counters.gets_attempted(), 1);
    }

    /// ENOSPC (`BlobError::Io`) is permanent — surfaces on attempt 1.
    #[tokio::test]
    async fn io_errors_are_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        let (faulty, counters) = FaultyBlobStorage::arc(
            inner,
            FaultPlan::new().fail_nth_put(1, InjectedError::Enospc),
        );
        let client = BlobClient::with_policy(faulty, test_policy());

        match client.put("chunks/full", Bytes::from_static(b"x")).await {
            Err(BlobError::Io(_)) => {}
            other => panic!("expected Io, got {other:?}"),
        }
        assert_eq!(counters.puts_attempted(), 1);
    }

    /// A persistent transient-class error exhausts the attempt budget
    /// and surfaces the last error.
    #[tokio::test]
    async fn persistent_transient_error_exhausts_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        inner
            .put("chunks/ee/ff", Bytes::from_static(b"body"))
            .await
            .unwrap();
        let (faulty, counters) = FaultyBlobStorage::arc(
            inner,
            FaultPlan::new().with_get(GetFault {
                key: KeyMatch::Any,
                when: When::Always,
                kind: GetFaultKind::TruncateThenError {
                    after_bytes: 0,
                    error: InjectedError::Sdk("still resetting".into()),
                },
            }),
        );
        let client = BlobClient::with_policy(faulty, test_policy());

        match client.get("chunks/ee/ff").await {
            Err(BlobError::Sdk(_)) => {}
            other => panic!("expected Sdk, got {other:?}"),
        }
        assert_eq!(counters.gets_attempted(), 3, "the full attempt budget");
    }

    /// A backend whose GET parks forever (the hung-TCP shape the
    /// pre-client SDK path could sit in indefinitely). First attempt
    /// hangs → deadline converts it to a transient error → the retry
    /// reaches the recovered backend.
    struct HangingOnceStorage {
        inner: Arc<dyn BlobStorage>,
        hangs_left: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl BlobStorage for HangingOnceStorage {
        async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError> {
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
            self.inner.get_streaming(key).await
        }
        async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
            use std::sync::atomic::Ordering;
            if self
                .hangs_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                std::future::pending::<()>().await;
                unreachable!();
            }
            self.inner.get(key).await
        }
        async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    #[tokio::test]
    async fn hung_attempt_hits_the_deadline_and_the_retry_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        inner
            .put("chunks/hang", Bytes::from_static(b"eventually"))
            .await
            .unwrap();
        let hanging = Arc::new(HangingOnceStorage {
            inner,
            hangs_left: std::sync::atomic::AtomicU32::new(1),
        });
        let client = BlobClient::with_policy(
            hanging,
            ClientPolicy {
                attempts: 3,
                base_backoff: Duration::from_millis(1),
                attempt_timeout: Duration::from_millis(50),
                list_timeout: Duration::from_secs(5),
            },
        );

        let got = client.get("chunks/hang").await.expect("deadline + retry");
        assert_eq!(&got[..], b"eventually");
    }

    #[tokio::test]
    async fn forever_hung_backend_surfaces_after_the_attempt_budget() {
        let dir = tempfile::tempdir().unwrap();
        let inner = local_store(&dir).await;
        let hanging = Arc::new(HangingOnceStorage {
            inner,
            hangs_left: std::sync::atomic::AtomicU32::new(u32::MAX),
        });
        let client = BlobClient::with_policy(
            hanging,
            ClientPolicy {
                attempts: 2,
                base_backoff: Duration::from_millis(1),
                attempt_timeout: Duration::from_millis(20),
                list_timeout: Duration::from_secs(5),
            },
        );

        let started = std::time::Instant::now();
        match client.get("chunks/never").await {
            Err(BlobError::Sdk(e)) => {
                assert!(e.to_string().contains("deadline"), "got: {e}");
            }
            other => panic!("expected deadline Sdk error, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded, not wedged"
        );
    }

    // `from_env` reads process-global env. Run the cases inside a
    // single test under one lock so we don't race when nextest fans
    // out, and restore the env on the way out so a test failure
    // doesn't poison neighbors with leaked state.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn set_env(key: &str, value: Option<&str>) -> Option<String> {
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        prev
    }

    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[tokio::test]
    async fn from_env_dispatches_per_backend_var() {
        let _guard = ENV_LOCK.lock().await;

        // Case 1: unset → defaults to local under `<root>/blobs`.
        {
            let dir = tempfile::tempdir().unwrap();
            let p1 = set_env("ENGRAM_BLOB_BACKEND", None);
            let p2 = set_env("ENGRAM_LOCAL_PATH", Some(dir.path().to_str().unwrap()));
            assert!(from_env().await.is_ok(), "default-local should succeed");
            assert!(
                dir.path().join("blobs").is_dir(),
                "local root must be created"
            );
            restore_env("ENGRAM_BLOB_BACKEND", p1);
            restore_env("ENGRAM_LOCAL_PATH", p2);
        }

        // Case 2: gcs without bucket → clear error naming the var.
        {
            let p1 = set_env("ENGRAM_BLOB_BACKEND", Some("gcs"));
            let p2 = set_env("ENGRAM_GCS_BUCKET", None);
            let err = match from_env().await {
                Ok(_) => panic!("missing bucket must error"),
                Err(e) => e,
            };
            assert!(
                err.contains("ENGRAM_GCS_BUCKET"),
                "error must name the missing var: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p1);
            restore_env("ENGRAM_GCS_BUCKET", p2);
        }

        // Case 3: unknown backend → error names the bad value.
        {
            let p = set_env("ENGRAM_BLOB_BACKEND", Some("azure"));
            let err = match from_env().await {
                Ok(_) => panic!("unknown backend must error"),
                Err(e) => e,
            };
            assert!(
                err.contains("azure"),
                "error must echo the unknown value: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p);
        }

        // Case 4: s3 → explicit "reserved" message, not just a generic
        // "unknown".
        {
            let p = set_env("ENGRAM_BLOB_BACKEND", Some("s3"));
            let err = match from_env().await {
                Ok(_) => panic!("s3 stub must surface as a clear error"),
                Err(e) => e,
            };
            assert!(
                err.contains("s3"),
                "s3 error must call out s3 explicitly: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p);
        }
    }
}
