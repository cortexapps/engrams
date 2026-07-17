//! `FaultyBlobStorage` — a scripted fault-injecting wrapper around
//! `Arc<dyn BlobStorage>` (ADR 0099, H5).
//!
//! The wrapper is driven by a **[`FaultPlan`]**: plain, `Clone`-able data
//! describing *deterministic* injections — fail the Nth `put`, truncate a
//! `get` after K bytes, error mid-drain on a streaming put (ENOSPC /
//! partial upload), inject `NotFound` on `head`/`exists`. Nothing here uses
//! an RNG, so the plan is fully reproducible and — because it's plain data —
//! the ADR 0098 simulator can generate plans from a seed and reuse this
//! exact wrapper.
//!
//! # Why every trait method is overridden
//!
//! [`BlobStorage`] ships convenience defaults (`put`, `get`, `exists`) that
//! forward to the required methods. A wrapper that overrode only the
//! required methods would still be *correct* by accident (the defaults
//! dynamic-dispatch back through `self`), but the ADR asks us to override
//! the defaults explicitly so there is **no** call path that reaches the
//! inner store without passing through the fault check — and so the
//! counters stay honest. Every override here therefore routes through the
//! same faulted primitive: `put` → `put_streaming`, `get` → `get_streaming`,
//! `exists` → `head`.
//!
//! # Counters
//!
//! [`FaultyBlobStorage::counters`] exposes atomic counters
//! (`puts_attempted`, `gets_truncated`, …) so tests can assert on what the
//! wrapper actually did rather than inferring it from side effects.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use engram_core::error::BlobError;
use engram_core::traits::storage::{BlobObjectMeta, BlobStorage, ByteStream};
use futures::StreamExt;

// ---------------------------------------------------------------------------
// Plan: plain data, no RNG.
// ---------------------------------------------------------------------------

/// Predicate over a blob key. `Any` matches everything; the rest are
/// substring/positional matches against the storage key
/// (`chunks/sha256/ab/cdef…`, `manifests/<id>/v<n>.json`, …).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum KeyMatch {
    /// Match every key.
    #[default]
    Any,
    /// Exact key equality.
    Exact(String),
    /// Key starts with this string (e.g. `"chunks/"`).
    Prefix(String),
    /// Key contains this substring (e.g. a chunk's hex hash).
    Contains(String),
}

impl KeyMatch {
    /// Whether `key` satisfies this predicate.
    pub fn matches(&self, key: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(k) => key == k,
            Self::Prefix(p) => key.starts_with(p),
            Self::Contains(s) => key.contains(s),
        }
    }
}

/// When a rule fires, relative to how many *prior* calls have matched its
/// predicate. Counting is per-rule and 1-based, so `Nth(1)` fires on the
/// first matching call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    /// Fire on every matching call.
    Always,
    /// Fire only on the Nth matching call (1-based).
    Nth(u64),
    /// Fire on the Nth matching call and every one after it.
    FromNth(u64),
}

impl When {
    fn fires(self, match_count: u64) -> bool {
        match self {
            Self::Always => true,
            Self::Nth(n) => match_count == n,
            Self::FromNth(n) => match_count >= n,
        }
    }
}

/// A storage error to inject. Plain data (`Clone`) mapped to a freshly
/// constructed [`BlobError`] at injection time — `BlobError` itself is not
/// `Clone` (it wraps `io::Error` / boxed SDK errors).
#[derive(Clone, Debug)]
pub enum InjectedError {
    /// `BlobError::NotFound` — key absent (404 / ENOENT).
    NotFound,
    /// `BlobError::Io(ENOSPC)` — the disk-full / quota case.
    Enospc,
    /// `BlobError::Sdk(_)` — a generic backend/transport failure.
    Sdk(String),
    /// `BlobError::Protocol(_)` — an unexpected response shape.
    Protocol(String),
}

impl InjectedError {
    /// Build a fresh `BlobError` for this injection.
    pub fn to_blob_error(&self) -> BlobError {
        match self {
            Self::NotFound => BlobError::NotFound,
            // 28 == ENOSPC on Linux and macOS.
            Self::Enospc => BlobError::Io(std::io::Error::from_raw_os_error(28)),
            Self::Sdk(msg) => BlobError::Sdk(Box::<dyn std::error::Error + Send + Sync>::from(
                msg.clone(),
            )),
            Self::Protocol(msg) => BlobError::Protocol(msg.clone()),
        }
    }
}

/// A fault on the put path (`put` and `put_streaming`).
#[derive(Clone, Debug)]
pub struct PutFault {
    pub key: KeyMatch,
    pub when: When,
    pub kind: PutFaultKind,
}

#[derive(Clone, Debug)]
pub enum PutFaultKind {
    /// Fail before writing anything durable to the inner store.
    FailBeforeWrite(InjectedError),
    /// Drain `after_bytes` of the streaming body, then error mid-stream —
    /// models ENOSPC / an aborted multipart upload. Nothing is written to
    /// the inner store (S3/GCS abort the multipart; the local fs backend
    /// writes to a tempfile that is never renamed), so the invariant
    /// "a failed put leaves no durable object" holds. For the buffered
    /// `put`, this behaves like [`Self::FailBeforeWrite`] with an ENOSPC.
    ErrorMidStream { after_bytes: u64 },
}

/// A fault on the get path (`get` and `get_streaming`).
#[derive(Clone, Debug)]
pub struct GetFault {
    pub key: KeyMatch,
    pub when: When,
    pub kind: GetFaultKind,
}

#[derive(Clone, Debug)]
pub enum GetFaultKind {
    /// Return `NotFound` immediately (the key "vanished").
    NotFound(InjectedError),
    /// Yield `after_bytes`, then an error mid-stream — a download that
    /// starts then fails.
    TruncateThenError {
        after_bytes: u64,
        error: InjectedError,
    },
    /// Yield only `after_bytes`, then a **clean** EOF — a *silent* short
    /// read. No error surfaces from storage; the resolver must catch the
    /// short body via hash verification. This is the sharpest test of the
    /// resolver's "byte-correct or error" contract.
    TruncateCleanEof { after_bytes: u64 },
}

/// A fault on the metadata path (`head` and, by routing, `exists`).
#[derive(Clone, Debug)]
pub struct HeadFault {
    pub key: KeyMatch,
    pub when: When,
    pub kind: HeadFaultKind,
}

#[derive(Clone, Debug)]
pub enum HeadFaultKind {
    /// Report the key as absent — `head` errors `NotFound`, `exists`
    /// returns `Ok(false)`.
    NotFound,
    /// Surface a hard error from both `head` and `exists`.
    Error(InjectedError),
}

/// A deterministic, reproducible script of storage faults.
///
/// Build it fluently:
/// ```ignore
/// let plan = FaultPlan::new()
///     .fail_nth_put(3, InjectedError::Enospc)
///     .with_get(GetFault {
///         key: KeyMatch::Contains(hash.to_hex()),
///         when: When::Always,
///         kind: GetFaultKind::TruncateCleanEof { after_bytes: 4 },
///     });
/// ```
#[derive(Clone, Debug, Default)]
pub struct FaultPlan {
    pub puts: Vec<PutFault>,
    pub gets: Vec<GetFault>,
    pub heads: Vec<HeadFault>,
}

impl FaultPlan {
    /// An empty plan — every operation passes through untouched.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a put fault.
    pub fn with_put(mut self, f: PutFault) -> Self {
        self.puts.push(f);
        self
    }

    /// Add a get fault.
    pub fn with_get(mut self, f: GetFault) -> Self {
        self.gets.push(f);
        self
    }

    /// Add a head fault.
    pub fn with_head(mut self, f: HeadFault) -> Self {
        self.heads.push(f);
        self
    }

    /// Shorthand: fail the Nth put of *any* key with `err`.
    pub fn fail_nth_put(self, n: u64, err: InjectedError) -> Self {
        self.with_put(PutFault {
            key: KeyMatch::Any,
            when: When::Nth(n),
            kind: PutFaultKind::FailBeforeWrite(err),
        })
    }
}

// ---------------------------------------------------------------------------
// Counters.
// ---------------------------------------------------------------------------

/// Observation counters for assertions. All `Relaxed` — tests read them
/// after the operations under test have completed.
#[derive(Debug, Default)]
pub struct Counters {
    pub puts_attempted: AtomicU64,
    pub puts_faulted: AtomicU64,
    pub gets_attempted: AtomicU64,
    pub gets_truncated: AtomicU64,
    pub gets_faulted: AtomicU64,
    pub heads_attempted: AtomicU64,
    pub heads_faulted: AtomicU64,
}

macro_rules! counter_accessors {
    ($($field:ident),+ $(,)?) => {
        $(
            #[doc = concat!("Current value of `", stringify!($field), "`.")]
            pub fn $field(&self) -> u64 {
                self.$field.load(Ordering::Relaxed)
            }
        )+
    };
}

impl Counters {
    counter_accessors!(
        puts_attempted,
        puts_faulted,
        gets_attempted,
        gets_truncated,
        gets_faulted,
        heads_attempted,
        heads_faulted,
    );
}

// ---------------------------------------------------------------------------
// The wrapper.
// ---------------------------------------------------------------------------

/// Per-rule match counters, one `AtomicU64` per rule in each plan vector.
/// Kept out of [`FaultPlan`] so the plan stays plain, `Clone`-able data.
struct Hits {
    puts: Vec<AtomicU64>,
    gets: Vec<AtomicU64>,
    heads: Vec<AtomicU64>,
}

/// Scripted fault-injecting wrapper around an `Arc<dyn BlobStorage>`.
///
/// Cheap to clone-wrap; construct one per test with a [`FaultPlan`]. See
/// the module docs for the design.
pub struct FaultyBlobStorage {
    inner: Arc<dyn BlobStorage>,
    plan: FaultPlan,
    hits: Hits,
    counters: Arc<Counters>,
}

impl FaultyBlobStorage {
    /// Wrap `inner`, injecting faults per `plan`.
    pub fn new(inner: Arc<dyn BlobStorage>, plan: FaultPlan) -> Self {
        let hits = Hits {
            puts: (0..plan.puts.len()).map(|_| AtomicU64::new(0)).collect(),
            gets: (0..plan.gets.len()).map(|_| AtomicU64::new(0)).collect(),
            heads: (0..plan.heads.len()).map(|_| AtomicU64::new(0)).collect(),
        };
        Self {
            inner,
            plan,
            hits,
            counters: Arc::new(Counters::default()),
        }
    }

    /// Convenience: wrap and hand back an `Arc<dyn BlobStorage>` plus the
    /// shared counter handle (the counters outlive the trait-object erase).
    pub fn arc(
        inner: Arc<dyn BlobStorage>,
        plan: FaultPlan,
    ) -> (Arc<dyn BlobStorage>, Arc<Counters>) {
        let faulty = Self::new(inner, plan);
        let counters = faulty.counters.clone();
        (Arc::new(faulty), counters)
    }

    /// Shared counter handle for assertions.
    pub fn counters(&self) -> Arc<Counters> {
        self.counters.clone()
    }

    /// First put-fault whose predicate matches `key` and whose trigger fires
    /// on this call. Increments the per-rule match counter for every rule
    /// whose predicate matches (independent of firing), so `Nth`/`FromNth`
    /// count matched calls, not calls-until-first-fire.
    fn put_fault_for(&self, key: &str) -> Option<PutFaultKind> {
        let mut fired: Option<PutFaultKind> = None;
        for (rule, hit) in self.plan.puts.iter().zip(self.hits.puts.iter()) {
            if !rule.key.matches(key) {
                continue;
            }
            let count = hit.fetch_add(1, Ordering::Relaxed) + 1;
            if fired.is_none() && rule.when.fires(count) {
                fired = Some(rule.kind.clone());
            }
        }
        fired
    }

    fn get_fault_for(&self, key: &str) -> Option<GetFaultKind> {
        let mut fired: Option<GetFaultKind> = None;
        for (rule, hit) in self.plan.gets.iter().zip(self.hits.gets.iter()) {
            if !rule.key.matches(key) {
                continue;
            }
            let count = hit.fetch_add(1, Ordering::Relaxed) + 1;
            if fired.is_none() && rule.when.fires(count) {
                fired = Some(rule.kind.clone());
            }
        }
        fired
    }

    fn head_fault_for(&self, key: &str) -> Option<HeadFaultKind> {
        let mut fired: Option<HeadFaultKind> = None;
        for (rule, hit) in self.plan.heads.iter().zip(self.hits.heads.iter()) {
            if !rule.key.matches(key) {
                continue;
            }
            let count = hit.fetch_add(1, Ordering::Relaxed) + 1;
            if fired.is_none() && rule.when.fires(count) {
                fired = Some(rule.kind.clone());
            }
        }
        fired
    }
}

/// A stream that yields at most `budget` bytes drawn from `source`, then
/// either a clean EOF or a single injected error.
fn truncating_stream(
    source: ByteStream,
    budget: u64,
    trailing_error: Option<InjectedError>,
) -> ByteStream {
    // Implemented as a hand-rolled `Stream` to avoid a new dep.
    ByteStream::new(TruncateStream {
        source,
        remaining: budget,
        trailing_error,
        done: false,
    })
}

struct TruncateStream {
    source: ByteStream,
    remaining: u64,
    trailing_error: Option<InjectedError>,
    done: bool,
}

impl futures::Stream for TruncateStream {
    type Item = Result<Bytes, BlobError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        if self.done {
            return Poll::Ready(None);
        }
        // Still have budget: pull from the source and clip to the budget.
        if self.remaining > 0 {
            match self.source.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    let take = std::cmp::min(self.remaining, chunk.len() as u64) as usize;
                    self.remaining -= take as u64;
                    let out = chunk.slice(0..take);
                    if self.remaining == 0 {
                        // Budget exhausted — this is the last data chunk.
                        // The trailing error (if any) comes on the next poll.
                        if self.trailing_error.is_none() {
                            self.done = true;
                        }
                    }
                    return Poll::Ready(Some(Ok(out)));
                }
                // Source ended or errored before we hit the budget: pass it
                // through (a genuine short body or upstream error).
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    self.done = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        // Budget exhausted and a trailing error was requested.
        self.done = true;
        match self.trailing_error.take() {
            Some(err) => Poll::Ready(Some(Err(err.to_blob_error()))),
            None => Poll::Ready(None),
        }
    }
}

#[async_trait]
impl BlobStorage for FaultyBlobStorage {
    async fn put_streaming(&self, key: &str, body: ByteStream) -> Result<u64, BlobError> {
        self.counters.puts_attempted.fetch_add(1, Ordering::Relaxed);
        match self.put_fault_for(key) {
            Some(PutFaultKind::FailBeforeWrite(err)) => {
                self.counters.puts_faulted.fetch_add(1, Ordering::Relaxed);
                Err(err.to_blob_error())
            }
            Some(PutFaultKind::ErrorMidStream { after_bytes }) => {
                self.counters.puts_faulted.fetch_add(1, Ordering::Relaxed);
                // Drain up to `after_bytes` from the caller's body to model
                // a partial upload, then error — WITHOUT writing anything to
                // the inner store (the destination stays absent).
                let mut body = body;
                let mut drained: u64 = 0;
                while drained < after_bytes {
                    match body.next().await {
                        Some(Ok(chunk)) => drained += chunk.len() as u64,
                        Some(Err(e)) => return Err(e),
                        None => break,
                    }
                }
                Err(InjectedError::Enospc.to_blob_error())
            }
            None => self.inner.put_streaming(key, body).await,
        }
    }

    // Override the convenience default so a `put` cannot bypass the fault
    // path: route it through the faulted `put_streaming`.
    async fn put(&self, key: &str, body: Bytes) -> Result<u64, BlobError> {
        self.put_streaming(key, ByteStream::from_bytes(body)).await
    }

    async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
        self.counters.gets_attempted.fetch_add(1, Ordering::Relaxed);
        match self.get_fault_for(key) {
            Some(GetFaultKind::NotFound(err)) => {
                self.counters.gets_faulted.fetch_add(1, Ordering::Relaxed);
                Err(err.to_blob_error())
            }
            Some(GetFaultKind::TruncateThenError { after_bytes, error }) => {
                self.counters.gets_truncated.fetch_add(1, Ordering::Relaxed);
                let source = self.inner.get_streaming(key).await?;
                Ok(truncating_stream(source, after_bytes, Some(error)))
            }
            Some(GetFaultKind::TruncateCleanEof { after_bytes }) => {
                self.counters.gets_truncated.fetch_add(1, Ordering::Relaxed);
                let source = self.inner.get_streaming(key).await?;
                Ok(truncating_stream(source, after_bytes, None))
            }
            None => self.inner.get_streaming(key).await,
        }
    }

    // Override the convenience default: drain the faulted `get_streaming`
    // so a buffered `get` sees the same truncation / error.
    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        let mut stream = self.get_streaming(key).await?;
        let mut buf = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(buf.freeze())
    }

    async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
        self.counters
            .heads_attempted
            .fetch_add(1, Ordering::Relaxed);
        match self.head_fault_for(key) {
            Some(HeadFaultKind::NotFound) => {
                self.counters.heads_faulted.fetch_add(1, Ordering::Relaxed);
                Err(BlobError::NotFound)
            }
            Some(HeadFaultKind::Error(err)) => {
                self.counters.heads_faulted.fetch_add(1, Ordering::Relaxed);
                Err(err.to_blob_error())
            }
            None => self.inner.head(key).await,
        }
    }

    // Override the convenience default: route `exists` through the faulted
    // `head` so head-faults apply to existence checks too.
    async fn exists(&self, key: &str) -> Result<bool, BlobError> {
        match self.head(key).await {
            Ok(_) => Ok(true),
            Err(BlobError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(key).await
    }

    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
        self.inner.list_prefix(prefix).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory `BlobStorage` so the wrapper's own logic can be
    /// tested without pulling in a concrete backend crate.
    #[derive(Default)]
    struct MemBlob {
        map: Mutex<HashMap<String, Bytes>>,
    }

    #[async_trait]
    impl BlobStorage for MemBlob {
        async fn put_streaming(&self, key: &str, mut body: ByteStream) -> Result<u64, BlobError> {
            let mut buf = BytesMut::new();
            while let Some(chunk) = body.next().await {
                buf.extend_from_slice(&chunk?);
            }
            let n = buf.len() as u64;
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), buf.freeze());
            Ok(n)
        }
        async fn get_streaming(&self, key: &str) -> Result<ByteStream, BlobError> {
            match self.map.lock().unwrap().get(key) {
                Some(b) => Ok(ByteStream::from_bytes(b.clone())),
                None => Err(BlobError::NotFound),
            }
        }
        async fn head(&self, key: &str) -> Result<BlobObjectMeta, BlobError> {
            match self.map.lock().unwrap().get(key) {
                Some(b) => Ok(BlobObjectMeta {
                    size_bytes: b.len() as u64,
                    etag: None,
                }),
                None => Err(BlobError::NotFound),
            }
        }
        async fn delete(&self, key: &str) -> Result<(), BlobError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }
        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, BlobError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    fn faulty(plan: FaultPlan) -> (FaultyBlobStorage, Arc<dyn BlobStorage>) {
        let inner: Arc<dyn BlobStorage> = Arc::new(MemBlob::default());
        (FaultyBlobStorage::new(inner.clone(), plan), inner)
    }

    #[tokio::test]
    async fn nth_put_fires_exactly_once_and_leaves_no_object() {
        let plan = FaultPlan::new().fail_nth_put(2, InjectedError::Enospc);
        let (f, inner) = faulty(plan);

        f.put("a", Bytes::from_static(b"1")).await.unwrap(); // ok
        let err = f.put("b", Bytes::from_static(b"2")).await.unwrap_err(); // Nth=2 fails
        assert!(matches!(err, BlobError::Io(_)));
        f.put("c", Bytes::from_static(b"3")).await.unwrap(); // ok again

        // The failed put wrote nothing durable.
        assert!(inner.exists("a").await.unwrap());
        assert!(!inner.exists("b").await.unwrap());
        assert!(inner.exists("c").await.unwrap());

        let c = f.counters();
        assert_eq!(c.puts_attempted(), 3);
        assert_eq!(c.puts_faulted(), 1);
    }

    #[tokio::test]
    async fn error_mid_stream_drains_but_writes_nothing() {
        let plan = FaultPlan::new().with_put(PutFault {
            key: KeyMatch::Any,
            when: When::Always,
            kind: PutFaultKind::ErrorMidStream { after_bytes: 3 },
        });
        let (f, inner) = faulty(plan);
        // Feed a multi-chunk streaming body; the wrapper drains 3 bytes
        // then errors without persisting anything.
        let body = ByteStream::new(futures::stream::iter(vec![
            Ok(Bytes::from_static(b"aa")),
            Ok(Bytes::from_static(b"bb")),
            Ok(Bytes::from_static(b"cc")),
        ]));
        let err = f.put_streaming("k", body).await.unwrap_err();
        assert!(matches!(err, BlobError::Io(_)));
        assert!(!inner.exists("k").await.unwrap());
        assert_eq!(f.counters().puts_faulted(), 1);
    }

    #[tokio::test]
    async fn truncate_clean_eof_yields_short_body_without_error() {
        let inner: Arc<dyn BlobStorage> = Arc::new(MemBlob::default());
        inner
            .put("k", Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        let plan = FaultPlan::new().with_get(GetFault {
            key: KeyMatch::Exact("k".into()),
            when: When::Always,
            kind: GetFaultKind::TruncateCleanEof { after_bytes: 4 },
        });
        let f = FaultyBlobStorage::new(inner, plan);
        // Buffered get drains to a short, error-free body.
        let bytes = f.get("k").await.unwrap();
        assert_eq!(&bytes[..], b"0123");
        assert_eq!(f.counters().gets_truncated(), 1);
    }

    #[tokio::test]
    async fn truncate_then_error_surfaces_after_partial_bytes() {
        let inner: Arc<dyn BlobStorage> = Arc::new(MemBlob::default());
        inner
            .put("k", Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        let plan = FaultPlan::new().with_get(GetFault {
            key: KeyMatch::Any,
            when: When::Always,
            kind: GetFaultKind::TruncateThenError {
                after_bytes: 4,
                error: InjectedError::Protocol("mid-download reset".into()),
            },
        });
        let f = FaultyBlobStorage::new(inner, plan);
        // Streaming: first chunk carries 4 bytes, then the error.
        let mut s = f.get_streaming("k").await.unwrap();
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(&first[..], b"0123");
        let err = s.next().await.unwrap().unwrap_err();
        assert!(matches!(err, BlobError::Protocol(_)));
        // Buffered get surfaces the error outright.
        assert!(matches!(f.get("k").await, Err(BlobError::Protocol(_))));
    }

    #[tokio::test]
    async fn head_notfound_makes_exists_false_only_for_matching_keys() {
        let inner: Arc<dyn BlobStorage> = Arc::new(MemBlob::default());
        inner
            .put("chunks/x", Bytes::from_static(b"x"))
            .await
            .unwrap();
        inner
            .put("manifests/y", Bytes::from_static(b"y"))
            .await
            .unwrap();
        let plan = FaultPlan::new().with_head(HeadFault {
            key: KeyMatch::Prefix("chunks/".into()),
            when: When::Always,
            kind: HeadFaultKind::NotFound,
        });
        let f = FaultyBlobStorage::new(inner, plan);
        // exists routes through the faulted head.
        assert!(
            !f.exists("chunks/x").await.unwrap(),
            "faulted key reads absent"
        );
        assert!(
            f.exists("manifests/y").await.unwrap(),
            "non-matching key untouched"
        );
        assert!(matches!(f.head("chunks/x").await, Err(BlobError::NotFound)));
        assert_eq!(f.counters().heads_faulted(), 2);
    }
}
