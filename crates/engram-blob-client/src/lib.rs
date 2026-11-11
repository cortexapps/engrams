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
//!   [`BlobError::Protocol`] / [`BlobError::Throttled`] / a deadline)
//!   are retried with exponential backoff. `NotFound`, `Config`, and
//!   `Io` (local-fs ENOSPC and friends) surface immediately. Every
//!   blob op is idempotent (content-addressed chunks, versioned
//!   manifests), so retry is unconditionally safe.
//! - **Throttled PUTs**: a PUT the store refused for load waits for
//!   seconds, with jitter ([`ClientPolicy::put_throttled`]); the store
//!   adds capacity over minutes, and an upload burst that retries in
//!   step meets the same limit again. Reads keep the short schedule: a
//!   guest can wait on a GET.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::error::BlobError;
use engram_core::traits::{BlobObjectMeta, BlobStorage, ByteStream, ListPage};

/// Whether an error is worth another attempt. `Sdk` carries the SDK
/// backends' transport failures (they fold IO errors into it);
/// `Protocol` is a garbled response (an LB hiccup); `Throttled` is the
/// store asking for less load. A permanent error
/// wearing `Sdk` clothing (403, 412) costs at most `attempts - 1`
/// extra round-trips before surfacing — cheap next to the Full
/// snapshot a poisoned chain costs.
fn is_transient(e: &BlobError) -> bool {
    matches!(
        e,
        BlobError::Sdk(_) | BlobError::Protocol(_) | BlobError::Throttled(_)
    )
}

/// Attempt budget, backoff shape, and per-attempt deadlines.
/// `attempts` is the TOTAL number of tries (`3` = 1 initial + 2
/// retries); the delay before retry `n` is `base_backoff * 2^(n-1)`,
/// with no jitter. A throttled PUT uses [`Self::put_throttled`]
/// instead. One op counts its attempts across both schedules.
#[derive(Clone, Copy, Debug)]
pub struct ClientPolicy {
    pub attempts: u32,
    pub base_backoff: Duration,
    /// The schedule for a PUT that failed with [`BlobError::Throttled`].
    pub put_throttled: ThrottleBackoff,
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
            put_throttled: ThrottleBackoff::default(),
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

/// Truncated exponential backoff with jitter. Before retry `n` the
/// ceiling is `min(base * 2^(n-1), max_delay)`, and the delay is a value
/// in `[ceiling / 2, ceiling)`: never less than half, so a throttled
/// store always gets a pause, and spread over the other half, so a burst
/// of throttled PUTs does not retry at the same instant.
///
/// The default (6 tries, 1 s base, 16 s cap) waits 15.5 s to 31 s in all
/// before it gives up. Google's client libraries start at 1 s and double;
/// the total stays under a minute because a periodic checkpoint holds
/// its sandbox's capture lock while it uploads.
#[derive(Clone, Copy, Debug)]
pub struct ThrottleBackoff {
    /// Total tries, the first included.
    pub attempts: u32,
    pub base: Duration,
    pub max_delay: Duration,
}

impl Default for ThrottleBackoff {
    fn default() -> Self {
        Self {
            attempts: 6,
            base: Duration::from_secs(1),
            max_delay: Duration::from_secs(16),
        }
    }
}

impl ThrottleBackoff {
    /// The delay before retry `retry` (1-based) of the call numbered
    /// `call`. The jitter is a hash of the two, not a random draw: a run
    /// replays exactly, and concurrent calls still get different values.
    fn delay(&self, retry: u32, call: u64) -> Duration {
        let ceiling = self
            .base
            .saturating_mul(1u32 << (retry - 1).min(16))
            .min(self.max_delay);
        let half = ceiling / 2;
        // A fraction of `half` in [0, 1), as 32 bits of the hash.
        let frac = u128::from(splitmix64(call ^ (u64::from(retry) << 48)) >> 32);
        half + Duration::from_nanos(((half.as_nanos() * frac) >> 32) as u64)
    }
}

/// SplitMix64's output function: a cheap, well-mixed 64-bit hash.
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The encapsulated blob-tier client. Construct via [`from_env`] in
/// binaries, [`BlobClient::wrap`]/[`BlobClient::with_policy`] in
/// tests and the simulator.
pub struct BlobClient {
    inner: Arc<dyn BlobStorage>,
    policy: ClientPolicy,
    /// Numbers each op; the seed of its throttle jitter.
    calls: AtomicU64,
}

impl BlobClient {
    /// Wrap `inner` with the default policy.
    pub fn wrap(inner: Arc<dyn BlobStorage>) -> Self {
        Self::with_policy(inner, ClientPolicy::default())
    }

    /// Wrap `inner` with an explicit policy (tests use ~0 backoff and
    /// short deadlines).
    pub fn with_policy(inner: Arc<dyn BlobStorage>, policy: ClientPolicy) -> Self {
        Self {
            inner,
            policy,
            calls: AtomicU64::new(0),
        }
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

    /// The delay before the next try after `attempt` failed with `e`, or
    /// `None` when `e` is final or the schedule is spent. `throttle` is
    /// the schedule for a [`BlobError::Throttled`] error, if the op has
    /// one.
    fn retry_delay(
        &self,
        e: &BlobError,
        attempt: u32,
        throttle: Option<&ThrottleBackoff>,
        call: u64,
    ) -> Option<Duration> {
        if !is_transient(e) {
            return None;
        }
        match throttle {
            Some(t) if matches!(e, BlobError::Throttled(_)) => {
                (attempt < t.attempts).then(|| t.delay(attempt, call))
            }
            _ => (attempt < self.policy.attempts).then(|| self.policy.backoff_for(attempt)),
        }
    }

    async fn with_retry<T, Fut>(
        &self,
        op: &'static str,
        deadline: Option<Duration>,
        throttle: Option<&ThrottleBackoff>,
        mut call: impl FnMut() -> Fut,
    ) -> Result<T, BlobError>
    where
        Fut: Future<Output = Result<T, BlobError>>,
    {
        let started = std::time::Instant::now();
        let call_no = self.calls.fetch_add(1, Ordering::Relaxed);
        let mut attempt = 1u32;
        let result = loop {
            let e = match Self::attempt(op, deadline, call()).await {
                Ok(v) => break Ok(v),
                Err(e) => e,
            };
            let Some(delay) = self.retry_delay(&e, attempt, throttle, call_no) else {
                if is_transient(&e) {
                    metrics::counter!("engram_blob_retry_exhausted_total", "op" => op).increment(1);
                    tracing::warn!(
                        op,
                        attempts = attempt,
                        error = %e,
                        "transient blob error persisted through every attempt; giving up",
                    );
                }
                break Err(e);
            };
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
        let throttle = Some(&self.policy.put_throttled);
        self.with_retry("put", deadline, throttle, || {
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
        self.with_retry("get_streaming", deadline, None, || {
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
        self.with_retry("get", deadline, None, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.get(&key).await }
        })
        .await
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes, BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("get_range", deadline, None, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.get_range(&key, offset, len).await }
        })
        .await
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("head", deadline, None, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.head(&key).await }
        })
        .await
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("delete", deadline, None, || {
            let inner = self.inner.clone();
            let key = key.to_owned();
            async move { inner.delete(&key).await }
        })
        .await
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        let deadline = Some(self.policy.list_timeout);
        self.with_retry("list_prefix", deadline, None, || {
            let inner = self.inner.clone();
            let prefix = prefix.to_owned();
            async move { inner.list_prefix(&prefix).await }
        })
        .await
    }

    /// Forwarded, NOT left to the trait default — the default would call
    /// `self.list_prefix` and put the whole listing back under one
    /// deadline, which is the failure this method exists to avoid. One
    /// page is a single round-trip, so it takes the ordinary
    /// `attempt_timeout` rather than the generous `list_timeout`.
    async fn list_prefix_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ListPage, BlobError> {
        let deadline = Some(self.policy.attempt_timeout);
        self.with_retry("list_prefix_page", deadline, None, || {
            let inner = self.inner.clone();
            let prefix = prefix.to_owned();
            let cursor = cursor.map(str::to_string);
            async move {
                inner
                    .list_prefix_page(&prefix, cursor.as_deref(), limit)
                    .await
            }
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
/// - `s3` — production (ADR 0122). Requires `ENGRAM_S3_BUCKET`;
///   honors `ENGRAM_S3_ENDPOINT_URL` (MinIO / R2) and
///   `ENGRAM_S3_REGION`. SDK-internal retries are disabled
///   (engram-aws) — this layer owns them.
///
/// Every arm comes back wrapped in [`BlobClient`] — one code path, so
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
            let bucket = std::env::var("ENGRAM_S3_BUCKET")
                .map_err(|_| "ENGRAM_BLOB_BACKEND=s3 requires ENGRAM_S3_BUCKET".to_string())?;
            tracing::info!(bucket = %bucket, "blob backend: s3");
            let store = engram_storage_s3::S3BlobStorage::connect(bucket)
                .await
                .map_err(|e| format!("s3 connect: {e}"))?;
            Arc::new(store)
        }
        other => {
            return Err(format!(
                "unknown ENGRAM_BLOB_BACKEND={other}; expected `local`, `gcs`, or `s3`"
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

    /// GCS's ranged GET through the client that wraps every store. The
    /// emulator closes its connection after a 416 without a
    /// `Connection: close`, so a request on the pooled connection can fail
    /// in transport; the client retries it, as it does in production.
    #[tokio::test(flavor = "current_thread")]
    async fn gcs_ranged_get_against_emulator() {
        if std::env::var("STORAGE_EMULATOR_HOST").is_err() {
            return;
        }
        let Ok(bucket) = std::env::var("ENGRAM_TEST_GCS_BUCKET") else {
            return;
        };
        let gcs = engram_storage_gcs::GcsBlobStorage::connect(bucket)
            .await
            .expect("connect against emulator");
        let client = BlobClient::wrap(Arc::new(gcs));
        engram_testkit::blob_conformance::ranged_get(&client).await;
    }

    fn test_policy() -> ClientPolicy {
        ClientPolicy {
            attempts: 3,
            base_backoff: Duration::from_millis(1),
            put_throttled: ThrottleBackoff {
                attempts: 3,
                base: Duration::from_millis(1),
                max_delay: Duration::from_millis(4),
            },
            attempt_timeout: Duration::from_secs(5),
            list_timeout: Duration::from_secs(5),
        }
    }

    /// Answers the first `throttles` PUTs and GETs with
    /// `BlobError::Throttled`, then succeeds. No I/O, so paused tokio time
    /// advances only through the client's own sleeps.
    struct ThrottlingStorage {
        throttles: u32,
        puts: AtomicU64,
        gets: AtomicU64,
    }

    impl ThrottlingStorage {
        fn new(throttles: u32) -> Arc<Self> {
            Arc::new(Self {
                throttles,
                puts: AtomicU64::new(0),
                gets: AtomicU64::new(0),
            })
        }

        fn answer(&self, calls: &AtomicU64) -> Result<(), BlobError> {
            if calls.fetch_add(1, Ordering::SeqCst) < u64::from(self.throttles) {
                return Err(BlobError::Throttled(
                    "429: request distribution is too uneven".into(),
                ));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl BlobStorage for ThrottlingStorage {
        async fn put_streaming(&self, _key: &str, _body: ByteStream) -> Result<u64, BlobError> {
            unreachable!("not used by these tests")
        }
        async fn put(&self, _key: &str, body: Bytes) -> Result<u64, BlobError> {
            self.answer(&self.puts).map(|()| body.len() as u64)
        }
        async fn get_streaming(&self, _key: &str) -> Result<ByteStream, BlobError> {
            unreachable!("not used by these tests")
        }
        async fn get(&self, _key: &str) -> Result<Bytes, BlobError> {
            self.answer(&self.gets)
                .map(|()| Bytes::from_static(b"body"))
        }
        async fn head(&self, _key: &str) -> Result<BlobObjectMeta, BlobError> {
            unreachable!("not used by these tests")
        }
        async fn delete(&self, _key: &str) -> Result<(), BlobError> {
            unreachable!("not used by these tests")
        }
        async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>, BlobError> {
            unreachable!("not used by these tests")
        }
    }

    /// Three throttles, then success: the PUT waits through retries 1-3
    /// of the default schedule (ceilings 1 s, 2 s, 4 s; each delay at
    /// least half its ceiling) and lands.
    #[tokio::test(start_paused = true)]
    async fn throttled_put_backs_off_for_seconds_then_lands() {
        let store = ThrottlingStorage::new(3);
        let client = BlobClient::wrap(store.clone());

        let started = tokio::time::Instant::now();
        let n = client
            .put("chunks/sha256/ab/cd", Bytes::from_static(b"chunk"))
            .await
            .expect("the PUT lands once the store stops throttling");
        let waited = started.elapsed();

        assert_eq!(n, 5);
        assert_eq!(store.puts.load(Ordering::SeqCst), 4);
        assert!(
            waited >= Duration::from_millis(3500) && waited < Duration::from_secs(7),
            "waited {waited:?}"
        );
    }

    /// A throttle that never clears ends the PUT after the whole default
    /// schedule: 6 tries, 15.5 s to 31 s of backoff, and the error is the
    /// store's throttle.
    #[tokio::test(start_paused = true)]
    async fn throttled_put_that_never_clears_gives_up_within_the_budget() {
        let store = ThrottlingStorage::new(u32::MAX);
        let client = BlobClient::wrap(store.clone());

        let started = tokio::time::Instant::now();
        let err = client
            .put("chunks/sha256/ab/cd", Bytes::from_static(b"chunk"))
            .await
            .expect_err("a lasting throttle surfaces");
        let waited = started.elapsed();

        assert!(matches!(err, BlobError::Throttled(_)), "got {err:?}");
        assert_eq!(store.puts.load(Ordering::SeqCst), 6);
        assert!(
            waited >= Duration::from_millis(15_500) && waited < Duration::from_secs(31),
            "waited {waited:?}"
        );
    }

    /// A GET keeps the short schedule when the store throttles: a guest
    /// can be waiting on it.
    #[tokio::test(start_paused = true)]
    async fn throttled_get_keeps_the_short_schedule() {
        let store = ThrottlingStorage::new(u32::MAX);
        let client = BlobClient::wrap(store.clone());

        let started = tokio::time::Instant::now();
        let err = client
            .get("chunks/sha256/ab/cd")
            .await
            .expect_err("a lasting throttle surfaces");

        assert!(matches!(err, BlobError::Throttled(_)), "got {err:?}");
        assert_eq!(store.gets.load(Ordering::SeqCst), 3);
        assert_eq!(started.elapsed(), Duration::from_millis(300));
    }

    #[test]
    fn throttle_delays_stay_in_range_and_spread_across_calls() {
        let t = ThrottleBackoff::default();
        for retry in 1..t.attempts {
            let ceiling = t.base.saturating_mul(1 << (retry - 1)).min(t.max_delay);
            let delays: Vec<Duration> = (0..256).map(|call| t.delay(retry, call)).collect();
            for d in &delays {
                assert!(*d >= ceiling / 2 && *d < ceiling, "retry {retry}: {d:?}");
            }
            let mut distinct = delays.clone();
            distinct.sort();
            distinct.dedup();
            assert!(distinct.len() > 200, "retry {retry}: delays do not spread");
            assert_eq!(
                t.delay(retry, 7),
                t.delay(retry, 7),
                "same input, same delay"
            );
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
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
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
                // The retry's read of a local file must finish inside this
                // deadline on a loaded runner.
                attempt_timeout: Duration::from_secs(1),
                ..test_policy()
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
                attempt_timeout: Duration::from_millis(20),
                ..test_policy()
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

        // Case 4: s3 without a bucket → error names the missing var
        // (fail-closed at startup, same contract as the gcs arm).
        {
            let p1 = set_env("ENGRAM_BLOB_BACKEND", Some("s3"));
            let p2 = set_env("ENGRAM_S3_BUCKET", None);
            let err = match from_env().await {
                Ok(_) => panic!("s3 without bucket must error"),
                Err(e) => e,
            };
            assert!(
                err.contains("ENGRAM_S3_BUCKET"),
                "error must name the missing var: {err}",
            );
            restore_env("ENGRAM_BLOB_BACKEND", p1);
            restore_env("ENGRAM_S3_BUCKET", p2);
        }
    }
}
