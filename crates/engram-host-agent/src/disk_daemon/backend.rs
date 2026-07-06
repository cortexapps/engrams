//! `ChunkedDiskBackend` — data plane for the NBD daemon.
//!
//! Maps NBD `(offset, length)` operations onto chunk-store
//! operations. Per-fault read resolves to one chunk fetch (via the
//! L1 NVMe cache); per-write copies the base chunk into a
//! per-session in-memory dirty buffer and applies the write there.
//!
//! Two key invariants the runtime depends on:
//!
//! 1. **Read-after-write** within a session sees the dirty bytes.
//!    Writes don't go back to the chunk store on every NBD_CMD_WRITE
//!    — that would explode object-storage cost. Dirty chunks live
//!    in RAM until `flush()` is called (snapshot trigger).
//! 2. **Base chunks are immutable.** The chunk store is content-
//!    addressed; the daemon never PUTs an existing hash again.
//!    Dirty chunks get rehashed at flush time; new hashes go up,
//!    the manifest version ticks.
//!
//! Memory cost: 16 MiB per dirty chunk. A session that writes
//! pseudorandomly across the whole 16 GiB rootfs would peak at
//! 16 GiB resident — at that point the dirty buffer is the wrong
//! shape anyway. Realistic Python/Node sessions touch < 100 MiB
//! of fresh writes between snapshots; that's six dirty chunks.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use engram_chunk_store::{
    cache::ChunkCache,
    manifest::{ChunkHash, ChunkRef, Manifest, ManifestKind},
    store::ChunkStore,
};
use engram_core::types::manifest::ManifestRef;
use tokio::sync::{Mutex, Notify};

/// ADR 0016 Phase B default: flush is triggered when dirty bytes
/// cross 256 MiB. Cold-create + resume callers pass this; tests pass
/// `u64::MAX` to disable threshold-driven notification.
pub const DEFAULT_DIRTY_THRESHOLD_BYTES: u64 = 256 * 1024 * 1024;

/// ADR 0039 item #19: bounded concurrency for the per-chunk GCS puts in
/// [`ChunkedDiskBackend::flush_upload`]. The 16 MiB-disk-chunk puts are
/// content-addressed/idempotent and keyed by chunk index, so they're
/// order-independent — we fan them out instead of awaiting one at a time
/// (~16 ms/put serial → ~32 s for a ~1,936-chunk dirty set). 32 sits
/// under a 10 Gbps host NIC's saturation and below GCS per-object rate
/// limits (mirrors the memory prefetch + re-chunk bound).
const DISK_FLUSH_UPLOAD_CONCURRENCY: usize = 32;

/// ADR 0071: bounded concurrency for the per-chunk fetches of a single
/// NBD read that spans multiple 16 MiB chunks. The fetches are
/// order-independent (each `read_chunk` resolves dirty/pending/mem/base
/// on its own); we fan them out and reassemble in order. Most NBD reads
/// span one chunk, so this only engages on a boundary-straddling or large
/// read — modest, but on the same path. 8 mirrors the postcopy-drain bound.
const DISK_READ_FETCH_CONCURRENCY: usize = 8;

/// Anything that can go wrong on the backend data plane.
#[derive(Debug)]
pub enum DiskBackendError {
    /// Caller passed a non-disk manifest (memory manifests aren't
    /// served as block devices).
    WrongKind(String),
    /// Manifest structurally invalid: chunk offset not aligned to
    /// chunk_size, or a chunk past `total_bytes`.
    InvalidManifest(String),
    /// Underlying chunk store / blob I/O failed.
    Chunk(engram_chunk_store::error::ChunkStoreError),
    /// NBD offset + length lands past the end of the virtual disk.
    /// The kernel side shouldn't normally send these; if it does,
    /// the daemon replies EINVAL.
    OutOfRange {
        offset: u64,
        length: u64,
        total: u64,
    },
    /// Internal invariant tripped (an "unreachable" branch fired).
    /// Used by `write_chunk` to surface a logic bug without
    /// panicking the daemon. Replied back to the NBD client as
    /// EIO; the operator gets a structured warn-log.
    InvariantViolation(String),
}

impl std::fmt::Display for DiskBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind(m) => write!(f, "wrong manifest kind: {m}"),
            Self::InvalidManifest(m) => write!(f, "invalid manifest: {m}"),
            Self::Chunk(e) => write!(f, "chunk store: {e}"),
            Self::OutOfRange {
                offset,
                length,
                total,
            } => write!(f, "NBD range {offset}+{length} exceeds total_bytes {total}"),
            Self::InvariantViolation(m) => write!(f, "invariant violation: {m}"),
        }
    }
}

impl std::error::Error for DiskBackendError {}

impl From<engram_chunk_store::error::ChunkStoreError> for DiskBackendError {
    fn from(e: engram_chunk_store::error::ChunkStoreError) -> Self {
        Self::Chunk(e)
    }
}

/// Outcome of [`ChunkedDiskBackend::flush`]. Carries the new
/// manifest ref the daemon should hand back to the coord-side
/// snapshot path so it can persist `disk_manifest` on the row.
#[derive(Clone, Debug)]
pub struct DiskFlushOutcome {
    /// The freshly-published manifest version. The id is the same
    /// as the input manifest's id; the version ticks.
    pub manifest_ref: ManifestRef,
    /// How many dirty chunks were flushed. Zero if the session
    /// never wrote to its disk.
    pub chunks_flushed: usize,
    /// Total bytes of new chunk content uploaded (sum of dirty
    /// chunk sizes). Useful for telemetry; small relative to the
    /// VM's total RAM since 16 MiB chunks dedup at content level.
    pub bytes_uploaded: u64,
}

/// ADR 0038 B3: handoff from `flush_local` (drain-under-pause) to
/// `flush_upload` (upload-post-resume). Carries the drained
/// `(chunk_idx, hash, bytes)` for each dirty chunk so `flush_upload`
/// uploads **its own** bytes rather than re-reading the shared
/// `pending_uploads` map. The bytes are ALSO mirrored into
/// `pending_uploads` for read-survival during the flush window, but the
/// upload no longer depends on that map — closing the drop where a
/// concurrent flush cleared `pending_uploads[idx]` before this flush's
/// upload read it, silently skipping the put while the manifest still
/// referenced the (now never-uploaded) chunk. Empty `new_chunks` =
/// nothing was dirty.
///
/// Issue #199: also carries the owned flush-pipeline guard acquired by
/// `flush_local`, so the drain and the `flush_upload` that consumes this
/// handoff are ONE critical section — two flushes can no longer rebase
/// out of order. The guard is `None` only for the empty/no-dirty
/// shortcut (nothing to publish, so nothing to serialize). It releases
/// when this struct is dropped (`flush_upload` consumes it by value;
/// `requeue_pending` likewise; the migration export drops it on
/// commit/abort).
pub struct PendingDiskFlush {
    new_chunks: Vec<(usize, ChunkHash, Bytes)>,
    /// Held across the publish; see `ChunkedDiskBackend::flush_pipeline`.
    flush_guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl PendingDiskFlush {
    /// Issue #529: the eviction finalize flavor persists these drained
    /// chunks to `<dest>/disk-pending/` BEFORE `snapshot_begin` returns
    /// (durability boundary moves earlier), then re-reads them from disk
    /// in the (possibly re-driven, possibly cross-process) background
    /// finalize job — it never calls `flush_upload` on this handle
    /// directly, so there's no live-backend rebase to preserve and
    /// nothing left to serialize once these bytes are extracted. Consumes
    /// `self`, dropping the flush-pipeline guard immediately.
    ///
    /// Its only caller is `PooledBackend::snapshot_begin`'s
    /// `#[cfg(target_os = "linux")]` disk-pending block (NBD is
    /// Linux-only) — `#[cfg]`'d rather than `#[allow(dead_code)]`'d so a
    /// non-Linux build doesn't carry an unreachable-by-construction method.
    #[cfg(target_os = "linux")]
    pub(crate) fn into_chunks(self) -> Vec<(usize, ChunkHash, Bytes)> {
        self.new_chunks
    }
}

/// ADR 0045 C2 disk post-copy: the frozen source's sealed disk state
/// — every chunk whose guest-visible content differs from the
/// published base manifest (dirty buffer + pending-uploads tier),
/// held as raw refcounted bytes. Never hashed, never uploaded; served
/// by index over `MigrationFetch::DiskChunkAt` and dropped when the
/// export retires (commit) or re-queued into `dirty` (abort).
pub struct PostCopyDiskSeal {
    chunks: HashMap<usize, Bytes>,
}

impl PostCopyDiskSeal {
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Sorted sealed chunk indices (the wire descriptor).
    pub fn indices(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.chunks.keys().map(|i| *i as u64).collect();
        v.sort_unstable();
        v
    }

    /// One sealed chunk's raw bytes (the `DiskChunkAt` serve path).
    pub fn get(&self, chunk_idx: u64) -> Option<Bytes> {
        self.chunks.get(&(chunk_idx as usize)).cloned()
    }
}

/// ADR 0045 C2 disk post-copy: how the destination backend reaches the
/// frozen source for a sealed chunk's raw bytes. Implementations own
/// retries; an `Err` is TERMINAL (the peer is gone) and latches the
/// overlay lost. Implemented by the host-agent over the migration
/// gRPC channel; tests use an in-process fake.
pub trait PostCopyDiskFetcher: Send + Sync {
    fn fetch(&self, chunk_idx: u64) -> futures::future::BoxFuture<'_, Result<Bytes, String>>;
}

/// The drain's terminal outcome: `Ok(chunks installed)` or
/// `Err(detail)` (peer lost — the coordinator rewinds). `None` until
/// terminal.
pub type PostCopyDrainSubscription = tokio::sync::watch::Receiver<Option<Result<u64, String>>>;

/// Destination-side state of an in-flight disk post-copy.
struct PostCopyDiskOverlay {
    /// Sealed indices not yet installed into `dirty`. Monotonically
    /// shrinks (fault-path + drain installs).
    sealed: Mutex<std::collections::HashSet<usize>>,
    fetcher: Arc<dyn PostCopyDiskFetcher>,
    /// The peer died with sealed chunks outstanding: still-sealed
    /// reads fail fast (EIO to the guest — the VM is about to be
    /// destroyed by the rewind) instead of hanging the NBD request.
    lost: std::sync::atomic::AtomicBool,
    done_tx: tokio::sync::watch::Sender<Option<Result<u64, String>>>,
}

/// Bounded fan-out for the destination's background sealed-chunk
/// drain. Disk chunks are 16 MiB — 8 in flight saturates a 10 Gbps
/// NIC without starving the guest's demand faults (which share the
/// same source).
const POSTCOPY_DISK_DRAIN_CONCURRENCY: usize = 8;

/// In-memory representation of the manifest, indexed for O(1) chunk
/// lookup. Identical shape to the memory side's positional manifest
/// (see `engram-uffd-handler::chunked::PositionalManifest`) but
/// kept separate so the disk daemon doesn't take a circular dep on
/// the UFFD handler crate.
#[derive(Clone)]
struct PositionalDiskManifest {
    /// `chunks[chunk_idx]` is `Some(hash)` when the base manifest
    /// has a chunk there; `None` for sparse / zero-filled holes.
    chunks: Vec<Option<ChunkHash>>,
    chunk_size: u64,
    total_bytes: u64,
}

impl PositionalDiskManifest {
    fn from_manifest(m: &Manifest) -> Result<Self, DiskBackendError> {
        if !matches!(m.kind, ManifestKind::Disk) {
            return Err(DiskBackendError::WrongKind(format!(
                "expected ManifestKind::Disk, got {:?}",
                m.kind
            )));
        }
        let chunk_size = m.chunk_size.as_u64();
        if chunk_size == 0 {
            return Err(DiskBackendError::InvalidManifest(
                "chunk_size is zero".into(),
            ));
        }
        let chunk_count = m.total_bytes.div_ceil(chunk_size) as usize;
        let mut chunks = vec![None; chunk_count];
        for entry in &m.chunks {
            if entry.offset % chunk_size != 0 {
                return Err(DiskBackendError::InvalidManifest(format!(
                    "chunk offset {} is not a multiple of chunk_size {}",
                    entry.offset, chunk_size,
                )));
            }
            let idx = (entry.offset / chunk_size) as usize;
            if idx >= chunks.len() {
                return Err(DiskBackendError::InvalidManifest(format!(
                    "chunk at offset {} (idx {idx}) exceeds total_bytes {}",
                    entry.offset, m.total_bytes,
                )));
            }
            chunks[idx] = Some(entry.hash);
        }
        Ok(Self {
            chunks,
            chunk_size,
            total_bytes: m.total_bytes,
        })
    }
}

/// Public data plane. One per FC sandbox.
///
/// Cheap to clone via `Arc` — the runtime task and a (future)
/// admin flush path both hold references.
pub struct ChunkedDiskBackend {
    /// Base + manifest_ref live under one lock so `flush()` can
    /// atomically rebase both: after a successful flush, the base's
    /// chunk hashes point at the freshly-uploaded chunks AND the
    /// manifest_ref ticks to the new version. Without this, an
    /// in-session post-flush read of just-rewritten content would
    /// re-fetch the old hash from the chunk store.
    state: Arc<Mutex<BackendState>>,
    chunk_size: u64,
    total_bytes: u64,
    cache: ChunkCache,
    store: Arc<ChunkStore>,
    /// ADR 0021 P2: in-memory LRU of clean chunks, so the guest's repeated
    /// `resume` reads of a hot chunk don't each re-read + re-sha256-verify the
    /// full 16 MiB off the (pd-balanced) chunk-cache disk. `std::sync::Mutex`
    /// (not tokio) because every access is a brief, non-awaiting get/put.
    mem_cache: Arc<std::sync::Mutex<ChunkMemCache>>,
    /// `chunk_idx -> dirty bytes`. Locked together so a concurrent
    /// read + write of the same chunk doesn't see torn state.
    dirty: Arc<Mutex<HashMap<usize, Vec<u8>>>>,
    /// ADR 0038 B3: chunks drained from `dirty` by `flush_local` (under
    /// the FC pause) but not yet uploaded to GCS by `flush_upload`
    /// (post-resume). `read_chunk` consults this tier between `dirty`
    /// and `base`, so a just-drained chunk stays readable until its
    /// upload lands + `base` is rebased — moving the multi-second GCS
    /// upload off the frozen-guest path. `chunk_idx -> (hash, bytes)`.
    pending_uploads: Arc<Mutex<HashMap<usize, (ChunkHash, Bytes)>>>,
    /// Unix-millis timestamp of the last successful `flush()`
    /// completion. `0` = never flushed since construction (the
    /// sentinel the diagnostic surface renders as "never").
    /// Read lock-free for the COW state RPC; written at the tail of
    /// `flush()` after the new manifest is durably published.
    last_flush_unix_ms: Arc<AtomicI64>,
    /// ADR 0016 Phase B: when dirty bytes monotonically cross
    /// `threshold_bytes` inside the dirty-lock, `ensure_dirty` pokes
    /// this Notify. The flush scheduler awaits `notified()` to wake
    /// out of its tick interval and trigger an early flush. Set to
    /// `u64::MAX` to disable threshold-driven wakeups (tests, or
    /// any caller that doesn't run a scheduler against this backend).
    threshold_notify: Arc<Notify>,
    /// ADR 0045 C1: while a migration export is open on this sandbox,
    /// `flush()` no-ops — the capture's drained manifest is the
    /// coherence cut the destination restores from, and a concurrent
    /// flush would publish a newer live_disk_manifest racing the
    /// destination's rebind (split-brain). Cleared on abort; moot on
    /// commit (sandbox destroyed).
    migration_fence: std::sync::atomic::AtomicBool,
    /// Issue #199: serialize the ENTIRE flush pipeline
    /// (`flush_local` drain → `flush_upload` GCS put → manifest
    /// rebuild/publish → `base` rebase) so two flushes can never run
    /// their upload/rebase phases concurrently. Without this only the
    /// `dirty` drain was serialized, and whichever flush rebased LAST
    /// won the published manifest + in-memory `base` regardless of data
    /// recency — an older flush's slow upload could finish after a newer
    /// flush published its chunk, overwriting it with the older hash and
    /// silently rolling back acked guest writes (same corruption class
    /// as the #191 teleport-canary-zeros family).
    ///
    /// Acquired by `flush_local` and carried, as an owned guard, inside
    /// [`PendingDiskFlush`] through to `flush_upload` (or to
    /// `requeue_pending` / drop on the no-publish migration path), so the
    /// drain and the publish that consumes it are one critical section.
    /// The synchronous `flush()` therefore holds it across its whole
    /// `flush_local` + `flush_upload`.
    ///
    /// Lock order: this is the OUTERMOST flush-path lock. It is acquired
    /// BEFORE `dirty` (in `flush_local`) and BEFORE `state`/`pending`
    /// (in `flush_upload`); never the reverse. The per-sandbox
    /// `capture_lock` (PooledBackend) sits ABOVE it — captures and
    /// migrations already serialize on that, and the scheduler `flush()`
    /// (which does not take `capture_lock`) serializes against them here.
    /// `flush()` checks `migration_fence` and returns BEFORE touching
    /// this lock, so a long-lived fenced migration that holds the guard
    /// across its export can never deadlock a scheduler tick.
    flush_pipeline: Arc<tokio::sync::Mutex<()>>,
    /// ADR 0045 C2 disk post-copy (destination): the sealed-chunk
    /// overlay — consulted between `pending` and `base` on the read
    /// path (and before the RMW base fetch on the write path). `None`
    /// in steady state and after the drain completes. `std` mutex:
    /// every access is a brief, non-awaiting clone of the `Arc`.
    post_copy: Arc<std::sync::Mutex<Option<Arc<PostCopyDiskOverlay>>>>,
    threshold_bytes: u64,
    /// ADR 0018 commit 12m: in-flight request counter for the NBD
    /// daemon's serve loop. Incremented on entering a write/read
    /// handler, decremented (with a `Notify::notify_waiters` on the
    /// 1→0 edge) on exit. `wait_idle()` parks on the notify until
    /// the count reads 0, giving the snapshot pipeline a barrier
    /// to drain in-flight virtio writes after FC has paused. Reads
    /// participate too (cheap, and lets future barriers cover them).
    in_flight: Arc<InFlightTracker>,

    /// ADR 0019: the lifecycle operation this sandbox's data plane is
    /// currently serving (cold boot / resume / …), if any. When active,
    /// `read_chunk` parents a `chunk.fetch` span on the operation so the
    /// page-in shows up in that operation's trace. Default = inactive
    /// (steady state → 0d metrics only).
    operation_scope: crate::trace_scope::OperationScope,

    /// Issue #204 regression test seam: an optional async barrier fired by
    /// `flush_local` AFTER it has moved the drained chunks into the held
    /// `pending` map but BEFORE it releases the `pending`+`dirty` locks —
    /// i.e. exactly the instant the pre-fix code left a chunk in NO tier.
    /// The test parks `flush_local` here and races a concurrent read/write
    /// of a drained index to prove the dirty→pending handoff is atomic.
    /// `None` in every non-test build/path (no runtime cost).
    #[cfg(test)]
    flush_local_handoff_seam: std::sync::Mutex<Option<FlushHandoffSeam>>,
}

/// Issue #204: the two-phase handshake a test installs to pause
/// `flush_local` at the dirty→pending handoff. `flush_local` signals
/// `arrived` once it reaches the seam (locks held), then awaits
/// `proceed`; the test releases `proceed` after it has launched the
/// racing read/write.
#[cfg(test)]
struct FlushHandoffSeam {
    arrived: Arc<Notify>,
    proceed: Arc<Notify>,
}

/// ADR 0018 commit 12m — see `ChunkedDiskBackend::in_flight`.
pub(crate) struct InFlightTracker {
    count: std::sync::atomic::AtomicUsize,
    notify: Notify,
}

impl InFlightTracker {
    fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    /// Increment + return a guard that decrements on drop. Use with
    /// `let _guard = tracker.enter();` at the top of each NBD handler.
    /// Only the Linux NBD daemon's `serve_loop` constructs guards;
    /// on macOS the runtime module is cfg-gated out, so the `enter`
    /// path is unreachable and the macOS clippy lane treats it as
    /// dead. The `dead_code` allow keeps the API stable across
    /// platforms — `wait_idle()` is still called from the
    /// PooledBackend snapshot path on both platforms (returns
    /// immediately when count is 0, which it always is on macOS
    /// where no daemon runs).
    #[allow(dead_code)]
    pub(crate) fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.count
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        InFlightGuard {
            tracker: self.clone(),
        }
    }

    /// Park until in-flight count drops to 0. Spurious wakeups
    /// (re-check the count) are handled by the inner loop. Returns
    /// immediately when count is already 0.
    pub(crate) async fn wait_idle(&self) {
        loop {
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            // Subscribe BEFORE the next count check to avoid the
            // classic "decrement-then-notify happens before we
            // subscribe" race. Notify gives one permit on
            // `notify_waiters`, so a subscribe-then-check loop
            // is the canonical idle wait shape.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[allow(dead_code)]
pub(crate) struct InFlightGuard {
    tracker: Arc<InFlightTracker>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let prev = self
            .tracker
            .count
            .fetch_sub(1, std::sync::atomic::Ordering::Release);
        if prev == 1 {
            // Last one out — wake any wait_idle parker.
            self.tracker.notify.notify_waiters();
        }
    }
}

struct BackendState {
    /// The RESOLVABLE manifest ref: always names a manifest that exists
    /// in the chunk store (the shared base at attach; this session's
    /// private vN after its first publish). Every out-of-process reader
    /// of `manifest_ref()` — snapshot capture, eviction finalize,
    /// cow-state — treats it as store-fetchable, so an unpublished
    /// placeholder must NEVER live here (review of #584: a session
    /// evicted before its first flush recorded a dangling ref, making
    /// it unevictable; the zero-dirty variant made snapshots
    /// permanently unresumable).
    manifest_ref: ManifestRef,
    /// ADR 0077 phase 2: the per-session manifest identity, minted at
    /// FRESH-CREATE ATTACH (not lazily on first flush — `fork_pending`
    /// is gone). The FIRST publish adopts it at v1, so concurrent
    /// same-base sessions never version the shared chain; until then
    /// `manifest_ref` stays the resolvable shared base. Cleared once
    /// adopted (and by a migration-destination rebase).
    fork_identity: Option<uuid::Uuid>,
    base: PositionalDiskManifest,
}

/// ADR 0021 P2: byte budget for the per-backend in-memory chunk cache.
/// The guest's `resume` I/O re-reads a handful of hot chunks (prod measured
/// chunk 0 — the ext4 superblock region — read 27× in a single restore);
/// 64 MiB comfortably holds that hot set at the 16 MiB disk chunk size while
/// bounding per-sandbox RAM.
const DISK_CHUNK_MEM_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// NBD durability: bound a single chunk fetch so a stuck origin GET can
/// never hang the NBD request forever. Without this, a fetch that never
/// returns (a black-holed GCS GET) leaves the guest's virtio-blk I/O in
/// uninterruptible sleep (jbd2/writeback D-state) AND prevents FC from
/// pausing the VM (`PATCH /vm` times out) — the wedged-session class.
/// On exhaustion the fetch returns an error, which `serve_loop` turns
/// into an NBD EIO: the guest's rootfs degrades to read-only but the VM
/// stays responsive (pausable, evictable). Bounded retries cover a
/// transient origin blip / GCS eventual-consistency without wedging
/// (~5 × 3s ≈ 15s worst case before EIO). A missing chunk should never
/// hang a VM; the chunk-GC pin-recheck fix prevents the missing chunk,
/// this is the defense-in-depth that keeps a miss from wedging the host.
const CHUNK_FETCH_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// Retry budget for a TRANSIENT fetch failure (origin 5xx / connection /
/// per-attempt timeout): the blob exists, so it's worth riding out a GCS
/// hiccup (~5 × backoff ≈ 15s) before failing the read with EIO.
const CHUNK_FETCH_MAX_ATTEMPTS: u32 = 5;
/// Retry budget for a definitive 404 (blob genuinely absent). Near-
/// permanent, so retry only enough to ride out GCS read-after-write
/// eventual consistency, then fail fast — no point spending the full
/// transient budget on a blob that isn't coming back.
const CHUNK_FETCH_NOTFOUND_MAX_ATTEMPTS: u32 = 2;
const CHUNK_FETCH_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Classify a chunk-fetch error: a definitive 404 (`BlobError::NotFound`,
/// which the GCS backend maps a 404 to) gets the short
/// [`CHUNK_FETCH_NOTFOUND_MAX_ATTEMPTS`] budget; everything else (5xx /
/// connection / per-attempt [`ChunkStoreError::FetchTimeout`]) gets the
/// fuller [`CHUNK_FETCH_MAX_ATTEMPTS`] since the blob is expected to exist.
fn chunk_fetch_is_not_found(e: &engram_chunk_store::error::ChunkStoreError) -> bool {
    matches!(
        e,
        engram_chunk_store::error::ChunkStoreError::Blob(engram_core::error::BlobError::NotFound)
    )
}

/// In-memory LRU of recently-read **clean** chunks, keyed by content hash.
///
/// Why this exists: a guest block read resolves to its containing chunk and
/// `read_chunk` pulls the WHOLE chunk through `ChunkCache::get` — which, on a
/// local hit, still reads the full chunk off the (pd-balanced, network-
/// attached) cache disk AND re-sha256-verifies all 16 MiB (~84 ms). With no
/// in-memory layer, a guest re-reading one hot chunk dozens of times during
/// `resume` paid that ~84 ms every time → seconds of substrate. This LRU makes
/// the 2nd..Nth read of a chunk a cheap RAM clone.
///
/// Content-addressed keys need no invalidation: a rewritten region produces a
/// fresh hash, so a stale entry is simply never looked up again and ages out.
struct ChunkMemCache {
    map: HashMap<ChunkHash, Bytes>,
    order: VecDeque<ChunkHash>,
    bytes: u64,
    budget_bytes: u64,
}

impl ChunkMemCache {
    fn new(budget_bytes: u64) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget_bytes,
        }
    }

    /// Return the chunk's bytes if cached, bumping it to most-recently-used.
    fn get(&mut self, hash: &ChunkHash) -> Option<Bytes> {
        let bytes = self.map.get(hash).cloned()?;
        if let Some(pos) = self.order.iter().position(|h| h == hash) {
            self.order.remove(pos);
        }
        self.order.push_back(*hash);
        Some(bytes)
    }

    /// Insert a chunk, evicting LRU entries until it fits the byte budget.
    fn put(&mut self, hash: ChunkHash, value: Bytes) {
        let len = value.len() as u64;
        if self.map.contains_key(&hash) || len > self.budget_bytes {
            return;
        }
        while self.bytes + len > self.budget_bytes {
            match self.order.pop_front() {
                Some(old) => {
                    if let Some(evicted) = self.map.remove(&old) {
                        self.bytes -= evicted.len() as u64;
                    }
                }
                None => break,
            }
        }
        self.bytes += len;
        self.map.insert(hash, value);
        self.order.push_back(hash);
    }
}

impl ChunkedDiskBackend {
    /// Build a backend rooted at `manifest_ref`. `cache` should be
    /// the host-agent's shared `ChunkCache` — that way base chunks
    /// stay warm across sessions of the same image. `store` is the
    /// underlying chunk store the cache wraps; the backend holds
    /// it directly so `flush()` can call `put_chunk` /
    /// `put_manifest` without going through the cache (cache is
    /// for reads only).
    pub async fn from_blob(
        manifest_ref: ManifestRef,
        cache: ChunkCache,
        store: Arc<ChunkStore>,
        threshold_bytes: u64,
    ) -> Result<Self, DiskBackendError> {
        let manifest = store.get_manifest(manifest_ref).await?;
        Self::from_manifest(manifest_ref, &manifest, cache, store, threshold_bytes)
    }

    /// ADR 0045 C1: construct from manifest CONTENT the caller already
    /// holds — a migration destination attaches a not-yet-durable disk
    /// manifest delivered inline (its chunks pre-pulled into the local
    /// cache); the store is only the fallthrough for base chunks.
    pub fn from_manifest(
        manifest_ref: ManifestRef,
        manifest: &engram_chunk_store::Manifest,
        cache: ChunkCache,
        store: Arc<ChunkStore>,
        threshold_bytes: u64,
    ) -> Result<Self, DiskBackendError> {
        let base = PositionalDiskManifest::from_manifest(manifest)?;
        let chunk_size = base.chunk_size;
        let total_bytes = base.total_bytes;
        Ok(Self {
            state: Arc::new(Mutex::new(BackendState {
                manifest_ref,
                fork_identity: None,
                base,
            })),
            chunk_size,
            total_bytes,
            cache,
            store,
            mem_cache: Arc::new(std::sync::Mutex::new(ChunkMemCache::new(
                DISK_CHUNK_MEM_BUDGET_BYTES,
            ))),
            dirty: Arc::new(Mutex::new(HashMap::new())),
            pending_uploads: Arc::new(Mutex::new(HashMap::new())),
            last_flush_unix_ms: Arc::new(AtomicI64::new(0)),
            threshold_notify: Arc::new(Notify::new()),
            migration_fence: std::sync::atomic::AtomicBool::new(false),
            flush_pipeline: Arc::new(tokio::sync::Mutex::new(())),
            post_copy: Arc::new(std::sync::Mutex::new(None)),
            threshold_bytes,
            in_flight: Arc::new(InFlightTracker::new()),
            operation_scope: crate::trace_scope::OperationScope::default(),
            #[cfg(test)]
            flush_local_handoff_seam: std::sync::Mutex::new(None),
        })
    }

    /// Build from an already-loaded `Manifest`. Used by unit tests
    /// to avoid the round-trip through the chunk store.
    pub fn new(
        manifest_ref: ManifestRef,
        manifest: &Manifest,
        cache: ChunkCache,
        store: Arc<ChunkStore>,
        threshold_bytes: u64,
    ) -> Result<Self, DiskBackendError> {
        let base = PositionalDiskManifest::from_manifest(manifest)?;
        let chunk_size = base.chunk_size;
        let total_bytes = base.total_bytes;
        Ok(Self {
            state: Arc::new(Mutex::new(BackendState {
                manifest_ref,
                fork_identity: None,
                base,
            })),
            chunk_size,
            total_bytes,
            cache,
            store,
            mem_cache: Arc::new(std::sync::Mutex::new(ChunkMemCache::new(
                DISK_CHUNK_MEM_BUDGET_BYTES,
            ))),
            dirty: Arc::new(Mutex::new(HashMap::new())),
            pending_uploads: Arc::new(Mutex::new(HashMap::new())),
            last_flush_unix_ms: Arc::new(AtomicI64::new(0)),
            threshold_notify: Arc::new(Notify::new()),
            migration_fence: std::sync::atomic::AtomicBool::new(false),
            flush_pipeline: Arc::new(tokio::sync::Mutex::new(())),
            post_copy: Arc::new(std::sync::Mutex::new(None)),
            threshold_bytes,
            in_flight: Arc::new(InFlightTracker::new()),
            operation_scope: crate::trace_scope::OperationScope::default(),
            #[cfg(test)]
            flush_local_handoff_seam: std::sync::Mutex::new(None),
        })
    }

    /// ADR 0018 commit 12m: hand out a clone of the in-flight tracker
    /// for the NBD daemon's `serve_loop` to record per-request
    /// guards against. `Arc`-cloned so the daemon's spawned task
    /// keeps a handle for the device's lifetime. The `dead_code`
    /// allow covers macOS where the runtime is cfg-gated off;
    /// `wait_idle()` below is the cross-platform accessor.
    #[allow(dead_code)]
    pub(crate) fn in_flight_tracker(&self) -> Arc<InFlightTracker> {
        self.in_flight.clone()
    }

    /// ADR 0018 commit 12m: park until every NBD request currently
    /// being handled has finished. The snapshot pipeline calls this
    /// after `inner.pause()` and before `flush()` — pause stops the
    /// guest's vCPUs, this drains the virtio → kernel-NBD →
    /// userspace-daemon pipeline so flush sees the complete
    /// just-quiesced disk state. No-op when nothing is in flight.
    pub async fn wait_idle(&self) {
        self.in_flight.wait_idle().await;
    }

    /// ADR 0016 Phase B accessor: hand out a clone of the threshold
    /// `Notify` so the flush scheduler can park on `.notified()` and
    /// wake when dirty bytes cross the threshold mid-write. One
    /// permit max — multiple concurrent crossings collapse to one
    /// flush, which is correct: a single flush drains the full
    /// dirty buffer regardless of how many writers piled in.
    pub fn threshold_notify(&self) -> Arc<Notify> {
        self.threshold_notify.clone()
    }

    /// Current threshold in bytes. Pinned at construction; reported
    /// by the COW state RPC if we want it visible to operators
    /// later (not wired into the RPC schema today, but stable to
    /// expose).
    pub fn threshold_bytes(&self) -> u64 {
        self.threshold_bytes
    }

    /// Bytes per chunk. NBD reads / writes that span chunk boundaries
    /// fan out into per-chunk operations internally. Pinned at
    /// construction; flush doesn't change it.
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Total virtual-disk size. Reported back to the kernel via
    /// `NBD_SET_SIZE_BLOCKS` during the daemon's startup dance.
    /// Pinned at construction; flush doesn't change it.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Current manifest ref. Reads the state under the lock —
    /// after a successful `flush()` the ref ticks to the new
    /// version atomically with the base-chunk hashes the lock
    /// guards.
    pub async fn manifest_ref(&self) -> ManifestRef {
        self.state.lock().await.manifest_ref
    }

    /// Number of dirty chunks currently buffered in RAM. ADR 0016
    /// COW diagnostic — read by `PooledBackend::cow_state` for the
    /// per-session/per-host endpoint. Locks `dirty` briefly; cheap
    /// since the map is small (<< 1024 entries in realistic
    /// workloads per the module docstring's analysis).
    pub async fn dirty_chunks_count(&self) -> usize {
        self.dirty.lock().await.len()
    }

    /// Total bytes resident in the dirty buffer. Sum of the
    /// per-chunk vec lengths under the same lock as
    /// [`Self::dirty_chunks_count`]. Used by the COW diagnostic AND
    /// (Phase B) the flush-scheduler threshold check.
    pub async fn dirty_bytes(&self) -> u64 {
        self.dirty
            .lock()
            .await
            .values()
            .map(|v| v.len() as u64)
            .sum()
    }

    /// Unix-millis timestamp of the most recent successful `flush()`
    /// completion (any outcome, including zero-chunk flushes — the
    /// signal is "we last verified durability at time T", not "we
    /// last had something to flush"). `0` if `flush()` has never
    /// completed since construction. Lock-free read via `Acquire`.
    pub fn last_flush_unix_ms(&self) -> i64 {
        self.last_flush_unix_ms.load(Ordering::Acquire)
    }

    /// Read `length` bytes from `offset`. Fans out across chunk
    /// boundaries; each chunk read serves from the dirty buffer
    /// if present, otherwise fetches via the L1 cache.
    pub async fn read(&self, offset: u64, length: u64) -> Result<Bytes, DiskBackendError> {
        if offset.saturating_add(length) > self.total_bytes {
            return Err(DiskBackendError::OutOfRange {
                offset,
                length,
                total: self.total_bytes,
            });
        }
        if length == 0 {
            return Ok(Bytes::new());
        }
        use futures::stream::{self, StreamExt, TryStreamExt};

        let chunk_size = self.chunk_size;
        let end = offset + length;
        // One descriptor per chunk this read spans: (chunk_idx, served chunk
        // width, intra-chunk start, bytes to take). `intra..intra+take` is
        // the slice this read wants from the fetched chunk.
        let mut descriptors: Vec<(usize, u64, usize, usize)> = Vec::new();
        let mut cursor = offset;
        while cursor < end {
            let chunk_idx = (cursor / chunk_size) as usize;
            let chunk_start = (chunk_idx as u64) * chunk_size;
            let chunk_end = std::cmp::min(chunk_start + chunk_size, self.total_bytes);
            let intra = (cursor - chunk_start) as usize;
            let take = (std::cmp::min(end, chunk_end) - cursor) as usize;
            descriptors.push((chunk_idx, chunk_end - chunk_start, intra, take));
            cursor += take as u64;
        }

        // Fan the per-chunk fetches out concurrently (bounded) and reassemble
        // IN ORDER — `buffered` preserves order. A single-chunk read (the
        // common case) runs exactly one fetch, same as the prior serial path.
        // Collect the futures eagerly (rather than `stream::iter(map(..))`) so
        // the borrowing closure isn't stored in the combinator — that form
        // trips a higher-ranked-lifetime `Send` error in the spawned handler.
        let fetches: Vec<_> = descriptors
            .iter()
            .map(|&(chunk_idx, read_len, _, _)| self.read_chunk(chunk_idx, read_len))
            .collect();
        let fetched: Vec<Bytes> = stream::iter(fetches)
            .buffered(DISK_READ_FETCH_CONCURRENCY)
            .try_collect()
            .await?;

        let mut out = Vec::with_capacity(length as usize);
        for (&(_, _, intra, take), chunk_bytes) in descriptors.iter().zip(fetched.iter()) {
            out.extend_from_slice(&chunk_bytes[intra..intra + take]);
        }
        Ok(Bytes::from(out))
    }

    /// Write `data` at `offset`. Idempotent on the same byte range
    /// — last writer wins. Materialises the affected chunks into
    /// the dirty buffer on first touch (copy from base, then patch).
    ///
    /// ADR 0018 commit 12m: insert + patch happen under a single
    /// dirty-lock acquisition. The pre-commit-12m shape used three
    /// acquisitions (ensure_dirty's two + a re-acquire to patch),
    /// which let `flush()` interleave between the last insert and
    /// the patch — `dirty.get_mut(...).expect(...)` could panic AND
    /// the patched bytes could land in the dirty map after flush's
    /// drain, missing the published manifest. The cross-host evac
    /// canary md5 mismatch surfaced this. The fetch of base bytes
    /// (the slow part) still happens outside any lock; only the
    /// insert-if-missing + patch are inside the critical section.
    pub async fn write(&self, offset: u64, data: &[u8]) -> Result<(), DiskBackendError> {
        let length = data.len() as u64;
        if offset.saturating_add(length) > self.total_bytes {
            return Err(DiskBackendError::OutOfRange {
                offset,
                length,
                total: self.total_bytes,
            });
        }
        if length == 0 {
            return Ok(());
        }
        let chunk_size = self.chunk_size;
        let mut cursor = offset;
        let mut src_off = 0usize;
        let end = offset + length;
        while cursor < end {
            let chunk_idx = (cursor / chunk_size) as usize;
            let chunk_start = (chunk_idx as u64) * chunk_size;
            let chunk_end = std::cmp::min(chunk_start + chunk_size, self.total_bytes);
            let intra = (cursor - chunk_start) as usize;
            let take_u64 = std::cmp::min(end, chunk_end) - cursor;
            let take = take_u64 as usize;
            let chunk_len = (chunk_end - chunk_start) as usize;
            self.write_chunk(chunk_idx, chunk_len, intra, &data[src_off..src_off + take])
                .await?;
            cursor += take_u64;
            src_off += take;
        }
        Ok(())
    }

    /// Patch one chunk's dirty buffer in a single critical section.
    /// If the chunk isn't in the dirty map yet, fetch its base bytes
    /// OUTSIDE the lock (slow, network), then take the dirty lock
    /// ONCE and either (a) insert the prefetched base and patch in
    /// place, or (b) patch the existing entry that a racing writer
    /// installed while we were fetching. Either way the patch lands
    /// atomically w.r.t. `flush()`'s drain.
    async fn write_chunk(
        &self,
        chunk_idx: usize,
        chunk_len: usize,
        intra: usize,
        payload: &[u8],
    ) -> Result<(), DiskBackendError> {
        // ADR 0045 C2 disk post-copy: a partial write to a SEALED
        // chunk must RMW against the peer's content (the source's
        // dirty bytes), never against `base`'s pre-divergence hash.
        // Materialize first — the bytes land in `dirty`, so the
        // normal flow below finds them via the pre-check.
        if let Some(ov) = self.postcopy_overlay() {
            self.postcopy_materialize(&ov, chunk_idx).await?;
        }
        // Cheap pre-check: if the chunk is already in dirty, skip
        // the base fetch entirely. Holding the dirty lock briefly
        // here is fine — a HashMap::contains_key is constant-time.
        //
        // Issue #204 lock order: like `read_chunk`, this path takes `dirty`
        // and `pending` SEQUENTIALLY (each scoped guard released before the
        // next is acquired) — never nested. `flush_local` nests
        // pending⟶dirty; a `dirty`-then-`pending` nest here would invert it
        // and could deadlock, so keep these acquisitions disjoint.
        let already_present = self.dirty.lock().await.contains_key(&chunk_idx);
        let prefetched_base: Option<Vec<u8>> = if already_present {
            None
        } else {
            // The RMW base MUST honor the same tier order as
            // `read_chunk`: pending BEFORE base. A flush drains
            // dirty→pending and only rebases `base` after the upload
            // lands (multi-second in prod), so a write arriving in
            // that window would otherwise rebuild the chunk from the
            // PRE-FLUSH base and silently drop everything the drain
            // just captured — the teleport-canary "zeros where the
            // session's data should be" corruption, reproduced by
            // the two-host NBD e2e (an 8 MiB write crossing the
            // flush threshold mid-stream loses its first wave).
            let pending_bytes = self
                .pending_uploads
                .lock()
                .await
                .get(&chunk_idx)
                .map(|(_hash, bytes)| bytes.to_vec());
            if let Some(b) = pending_bytes {
                Some(b)
            } else {
                // Fetch base bytes OUTSIDE the dirty lock. Another writer
                // may insert the same chunk while we're awaiting the
                // chunk-store fetch; we handle that under the lock below
                // (the racing-writer branch).
                let hash = self
                    .state
                    .lock()
                    .await
                    .base
                    .chunks
                    .get(chunk_idx)
                    .copied()
                    .flatten();
                let base_bytes = match hash {
                    Some(hash) => self.cache.get(hash, || self.store.get_chunk(hash)).await?,
                    None => Bytes::from(vec![0u8; chunk_len]),
                };
                Some(base_bytes.to_vec())
            }
        };

        // Single critical section: ensure entry exists, patch in
        // place. `flush()` either runs entirely before this lock
        // acquisition (drains an old dirty map, ticks the manifest,
        // we then dirty a fresh chunk against the new base) OR runs
        // entirely after (our patch lands in the dirty map before
        // the drain, gets included in the published manifest).
        // There's no in-between state where flush sees half a write.
        let crossed = {
            let mut dirty = self.dirty.lock().await;
            let before: u64 = dirty.values().map(|v| v.len() as u64).sum();
            let inserted_len = match dirty.entry(chunk_idx) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut()[intra..intra + payload.len()].copy_from_slice(payload);
                    0u64
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let mut buf = match prefetched_base {
                        Some(b) => b,
                        // Unreachable in practice: `already_present` was
                        // false, so we definitely fetched a base above.
                        // Bail out cleanly rather than panic if the
                        // assumption ever changes.
                        None => {
                            return Err(DiskBackendError::InvariantViolation(
                                "write_chunk: vacant entry with no prefetched base".into(),
                            ))
                        }
                    };
                    buf[intra..intra + payload.len()].copy_from_slice(payload);
                    let len = buf.len() as u64;
                    slot.insert(buf);
                    len
                }
            };
            let after = before + inserted_len;
            before < self.threshold_bytes && after >= self.threshold_bytes
        };
        if crossed {
            self.threshold_notify.notify_one();
        }
        Ok(())
    }

    /// Flush dirty chunks to the chunk store and tick the manifest
    /// version. The new `ManifestRef` is the durability gate the
    /// snapshot path attaches to `SnapshotRecord.disk_manifest`.
    ///
    /// After flush, the dirty buffer is cleared and reads of those
    /// regions go through the new manifest entries via the chunk
    /// cache like any other base chunk. The post-flush state is
    /// indistinguishable from "fresh session against the new
    /// manifest version."
    /// Full synchronous flush = `flush_local` + `flush_upload` back to
    /// back. Used by the background scheduler + the SIGTERM/drain paths,
    /// where there's no FC resume to overlap with, so the historical
    /// drain→upload→rebase behavior is preserved.
    pub async fn flush(&self) -> Result<DiskFlushOutcome, DiskBackendError> {
        if self
            .migration_fence
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // Migration capture in flight: report a no-op flush (the
            // scheduler skips the publish on chunks_flushed == 0).
            return Ok(DiskFlushOutcome {
                manifest_ref: self.state.lock().await.manifest_ref,
                chunks_flushed: 0,
                bytes_uploaded: 0,
            });
        }
        let pending = self.flush_local().await?;
        self.flush_upload(pending).await
    }

    /// ADR 0045 C1 (destination): point the backend at the manifest
    /// ref the durability catch-up actually published (the provisional
    /// ref can lose the shared-lineage version race). Future flushes
    /// chain from here.
    pub async fn rebase_manifest_ref(&self, manifest_ref: ManifestRef) {
        let mut state = self.state.lock().await;
        state.manifest_ref = manifest_ref;
        state.fork_identity = None;
    }

    /// ADR 0049 follow-up: arm the lazy per-session manifest fork. Called
    /// on the fresh-create attach path (where the backend is born on the
    /// SHARED base `manifest_id`) so the first flush mints a private id
    /// instead of versioning the shared base. See [`BackendState::fork_pending`].
    /// ADR 0077 phase 2: fork the manifest identity NOW (fresh-create
    /// attach) — mint the private `manifest_id` the FIRST publish adopts
    /// at v1, whose base chunks stay the deduped+pinned enabled-image
    /// set. `manifest_ref` deliberately stays the SHARED BASE ref until
    /// that publish: it is the resolvable pointer every out-of-process
    /// consumer (snapshot capture, eviction finalize, cow-state) fetches
    /// from the store, and a pre-publish placeholder here left sessions
    /// evicted before their first flush unevictable / unresumable.
    /// Idempotent-safe: only the fresh-create call site invokes it,
    /// exactly once, before any flush.
    pub async fn fork_manifest_identity(&self) {
        self.state.lock().await.fork_identity = Some(uuid::Uuid::new_v4());
    }

    /// ADR 0045 C1: see `migration_fence`.
    pub fn set_migration_fence(&self, fenced: bool) {
        self.migration_fence
            .store(fenced, std::sync::atomic::Ordering::SeqCst);
    }

    /// ADR 0038 B3 — phase 1 (runs under the FC pause on the snapshot
    /// path): drain the dirty buffer, hash each chunk locally, stash the
    /// bytes in `pending_uploads`. NO network, NO `base` rebase — those
    /// happen in `flush_upload` after the guest resumes, moving the
    /// multi-second GCS upload off the frozen-guest path. The drain
    /// still captures disk-at-the-pause-instant (ADR 0018 §12m).
    pub async fn flush_local(&self) -> Result<PendingDiskFlush, DiskBackendError> {
        // Issue #199: take the flush-pipeline guard BEFORE the `dirty`
        // lock (lock order: flush_pipeline ⟶ dirty ⟶ state/pending) and
        // hand it off inside the returned `PendingDiskFlush`, so the
        // drain and the `flush_upload` that publishes its chunks are one
        // serialized critical section. Without this, a slow upload from
        // an EARLIER drain could publish/rebase after a LATER drain
        // already did, overwriting the newer chunk with the older hash.
        let flush_guard = self.flush_pipeline.clone().lock_owned().await;
        // Issue #204: the dirty→pending handoff must be ATOMIC w.r.t.
        // readers/writers. Acquire the `pending` lock BEFORE draining
        // `dirty`, and hold BOTH across the drain+insert move, so a drained
        // chunk is never absent from both tiers for any instant. The old
        // code drained + dropped the dirty lock, then took `pending`
        // separately — between those two acquisitions a chunk lived in NO
        // tier: a concurrent `read_chunk` fell through to the stale `base`
        // (transient stale read), and a concurrent `write_chunk` RMW'd from
        // the stale base and re-inserted into `dirty`, permanently shadowing
        // the just-drained bytes (silent lost write on the next publish).
        //
        // Lock order on the flush path: flush_pipeline ⟶ pending ⟶ dirty.
        // This is safe against `read_chunk`/`write_chunk`, which take
        // `dirty` and `pending` only SEQUENTIALLY (each released before the
        // other is acquired) — they never NEST the two in any order — so
        // there is no opposite-order nested acquisition to deadlock against.
        let mut pending = self.pending_uploads.lock().await;
        let mut dirty_guard = self.dirty.lock().await;
        if dirty_guard.is_empty() {
            // Nothing to publish ⟹ nothing to serialize; release the
            // pipeline guard immediately (don't carry it through an
            // empty no-op flush_upload, which would needlessly block a
            // concurrent flush).
            drop(dirty_guard);
            drop(pending);
            drop(flush_guard);
            return Ok(PendingDiskFlush {
                new_chunks: Vec::new(),
                flush_guard: None,
            });
        }
        let mut new_chunks: Vec<(usize, ChunkHash, Bytes)> = Vec::with_capacity(dirty_guard.len());
        for (chunk_idx, bytes) in dirty_guard.drain() {
            let bytes = Bytes::from(bytes);
            // Local hash — identical to what `put_chunk` computes, so the
            // manifest `flush_upload` builds is consistent with the
            // bytes it uploads.
            let hash = ChunkHash::of(&bytes);
            // Mirror into the pending tier for read-survival during the
            // flush window. `flush_upload` uploads from `new_chunks`'s own
            // bytes (carried below), NOT from this shared map — so a
            // concurrent flush clearing `pending[idx]` can't make us skip
            // a put. Because we still hold the `dirty` lock here, the chunk
            // is in `dirty` (until `drain` consumes it) and lands in
            // `pending` under the same critical section — never in neither.
            pending.insert(chunk_idx, (hash, bytes.clone()));
            new_chunks.push((chunk_idx, hash, bytes));
        }
        // Issue #204 regression seam: fire while BOTH locks are still held,
        // i.e. at the exact instant the pre-fix code left a chunk tier-less.
        #[cfg(test)]
        {
            let seam = self.flush_local_handoff_seam.lock().unwrap().take();
            if let Some(seam) = seam {
                seam.arrived.notify_one();
                let proceed = seam.proceed.notified();
                tokio::pin!(proceed);
                proceed.await;
            }
        }
        drop(dirty_guard);
        drop(pending);
        Ok(PendingDiskFlush {
            new_chunks,
            flush_guard: Some(flush_guard),
        })
    }

    /// Issue #204 test-only: arm the `flush_local` dirty→pending handoff
    /// seam. The returned `(arrived, proceed)` pair lets a test park
    /// `flush_local` at the handoff (both locks held) and then race a
    /// concurrent read/write. `arrived` fires once `flush_local` reaches
    /// the seam; `flush_local` blocks until the test notifies `proceed`.
    #[cfg(test)]
    fn arm_flush_handoff_seam(&self) -> (Arc<Notify>, Arc<Notify>) {
        let arrived = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());
        *self.flush_local_handoff_seam.lock().unwrap() = Some(FlushHandoffSeam {
            arrived: arrived.clone(),
            proceed: proceed.clone(),
        });
        (arrived, proceed)
    }

    /// ADR 0038 B3 — phase 2 (runs post-resume on the snapshot path):
    /// upload the stashed chunks to GCS, then rebuild + publish the
    /// manifest and rebase `base`. Keeping the rebase *after* the upload
    /// preserves "a manifest someone restores from ⟹ its chunks are
    /// durable" — the background scheduler reads `base`, so it never
    /// references a not-yet-uploaded chunk. Uploaded entries are cleared
    /// from `pending_uploads` on success (reads fall through to the now-
    /// rebased `base`). A failed upload leaves them in `pending` — reads
    /// stay correct; they're reclaimed on `destroy`.
    /// ADR 0045 C1: the migration flavor of `flush_upload` — land the
    /// drained chunks in the host-local NVMe cache ONLY (no GCS PUT on
    /// the teleport pause path) and return the post-drain manifest
    /// WITHOUT publishing or rebasing. The manifest's chunks are
    /// reachable through the cache for the destination's pull; the
    /// destination's durability catch-up uploads + publishes later.
    /// The source's own `state` is untouched: on commit the VM is
    /// destroyed; on abort call [`Self::requeue_pending`] first.
    ///
    /// Returns `(manifest, new_hashes)` — `new_hashes` is the
    /// pending-tier transfer set the destination must pull.
    pub async fn flush_to_local_cache(
        &self,
        pending: &PendingDiskFlush,
    ) -> Result<(Manifest, Vec<ChunkHash>), DiskBackendError> {
        let chunk_size = self.chunk_size;
        let mut new_hashes = Vec::with_capacity(pending.new_chunks.len());
        for (_idx, hash, bytes) in &pending.new_chunks {
            // No per-write sweep — batch-closing sweep below.
            self.cache
                .put_no_evict(*hash, bytes)
                .await
                .map_err(DiskBackendError::Chunk)?;
            new_hashes.push(*hash);
        }
        self.cache.sweep().await.map_err(DiskBackendError::Chunk)?;
        let state = self.state.lock().await;
        let mut chunks: Vec<ChunkRef> = state
            .base
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                h.map(|hash| ChunkRef {
                    offset: (i as u64) * chunk_size,
                    hash,
                })
            })
            .collect();
        for (idx, hash, _bytes) in &pending.new_chunks {
            let offset = (*idx as u64) * chunk_size;
            if let Some(existing) = chunks.iter_mut().find(|c| c.offset == offset) {
                existing.hash = *hash;
            } else {
                chunks.push(ChunkRef {
                    offset,
                    hash: *hash,
                });
            }
        }
        chunks.sort_by_key(|c| c.offset);
        Ok((
            Manifest {
                schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
                kind: ManifestKind::Disk,
                chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(chunk_size),
                total_bytes: self.total_bytes,
                chunks,
                parent: Some(state.manifest_ref),
                working_set_trace: None,
                annotations: serde_json::Value::Null,
            },
            new_hashes,
        ))
    }

    /// ADR 0045 C1 abort path: put the drained-but-never-uploaded
    /// chunks back into `dirty` so the resumed guest's next flush
    /// retries them (a newer guest write wins — same rule as the
    /// upload-failure re-queue). Idempotent.
    pub async fn requeue_pending(&self, pending: PendingDiskFlush) {
        let mut dirty = self.dirty.lock().await;
        for (idx, _hash, bytes) in pending.new_chunks {
            dirty.entry(idx).or_insert_with(|| bytes.to_vec());
        }
    }

    /// ADR 0045 C2 disk post-copy — the SOURCE seal (runs under the
    /// FC pause; the blackout's disk leg). Snapshots every chunk whose
    /// guest-visible content is NOT reproducible from the published
    /// base manifest: the dirty buffer (drained — zero-copy `Vec`→
    /// `Bytes` moves; the guest is paused and `wait_idle`'d, and the
    /// abort path re-queues) plus the pending-uploads tier (refcounted
    /// `Bytes` clones — left in place; an in-flight pre-pause
    /// `flush_upload` may still complete and clear them, which is
    /// benign because the seal owns its own references). NO hashing,
    /// NO cache writes, NO manifest mint — this is what deleted the
    /// 1.3 s re-chunk from the blackout.
    ///
    /// Returns the seal plus the CURRENT published base manifest
    /// (content + ref). Every hash in that manifest is durable (the
    /// `flush_upload` invariant: `base` rebases only after uploads
    /// land), so the destination rebases to it and resolves non-sealed
    /// chunks via cache/GCS; sealed indices override via the peer.
    /// `guest content == manifest ⊕ seal` at the freeze instant.
    pub async fn seal_for_postcopy(&self) -> (PostCopyDiskSeal, Manifest, ManifestRef) {
        // Pending first (older tier), then dirty drained on top
        // (newer wins — same precedence as `read_chunk`).
        let mut chunks: HashMap<usize, Bytes> = self
            .pending_uploads
            .lock()
            .await
            .iter()
            .map(|(idx, (_hash, bytes))| (*idx, bytes.clone()))
            .collect();
        {
            let mut dirty = self.dirty.lock().await;
            for (idx, buf) in dirty.drain() {
                chunks.insert(idx, Bytes::from(buf));
            }
        }
        let state = self.state.lock().await;
        let chunk_size = self.chunk_size;
        let manifest = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(chunk_size),
            total_bytes: self.total_bytes,
            chunks: state
                .base
                .chunks
                .iter()
                .enumerate()
                .filter_map(|(i, h)| {
                    h.map(|hash| ChunkRef {
                        offset: (i as u64) * chunk_size,
                        hash,
                    })
                })
                .collect(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        (PostCopyDiskSeal { chunks }, manifest, state.manifest_ref)
    }

    /// ADR 0045 C2 disk post-copy abort path: the move failed before
    /// the dest took over — put the sealed bytes back into `dirty` so
    /// the resumed guest's next flush captures them. Re-queuing a
    /// pending-tier-sourced chunk is benign (content-addressed re-
    /// upload of identical bytes). Idempotent; `or_insert` so a newer
    /// guest write always wins.
    pub async fn requeue_postcopy_seal(&self, seal: &PostCopyDiskSeal) {
        let mut dirty = self.dirty.lock().await;
        for (idx, bytes) in &seal.chunks {
            dirty.entry(*idx).or_insert_with(|| bytes.to_vec());
        }
    }

    /// ADR 0045 C2 disk post-copy — the DESTINATION install. Rebases
    /// `base` + `manifest_ref` to the source's published view (shipped
    /// inline with the seal; all its hashes are durable) and arms the
    /// overlay: reads/writes touching a sealed index demand-fetch the
    /// raw chunk from the frozen source and land it in `dirty` —
    /// exactly the state the source held, so the next periodic flush
    /// is the durability catch-up. Must run BEFORE `state.bin` lands
    /// (the FC load gate): once the guest runs, every sealed read is
    /// peer-authoritative.
    ///
    /// Returns the drain-completion subscription for
    /// `migration_drain_wait` (fires `Ok(installed)` or `Err(detail)`).
    pub async fn install_postcopy_overlay(
        &self,
        manifest: &Manifest,
        manifest_ref: ManifestRef,
        sealed_indices: &[u64],
        fetcher: Arc<dyn PostCopyDiskFetcher>,
    ) -> Result<PostCopyDrainSubscription, DiskBackendError> {
        let base = PositionalDiskManifest::from_manifest(manifest)?;
        {
            let mut state = self.state.lock().await;
            state.base = base;
            state.manifest_ref = manifest_ref;
        }
        let (done_tx, done_rx) = tokio::sync::watch::channel(None);
        let overlay = Arc::new(PostCopyDiskOverlay {
            sealed: Mutex::new(sealed_indices.iter().map(|i| *i as usize).collect()),
            fetcher,
            lost: std::sync::atomic::AtomicBool::new(false),
            done_tx,
        });
        *self.post_copy.lock().expect("post_copy poisoned") = Some(overlay);
        Ok(done_rx)
    }

    /// The installed overlay, if a disk post-copy is in flight.
    fn postcopy_overlay(&self) -> Option<Arc<PostCopyDiskOverlay>> {
        self.post_copy.lock().expect("post_copy poisoned").clone()
    }

    /// Subscribe to the in-flight drain's terminal outcome. `None`
    /// when no overlay is installed — either this sandbox never was a
    /// post-copy destination, or the drain already completed and
    /// cleared the overlay; both mean "nothing to wait for".
    pub fn postcopy_drain_subscribe(&self) -> Option<PostCopyDrainSubscription> {
        self.postcopy_overlay().map(|ov| ov.done_tx.subscribe())
    }

    /// Materialize one sealed chunk: fetch the raw bytes from the
    /// frozen source, install into `dirty` (`or_insert` — a racing
    /// guest write or drain install wins), unseal. Returns
    /// `Ok(None)` when the index isn't sealed (the caller falls
    /// through to the normal tiers). A terminal fetch failure latches
    /// `lost` — the peer is gone with sealed content outstanding; the
    /// drain reports `PeerLost` and the coordinator rewinds the VM.
    async fn postcopy_materialize(
        &self,
        ov: &Arc<PostCopyDiskOverlay>,
        chunk_idx: usize,
    ) -> Result<Option<Bytes>, DiskBackendError> {
        if !ov.sealed.lock().await.contains(&chunk_idx) {
            return Ok(None);
        }
        if ov.lost.load(Ordering::SeqCst) {
            return Err(DiskBackendError::Chunk(
                engram_chunk_store::error::ChunkStoreError::Internal(format!(
                    "post-copy disk peer lost; sealed chunk {chunk_idx} unfillable"
                )),
            ));
        }
        let bytes = match ov.fetcher.fetch(chunk_idx as u64).await {
            Ok(b) => b,
            Err(e) => {
                ov.lost.store(true, Ordering::SeqCst);
                return Err(DiskBackendError::Chunk(
                    engram_chunk_store::error::ChunkStoreError::Internal(format!(
                        "post-copy disk peer fetch chunk {chunk_idx}: {e}"
                    )),
                ));
            }
        };
        let installed = {
            let mut dirty = self.dirty.lock().await;
            match dirty.entry(chunk_idx) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(bytes.to_vec());
                    bytes
                }
                // A concurrent materialize (drain vs. fault) or a
                // guest write landed first — theirs is at least as
                // new; serve it.
                std::collections::hash_map::Entry::Occupied(e) => Bytes::copy_from_slice(e.get()),
            }
        };
        // Unseal AFTER the install: once unsealed, readers resolve via
        // `dirty` and must find the bytes there.
        ov.sealed.lock().await.remove(&chunk_idx);
        Ok(Some(installed))
    }

    /// Background drain: pull every still-sealed chunk from the source
    /// (bounded fan-out), then unfence the publisher and clear the
    /// overlay — the disk no longer depends on the peer. On failure
    /// the overlay stays (lost-latched) and the subscription reports
    /// the error; `migration_drain_wait` turns it into `PeerLost`.
    pub fn spawn_postcopy_drain(self: Arc<Self>) {
        tokio::spawn(async move {
            let Some(ov) = self.postcopy_overlay() else {
                return;
            };
            let started = std::time::Instant::now();
            let indices: Vec<usize> = ov.sealed.lock().await.iter().copied().collect();
            let total = indices.len();
            use futures::StreamExt;
            let mut pulls = futures::stream::iter(indices.into_iter().map(|idx| {
                let backend = self.clone();
                let ov = ov.clone();
                async move { backend.postcopy_materialize(&ov, idx).await.map(|_| ()) }
            }))
            .buffer_unordered(POSTCOPY_DISK_DRAIN_CONCURRENCY);
            let mut failure: Option<String> = None;
            while let Some(r) = pulls.next().await {
                if let Err(e) = r {
                    failure = Some(e.to_string());
                    break;
                }
            }
            drop(pulls);
            let result = match failure {
                None => {
                    // Self-sufficient: durability rides the normal
                    // flush path from here.
                    self.set_migration_fence(false);
                    *self.post_copy.lock().expect("post_copy poisoned") = None;
                    tracing::info!(
                        chunks = total,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "post-copy disk drain complete; publisher unfenced (ADR 0045 C2)",
                    );
                    Ok(total as u64)
                }
                Some(detail) => {
                    ov.lost.store(true, Ordering::SeqCst);
                    let remaining = ov.sealed.lock().await.len();
                    tracing::error!(
                        remaining,
                        %detail,
                        "post-copy disk drain lost its peer with sealed chunks outstanding",
                    );
                    Err(detail)
                }
            };
            let _ = ov.done_tx.send(Some(result));
        });
    }

    pub async fn flush_upload(
        &self,
        pending: PendingDiskFlush,
    ) -> Result<DiskFlushOutcome, DiskBackendError> {
        let chunk_size = self.chunk_size;
        // Issue #199: keep the flush-pipeline guard (acquired in
        // `flush_local`) alive for the WHOLE of `flush_upload` — the
        // GCS puts, the manifest rebuild/publish, AND the `base` rebase.
        // It releases on drop at function exit (or on the early returns
        // below, which also drop it). This is what makes the
        // drain→upload→publish→rebase one atomic critical section.
        let _flush_guard = pending.flush_guard;
        let new_chunks = pending.new_chunks;
        if new_chunks.is_empty() {
            let out = DiskFlushOutcome {
                manifest_ref: self.state.lock().await.manifest_ref,
                chunks_flushed: 0,
                bytes_uploaded: 0,
            };
            self.stamp_flush_completion();
            return Ok(out);
        }
        // Upload the stashed chunks to GCS — OFF the frozen-guest path.
        // ADR 0039 item #19: the per-chunk put dominates the flush
        // (~16 ms each), so awaiting one at a time made a large dirty
        // set (~1,936 chunks → ~32 s) the slow phase of an eviction.
        // Puts are content-addressed/idempotent and keyed by chunk_idx,
        // so they're order-independent — fan them out with bounded
        // `buffer_unordered`.
        //
        // Each task uploads the bytes it carried out of `flush_local`,
        // NOT a re-read of the shared `pending_uploads` map. The earlier
        // shared-map lookup silently skipped (`Ok(())`) any chunk a
        // *concurrent* flush had already cleared from `pending_uploads`,
        // while the manifest rebuild below still referenced its hash —
        // publishing a manifest pointing at a chunk nobody uploaded
        // (prod incident: session 79b689e4, ~half its disk writes
        // dropped → guest EIO). Uploading from `new_chunks`'s own bytes
        // makes the put set exactly `new_chunks`, with no skips.
        //
        // ATOMICITY (durability invariant): `try_collect` aborts on the
        // FIRST put error and the manifest rebuild runs ONLY after every
        // put succeeds — a published manifest never references a
        // not-yet-durable chunk. On failure we re-queue the drained bytes
        // into `dirty` (a newer guest write wins) so the next checkpoint
        // retries; nothing is lost and the previous snapshot stands. A
        // failed flush is a no-op, never a half-written snapshot.
        {
            use futures::stream::{self, StreamExt, TryStreamExt};
            let op = self.operation_scope.current();
            let cache = &self.cache;
            let upload = stream::iter(new_chunks.iter().cloned())
                .map(|(chunk_idx, hash, bytes)| {
                    let store = &self.store;
                    let op = op.clone();
                    async move {
                        let size = bytes.len() as u64;
                        // ADR 0019: span each dirty-chunk upload under an
                        // active operation so this (now post-resume)
                        // flush still shows in the op's trace.
                        let put = store.put_chunk(&bytes);
                        match op {
                            Some(op) => {
                                let span = op.span.in_scope(|| {
                                    tracing::info_span!(
                                        "chunk.flush",
                                        op = op.kind,
                                        chunk = chunk_idx,
                                        bytes = size,
                                    )
                                });
                                tracing::Instrument::instrument(put, span).await?;
                            }
                            None => {
                                put.await?;
                            }
                        }
                        // ADR 0039 (sticky-everywhere): write-through to the
                        // local cache so the flushing host keeps its OWN
                        // just-uploaded chunks and never re-fetches its writes
                        // from GCS (`put_chunk` is upload-only). Best-effort:
                        // the chunk is durable in GCS, so a local-cache write
                        // failure (ENOSPC/perms) is logged, not fatal — reads
                        // fall back to GCS.
                        if let Err(e) = cache.put(hash, &bytes).await {
                            tracing::warn!(
                                chunk = chunk_idx,
                                %hash,
                                error = %e,
                                "flush write-through to local cache failed (chunk durable in GCS; reads fall back)",
                            );
                        }
                        Ok::<_, DiskBackendError>(())
                    }
                })
                .buffer_unordered(DISK_FLUSH_UPLOAD_CONCURRENCY)
                .try_collect::<()>()
                .await;
            if let Err(e) = upload {
                // Atomic no-op. Re-queue the drained bytes into `dirty` so
                // the next flush retries them — but only for an idx the
                // guest hasn't rewritten since the drain (a newer dirty
                // write supersedes our now-stale bytes). The bytes also
                // remain in `pending_uploads`, so reads keep resolving
                // locally (dirty → pending) rather than the un-rebased
                // base. The manifest is NOT advanced.
                let mut dirty = self.dirty.lock().await;
                for (idx, _hash, bytes) in &new_chunks {
                    dirty.entry(*idx).or_insert_with(|| bytes.to_vec());
                }
                tracing::warn!(
                    chunks = new_chunks.len(),
                    error = %e,
                    "nbd disk flush_upload failed; manifest NOT advanced, dirty re-queued (no partial snapshot)",
                );
                return Err(e);
            }
        }

        // Now atomically: rebuild the manifest from the (locked)
        // current base + the just-uploaded hashes, publish to the
        // store, and rebase `state` so future reads serve from the
        // new hashes. Holding the state lock across the put_manifest
        // call is OK — it's a single PUT, bounded by network
        // latency, and reads against this backend during flush are
        // already in-flight or waiting on the dirty lock anyway.
        let mut state = self.state.lock().await;
        // Issue #199 (fence re-check): `migration_fence` is checked once
        // at the top of the synchronous `flush()`, but a flush already
        // PAST that check when `set_migration_fence(true)` runs would
        // otherwise complete its upload + publish AFTER the migration's
        // coherence cut — publishing a newer `live_disk_manifest` that
        // races the destination's rebind (the split-brain the fence
        // exists to prevent). Re-check here, under the `state` lock and
        // immediately before the publish, so a fence raised any time
        // during our (multi-second) upload aborts the publish + rebase.
        // The drained bytes are re-queued into `dirty` (a newer guest
        // write wins — same rule as the upload-failure re-queue) and
        // stay in `pending_uploads`, so reads keep resolving locally.
        if self
            .migration_fence
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            let manifest_ref = state.manifest_ref;
            drop(state);
            let mut dirty = self.dirty.lock().await;
            for (idx, _hash, bytes) in &new_chunks {
                dirty.entry(*idx).or_insert_with(|| bytes.to_vec());
            }
            drop(dirty);
            tracing::warn!(
                chunks = new_chunks.len(),
                "nbd disk flush_upload aborted: migration fence raised mid-upload; \
                 manifest NOT advanced, dirty re-queued (issue #199)",
            );
            return Ok(DiskFlushOutcome {
                manifest_ref,
                chunks_flushed: 0,
                bytes_uploaded: 0,
            });
        }
        let mut chunks: Vec<ChunkRef> = state
            .base
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                h.map(|hash| ChunkRef {
                    offset: (i as u64) * chunk_size,
                    hash,
                })
            })
            .collect();
        let mut bytes_uploaded = 0u64;
        for (idx, hash, bytes) in &new_chunks {
            // Replace the existing entry if one was there; insert
            // otherwise. Linear scan because the chunks list is
            // small (<< 1024 entries for typical disks).
            let offset = (*idx as u64) * chunk_size;
            if let Some(existing) = chunks.iter_mut().find(|c| c.offset == offset) {
                existing.hash = *hash;
            } else {
                chunks.push(ChunkRef {
                    offset,
                    hash: *hash,
                });
            }
            bytes_uploaded += bytes.len() as u64;
        }
        chunks.sort_by_key(|c| c.offset);
        let new_manifest = Manifest {
            schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(chunk_size),
            total_bytes: self.total_bytes,
            chunks,
            parent: Some(state.manifest_ref),
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };
        // ADR 0014 NBD-version race: multiple sandboxes restored from
        // the same template share the same `manifest_ref`. When sandbox
        // A flushes v2, sandbox B's local `state.manifest_ref` is
        // still v1 and its first flush attempts v2 → conflict. Each
        // failed flush would leak a 4-GiB FC snapshot dir (see
        // PooledBackend::snapshot's cleanup), filling host disk inside
        // an hour. Retry on conflict: re-target the latest store
        // version + bump, up to a small cap (a genuine sustained race
        // would loop forever otherwise — bound it).
        //
        // ADR 0014 issue #7: the original retry guard was
        // `if attempted - latest > 32` over `u64` operands. The typical
        // race shape is `attempted < latest` (the store is ahead of
        // us), so the subtraction wraps to ~u64::MAX in release mode
        // (and panics in debug) and the very first retry bails out —
        // exactly the failure mode that surfaced in post-M1.16 prod
        // logs ("nbd disk flush: chunk store: manifest <id> version
        // conflict: attempted v2, latest is v3"). Fixed: explicit
        // attempt counter, no signed-subtraction sin.
        const MAX_FLUSH_RETRIES: u32 = 32;
        let mut attempts: u32 = 0;
        // ADR 0049 follow-up: the FIRST flush of a fresh-base session forks
        // the shared base manifest to a PRIVATE per-session id (a brand-new
        // UUID at v1, parent = base above), so concurrent same-base sessions
        // never share a version chain and `(manifest_id, version)` is
        // globally unambiguous. A fresh id can't version-conflict, so the
        // retry loop below only ever fires for the resume/own-id path. The
        // base chunks the fork's manifest lists stay deduped + pinned (the
        // enabled-image GC source); only the manifest identity forks.
        // ADR 0077 phase 2: always tick our OWN private id. The fork
        // happened at attach, so there is no fresh-base branch here —
        // a fresh id can't conflict, and the retry loop below only ever
        // fires for the own-chain store-ahead race (issue #14).
        // ADR 0077 phase 2: the first publish of a forked fresh-create
        // adopts the private identity at v1 (never versioning the shared
        // base chain); afterwards — and for every non-forked backend —
        // it is a plain next_version() of the resolvable ref.
        let mut attempt_ref = match state.fork_identity {
            Some(id) => ManifestRef {
                manifest_id: id,
                version: 1,
            },
            None => state.manifest_ref.next_version(),
        };
        let new_ref = loop {
            attempts += 1;
            match self.store.put_manifest(attempt_ref, &new_manifest).await {
                Ok(()) => break attempt_ref,
                Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                    latest,
                    attempted,
                    manifest_id,
                }) => {
                    if attempts >= MAX_FLUSH_RETRIES {
                        // Sustained race; bail with the latest
                        // conflict surface so the caller sees the
                        // actual store-vs-attempt mismatch.
                        return Err(engram_chunk_store::ChunkStoreError::VersionConflict {
                            latest,
                            attempted,
                            manifest_id,
                        }
                        .into());
                    }
                    let next = ManifestRef {
                        manifest_id,
                        version: latest + 1,
                    };
                    tracing::debug!(
                        manifest_id = %manifest_id,
                        attempted = attempted,
                        latest_in_store = latest,
                        retry_with = next.version,
                        attempts,
                        "flush: NBD manifest version conflict; retrying with store's latest+1",
                    );
                    attempt_ref = next;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        };

        // Rebase: future reads of any chunk_idx we just rewrote
        // resolve to the NEW hash via the cache + store. Without
        // this rebase a post-flush read would re-fetch the OLD
        // hash from the base and serve stale bytes.
        for (idx, hash, _) in &new_chunks {
            if *idx < state.base.chunks.len() {
                state.base.chunks[*idx] = Some(*hash);
            }
        }
        state.manifest_ref = new_ref;
        state.fork_identity = None;
        drop(state);

        // ADR 0038 B3: chunks are now durable in GCS AND `base` is
        // rebased to their hashes, so reads resolve through the cache/
        // store — drop them from the pending tier.
        {
            let mut pending = self.pending_uploads.lock().await;
            for (idx, hash, _) in &new_chunks {
                // Only drop OUR entry — a concurrent flush may have
                // re-inserted a newer (not-yet-uploaded) write for this
                // idx; clobbering it would lose read-survival for those
                // bytes until the next flush.
                if matches!(pending.get(idx), Some((h, _)) if h == hash) {
                    pending.remove(idx);
                }
            }
        }

        self.stamp_flush_completion();

        Ok(DiskFlushOutcome {
            manifest_ref: new_ref,
            chunks_flushed: new_chunks.len(),
            bytes_uploaded,
        })
    }

    /// Stamp `last_flush_unix_ms` at flush completion. Helper so the
    /// no-dirty-shortcut and the full flush path both record the
    /// same signal: "we completed a flush at time T". ADR 0016 Phase
    /// A — diagnostic for `cow_state`.
    fn stamp_flush_completion(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.last_flush_unix_ms.store(now_ms, Ordering::Release);
    }

    /// Fetch a chunk's bytes — from the dirty buffer if present,
    /// otherwise via the L1 cache + chunk store. `chunk_len` is the
    /// chunk's full byte size (might be < `chunk_size` for the
    /// final chunk of a non-aligned disk).
    async fn read_chunk(
        &self,
        chunk_idx: usize,
        chunk_len: u64,
    ) -> Result<Bytes, DiskBackendError> {
        // Dirty buffer wins. Hold the lock only long enough to
        // clone the bytes; chunk reads are O(N) memcpy.
        //
        // Issue #204 lock order: this path consults `dirty` then `pending`
        // SEQUENTIALLY — the `dirty` lock is released (end of this scope)
        // BEFORE the `pending` lock is taken below. It must NEVER nest the
        // two (hold `dirty` across the `pending` acquisition), because
        // `flush_local` nests pending⟶dirty; a `dirty`-then-`pending` nest
        // here would invert that order and risk deadlock.
        {
            let dirty = self.dirty.lock().await;
            if let Some(buf) = dirty.get(&chunk_idx) {
                return Ok(Bytes::copy_from_slice(buf));
            }
        }
        // ADR 0038 B3: pending tier — a chunk drained by `flush_local`
        // (under the FC pause) but not yet uploaded by `flush_upload`
        // (post-resume). `base` isn't rebased until the upload lands, so
        // without this a post-drain read would resolve the OLD `base`
        // hash and serve stale bytes. (A post-resume re-write goes to
        // `dirty`, checked above, so newest-wins ordering holds.)
        {
            let pending = self.pending_uploads.lock().await;
            if let Some((_hash, bytes)) = pending.get(&chunk_idx) {
                return Ok(bytes.clone());
            }
        }
        // ADR 0045 C2 disk post-copy: a sealed chunk's truth lives on
        // the frozen source — `base` holds the PRE-divergence hash.
        // Demand-fetch, install into `dirty`, serve.
        if let Some(ov) = self.postcopy_overlay() {
            if let Some(bytes) = self.postcopy_materialize(&ov, chunk_idx).await? {
                return Ok(bytes);
            }
        }
        // Snapshot the hash under the state lock, then release
        // before the (potentially slow) cache fetch — a concurrent
        // flush mid-fetch will rebase the state, but the hash we
        // captured is still valid (content-addressed; the chunk
        // store retains the old hash until GC).
        let hash = self
            .state
            .lock()
            .await
            .base
            .chunks
            .get(chunk_idx)
            .copied()
            .flatten();
        match hash {
            Some(hash) => {
                // ADR 0021: in-memory LRU above the on-disk chunk cache. With
                // the per-read sha256 now removed (verify-on-populate lives in
                // the chunk cache), a warm on-disk hit is already cheap — this
                // layer guarantees a RAM-speed re-read even if page-cache
                // pressure evicts the file and would otherwise force a cold
                // pd-balanced re-read. Keyed by content hash, so it is immune
                // to a mid-fetch flush rebase, same as the hash snapshot above.
                {
                    let mut mem = self.mem_cache.lock().unwrap();
                    if let Some(bytes) = mem.get(&hash) {
                        // Emit a zero-cost marker span so the trace shows the
                        // mem tier serving hot re-reads (the win is countable).
                        if let Some(op) = self.operation_scope.current() {
                            op.span.in_scope(|| {
                                let _e = tracing::info_span!(
                                    "chunk.fetch",
                                    op = op.kind,
                                    chunk = chunk_idx,
                                    bytes = chunk_len,
                                    tier = "mem",
                                )
                                .entered();
                            });
                        }
                        return Ok(bytes);
                    }
                }
                // Best-effort tier label for the span: present on local disk
                // (warm) vs cold fetch from blob storage. Sampled just before
                // the fetch — a concurrent populate could flip it, which is
                // fine for a diagnostic attribute (ADR 0021: cold/hot in OTel).
                let tier = if self.cache.contains_on_disk(hash) {
                    "nvme"
                } else {
                    "blobstorage"
                };
                // NBD durability: bounded-retry + per-attempt timeout so a
                // stuck or missing origin fetch fails fast (→ NBD EIO via
                // serve_loop) instead of hanging the guest's virtio-blk I/O.
                // The `chunk.fetch` span is emitted inside the helper,
                // attached to the active op scope + serving tier.
                let bytes = self
                    .fetch_chunk_bounded(hash, chunk_idx, chunk_len, tier)
                    .await?;
                self.mem_cache.lock().unwrap().put(hash, bytes.clone());
                Ok(bytes)
            }
            // Zero-filled hole — the manifest had no entry here.
            None => Ok(Bytes::from(vec![0u8; chunk_len as usize])),
        }
    }

    /// Fetch a chunk via the cache (single-flight + populate), bounding
    /// each attempt with [`CHUNK_FETCH_ATTEMPT_TIMEOUT`] and retrying up to
    /// [`CHUNK_FETCH_MAX_ATTEMPTS`] so a stuck or transiently-missing origin
    /// GET fails fast with an error (→ NBD EIO) rather than hanging the
    /// guest's virtio-blk I/O indefinitely (which also blocks FC from
    /// pausing the VM).
    ///
    /// The timeout lives INSIDE the single-flight closure on purpose:
    /// [`ChunkCache::get`] drains its waiters whenever the fetch returns
    /// (Ok or Err), so a timed-out fetch propagates the error to every
    /// waiter and never poisons the inflight map — whereas timing out
    /// *around* `cache.get` would cancel the in-flight fetcher and strand
    /// the other waiters on the same hash.
    async fn fetch_chunk_bounded(
        &self,
        hash: ChunkHash,
        chunk_idx: usize,
        chunk_len: u64,
        tier: &'static str,
    ) -> Result<Bytes, DiskBackendError> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let store = self.store.clone();
            let fut = self.cache.get(hash, move || async move {
                match tokio::time::timeout(CHUNK_FETCH_ATTEMPT_TIMEOUT, store.get_chunk(hash)).await
                {
                    Ok(r) => r,
                    Err(_) => Err(engram_chunk_store::error::ChunkStoreError::FetchTimeout(
                        format!("{hash} after {CHUNK_FETCH_ATTEMPT_TIMEOUT:?}"),
                    )),
                }
            });
            // ADR 0019: attach the page-in span to the active lifecycle
            // operation (cold boot etc.), tagged with the serving tier.
            let outcome = match self.operation_scope.current() {
                Some(op) => {
                    let span = op.span.in_scope(|| {
                        tracing::info_span!(
                            "chunk.fetch",
                            op = op.kind,
                            chunk = chunk_idx,
                            bytes = chunk_len,
                            tier = tier,
                        )
                    });
                    tracing::Instrument::instrument(fut, span).await
                }
                None => fut.await,
            };
            match outcome {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    // 404s are near-permanent (short budget — just ride out
                    // GCS read-after-write EC); transient errors get the
                    // fuller budget since the blob is expected to exist.
                    let not_found = chunk_fetch_is_not_found(&e);
                    let max = if not_found {
                        CHUNK_FETCH_NOTFOUND_MAX_ATTEMPTS
                    } else {
                        CHUNK_FETCH_MAX_ATTEMPTS
                    };
                    if attempt >= max {
                        // Exhausted the bounded budget — fail the read so the
                        // NBD layer returns EIO. Loud + countable (with the
                        // outcome class) so a missing/stuck chunk surfaces as
                        // an alert, not a silent wedge.
                        let outcome = if not_found { "notfound" } else { "transient" };
                        metrics::counter!(
                            "engram_nbd_chunk_fetch_failed_total",
                            "tier" => tier,
                            "outcome" => outcome,
                        )
                        .increment(1);
                        tracing::error!(
                            chunk = chunk_idx,
                            hash = %hash,
                            attempts = attempt,
                            outcome,
                            error = %e,
                            "chunk fetch exhausted bounded retries; failing the NBD request \
                             (guest gets EIO — rootfs degrades read-only but the VM stays \
                             pausable/evictable instead of wedging)",
                        );
                        return Err(e.into());
                    }
                    tracing::warn!(
                        chunk = chunk_idx,
                        hash = %hash,
                        attempt,
                        not_found,
                        error = %e,
                        "chunk fetch failed; retrying (bounded)",
                    );
                    tokio::time::sleep(CHUNK_FETCH_RETRY_BACKOFF).await;
                }
            }
        }
    }

    /// The operation scope for this sandbox's data plane (ADR 0019). The
    /// host calls `begin`/`end` on it around lifecycle operations so
    /// `read_chunk` page-ins attach to the operation's trace.
    pub fn operation_scope(&self) -> &crate::trace_scope::OperationScope {
        &self.operation_scope
    }

    // ensure_dirty was the pre-12m two-phase chunk materialization
    // helper. Folded into `write_chunk` above, which combines the
    // base-bytes fetch (outside the dirty lock) with a single-lock
    // insert-and-patch (inside the lock). Kept the threshold-cross
    // detection in the patch path so the flush scheduler still
    // wakes on monotonic crossings.
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::cache::ChunkCacheConfig;
    use engram_chunk_store::manifest::{ChunkSize, MANIFEST_SCHEMA_VERSION};
    use engram_core::traits::{BlobStorage, ByteStream};
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;

    fn synth_manifest(
        total_bytes: u64,
        chunk_size: u64,
        entries: Vec<(u64, ChunkHash)>,
    ) -> Manifest {
        Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: ChunkSize::bytes(chunk_size),
            total_bytes,
            chunks: entries
                .into_iter()
                .map(|(offset, hash)| ChunkRef { offset, hash })
                .collect(),
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        }
    }

    async fn build_backend(
        manifest: &Manifest,
    ) -> (ChunkedDiskBackend, Arc<ChunkStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, manifest).await.unwrap();
        let backend =
            ChunkedDiskBackend::new(manifest_ref, manifest, cache, store.clone(), u64::MAX)
                .unwrap();
        (backend, store, dir)
    }

    /// Synth helper: put a chunk of `byte` repeated `size` times
    /// into the store and return its hash. Lets tests build a base
    /// manifest with known content.
    async fn put_chunk(store: &ChunkStore, byte: u8, size: usize) -> ChunkHash {
        let bytes = vec![byte; size];
        store.put_chunk(&bytes).await.unwrap()
    }

    #[tokio::test]
    async fn read_within_a_single_base_chunk_returns_chunk_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 8192u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let h1 = put_chunk(&store, 0xbb, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0), (chunk_size, h1)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        let bytes = backend.read(0, 4096).await.unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(bytes.iter().all(|b| *b == 0xaa));

        let bytes = backend.read(4096, 4096).await.unwrap();
        assert!(bytes.iter().all(|b| *b == 0xbb));
    }

    /// ADR 0071 (#2): a read that straddles chunk boundaries fans the
    /// per-chunk fetches out concurrently (`buffered`) and must reassemble
    /// them IN ORDER — chunk 0's bytes before chunk 1's before chunk 2's, no
    /// transposition from out-of-order fetch completion.
    #[tokio::test]
    async fn read_across_chunk_boundaries_reassembles_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 3 * chunk_size;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let h1 = put_chunk(&store, 0xbb, chunk_size as usize).await;
        let h2 = put_chunk(&store, 0xcc, chunk_size as usize).await;
        let manifest = synth_manifest(
            total,
            chunk_size,
            vec![(0, h0), (chunk_size, h1), (2 * chunk_size, h2)],
        );
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        // From the middle of chunk 0 through the middle of chunk 2: 2048 of
        // 0xaa, then 4096 of 0xbb, then 2048 of 0xcc.
        let bytes = backend.read(2048, 2 * chunk_size).await.unwrap();
        assert_eq!(bytes.len(), (2 * chunk_size) as usize);
        assert!(bytes[..2048].iter().all(|b| *b == 0xaa), "chunk 0 tail");
        assert!(
            bytes[2048..2048 + 4096].iter().all(|b| *b == 0xbb),
            "chunk 1 whole",
        );
        assert!(
            bytes[2048 + 4096..].iter().all(|b| *b == 0xcc),
            "chunk 2 head"
        );
    }

    /// ADR 0049 follow-up regression: the same-base concurrent-restore
    /// corruption. Two backends built on the SAME base manifest (the
    /// fresh-create path) must, when fork-armed, each fork to a DISTINCT
    /// private `manifest_id` on first flush and read back their OWN bytes —
    /// never collide on one shared version chain. Before the fix both wrote
    /// under the base id, raced its versions, and `(id, version)` was
    /// ambiguous across sessions → `reread ''`.
    #[tokio::test]
    async fn fresh_same_base_backends_fork_to_distinct_private_ids() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, h0)]);
        let base_ref = ManifestRef::new();
        store.put_manifest(base_ref, &manifest).await.unwrap();
        let mk_cache = || {
            let mut cfg = ChunkCacheConfig::new(dir.path().join(format!("c{}", rand_suffix())));
            cfg.budget_bytes = 64 * 1024 * 1024;
            ChunkCache::new(cfg)
        };

        // Two fresh-create backends on the SAME base, forked AT ATTACH
        // (ADR 0077 phase 2) — before any write touches them.
        let a = ChunkedDiskBackend::new(base_ref, &manifest, mk_cache(), store.clone(), u64::MAX)
            .unwrap();
        let b = ChunkedDiskBackend::new(base_ref, &manifest, mk_cache(), store.clone(), u64::MAX)
            .unwrap();
        a.fork_manifest_identity().await;
        b.fork_manifest_identity().await;

        a.write(0, &[0x11u8; 4096]).await.unwrap();
        b.write(0, &[0x22u8; 4096]).await.unwrap();
        let oa = a.flush().await.unwrap();
        let ob = b.flush().await.unwrap();

        // Each forked off the base to its OWN private id, both at v1.
        assert_ne!(
            oa.manifest_ref.manifest_id, base_ref.manifest_id,
            "A never forked"
        );
        assert_ne!(
            ob.manifest_ref.manifest_id, base_ref.manifest_id,
            "B never forked"
        );
        assert_ne!(
            oa.manifest_ref.manifest_id, ob.manifest_ref.manifest_id,
            "same-base backends collided on one manifest id"
        );
        assert_eq!(oa.manifest_ref.version, 1);
        assert_eq!(ob.manifest_ref.version, 1);

        // No cross-session clobber: each reads its OWN bytes.
        assert!(a.read(0, 4096).await.unwrap().iter().all(|x| *x == 0x11));
        assert!(b.read(0, 4096).await.unwrap().iter().all(|x| *x == 0x22));

        // A second flush TICKS the private id (no re-fork).
        a.write(0, &[0x33u8; 4096]).await.unwrap();
        let oa2 = a.flush().await.unwrap();
        assert_eq!(
            oa2.manifest_ref.manifest_id, oa.manifest_ref.manifest_id,
            "re-forked"
        );
        assert_eq!(oa2.manifest_ref.version, 2);
    }

    /// #584 review regression: `manifest_ref()` must stay STORE-RESOLVABLE
    /// between the fresh-create fork and the first publish. The eviction
    /// capture, finalize, and cow-state paths all `get_manifest` whatever
    /// this returns — an unpublished placeholder here made a session
    /// evicted before its first flush unevictable (finalize NotFound on
    /// every redrive), and the zero-dirty capture variant recorded the
    /// dangling ref into the snapshot row, permanently unresumable.
    #[tokio::test]
    async fn forked_backend_stays_resolvable_until_first_publish() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, h0)]);
        let base_ref = ManifestRef::new();
        store.put_manifest(base_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend = ChunkedDiskBackend::new(
            base_ref,
            &manifest,
            ChunkCache::new(cfg),
            store.clone(),
            u64::MAX,
        )
        .unwrap();
        backend.fork_manifest_identity().await;

        // Pre-publish: the advertised ref is the shared base — resolvable.
        let pre = backend.manifest_ref().await;
        assert_eq!(pre, base_ref, "pre-publish ref must be the shared base");
        store
            .get_manifest(pre)
            .await
            .expect("pre-publish manifest_ref must resolve in the store");

        // Zero-dirty flush (the evict-with-no-writes shape): still the
        // resolvable base, no phantom publish.
        let out = backend.flush().await.unwrap();
        assert_eq!(out.chunks_flushed, 0);
        assert_eq!(
            out.manifest_ref, base_ref,
            "zero-dirty flush must not mint a ref"
        );
        store
            .get_manifest(out.manifest_ref)
            .await
            .expect("zero-dirty flush outcome must resolve in the store");

        // First real write adopts the private identity at v1 — and THAT
        // resolves too.
        backend.write(0, &[0x55u8; 4096]).await.unwrap();
        let out = backend.flush().await.unwrap();
        assert_ne!(out.manifest_ref.manifest_id, base_ref.manifest_id);
        assert_eq!(out.manifest_ref.version, 1);
        store
            .get_manifest(out.manifest_ref)
            .await
            .expect("first publish must resolve in the store");
        assert_eq!(backend.manifest_ref().await, out.manifest_ref);
    }

    /// The resume/recovery path (fork NOT armed) keeps ticking the id it
    /// attached — it already owns a private manifest from its prior snapshot.
    #[tokio::test]
    async fn unforked_backend_ticks_its_attached_id() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, h0)]);
        let own_ref = ManifestRef::new();
        store.put_manifest(own_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let backend =
            ChunkedDiskBackend::new(own_ref, &manifest, ChunkCache::new(cfg), store, u64::MAX)
                .unwrap();
        // No fork_manifest_identity — resume semantics (own id).
        backend.write(0, &[0x44u8; 4096]).await.unwrap();
        let out = backend.flush().await.unwrap();
        assert_eq!(
            out.manifest_ref.manifest_id, own_ref.manifest_id,
            "unforked backend forked"
        );
        assert_eq!(out.manifest_ref.version, own_ref.version + 1);
    }

    /// Tiny per-call suffix so the two backends use distinct cache dirs
    /// without `Math.random`-style nondeterminism in the assertions.
    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    /// The flush-window write race (the teleport-canary zeros): a write
    /// landing AFTER `flush_local` drained its chunk to the pending tier
    /// but BEFORE the upload rebases `base` must RMW from PENDING, not
    /// the stale base — otherwise everything the drain just captured is
    /// silently dropped from the chunk and reads (and the next publish)
    /// serve zeros/stale bytes where the first write wave should be.
    #[tokio::test]
    async fn write_during_flush_window_rmws_from_pending_not_stale_base() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        // Wave 1: first half of the chunk.
        backend.write(0, &[0x11u8; 2048]).await.unwrap();
        // The scheduler's drain races in: dirty -> pending (upload not
        // yet landed, base NOT rebased).
        let _pending = backend.flush_local().await.unwrap();
        // Wave 2: second half, arriving inside the flush window.
        backend.write(2048, &[0x22u8; 2048]).await.unwrap();

        let bytes = backend.read(0, 4096).await.unwrap();
        assert!(
            bytes[..2048].iter().all(|b| *b == 0x11),
            "wave-1 bytes must survive a wave-2 RMW inside the flush window \
             (stale-base RMW would resurrect the pre-flush base here)",
        );
        assert!(bytes[2048..].iter().all(|b| *b == 0x22), "wave-2 bytes");
    }

    /// Issue #204: the dirty→pending handoff inside `flush_local` must be
    /// ATOMIC w.r.t. concurrent readers/writers. The pre-fix code drained
    /// `dirty`, dropped the dirty lock, then took `pending` separately — so
    /// for an instant a drained chunk lived in NO tier. A `read` in that gap
    /// fell through to the stale `base` (transient stale read); a `write` in
    /// that gap RMW'd from stale base and re-inserted into `dirty`,
    /// permanently shadowing the drained bytes (silent lost write).
    ///
    /// This test installs a seam that parks `flush_local` at exactly that
    /// handoff point (with the fix: both locks held) and races a read AND a
    /// write of the drained chunk. It asserts:
    ///   (a) the racing read returns the DRAINED content, never stale base;
    ///   (b) the racing write merges onto the drained content (not stale
    ///       base), so a subsequent flush publishes the correct bytes.
    /// On the pre-fix code (a) reads base and (b) loses the drained half,
    /// so this fails; with the fix both readers/writers block on the held
    /// `pending` lock until the handoff completes and observe drained truth.
    #[tokio::test]
    async fn flush_local_handoff_is_atomic_no_tierless_gap() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64;
        // Base chunk is all-0xaa — the "stale base" the gap would serve.
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap(),
        );

        // Guest writes the WHOLE chunk to 0x11 (acked) — this is the
        // content the flush is about to drain.
        backend.write(0, &[0x11u8; 4096]).await.unwrap();

        // Arm the seam, then run flush_local on a task; it will park at the
        // dirty→pending handoff with both locks held.
        let (arrived, proceed) = backend.arm_flush_handoff_seam();
        let flush_backend = backend.clone();
        let flush_task = tokio::spawn(async move { flush_backend.flush_local().await.map(|_| ()) });

        // Wait until flush_local has reached the seam (chunk drained,
        // handoff in progress).
        arrived.notified().await;

        // Race a READ and a WRITE of the drained chunk against the handoff.
        // With the fix these block on the held `pending` lock; with the bug
        // they slip through the tier-less gap and hit stale base.
        let read_backend = backend.clone();
        let read_task = tokio::spawn(async move { read_backend.read(0, 4096).await });
        let write_backend = backend.clone();
        // Overwrite the SECOND half to 0x22; the first half must remain the
        // drained 0x11 (a stale-base RMW would resurrect 0xaa there).
        let write_task =
            tokio::spawn(async move { write_backend.write(2048, &[0x22u8; 2048]).await });

        // Give the racing ops a chance to wedge against the held locks
        // (or, on the buggy code, to race through the gap), then complete
        // the handoff.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        proceed.notify_one();

        flush_task.await.unwrap().unwrap();
        let read_bytes = read_task.await.unwrap().unwrap();
        write_task.await.unwrap().unwrap();

        // (a) The racing read must observe the drained content (0x11),
        // never the pre-flush base (0xaa).
        assert!(
            read_bytes.iter().all(|b| *b == 0x11),
            "racing read in the flush handoff window served stale base \
             instead of the drained content (issue #204 tier-less gap)",
        );

        // (b) The racing write must have RMW'd from the drained content:
        // first half stays 0x11, second half becomes 0x22. A stale-base RMW
        // would leave 0xaa in the first half and lose the acked write.
        let after = backend.read(0, 4096).await.unwrap();
        assert!(
            after[..2048].iter().all(|b| *b == 0x11),
            "drained bytes were shadowed by a stale-base RMW during the \
             flush handoff (silent lost write, issue #204)",
        );
        assert!(
            after[2048..].iter().all(|b| *b == 0x22),
            "racing write's bytes missing after the handoff",
        );

        // (c) A subsequent flush publishes the merged-correct content (the
        // write landed in `dirty` after the drain, so it flushes cleanly).
        backend.flush().await.unwrap();
        let published = backend.read(0, 4096).await.unwrap();
        assert!(published[..2048].iter().all(|b| *b == 0x11));
        assert!(published[2048..].iter().all(|b| *b == 0x22));
    }

    /// NBD durability: a read whose backing chunk blob is missing must
    /// return an error in BOUNDED time (→ NBD EIO via serve_loop), never
    /// hang. Regression for the wedged-session class where a stuck/missing
    /// chunk fetch left the guest's virtio-blk I/O in uninterruptible sleep
    /// (jbd2/writeback D-state) and blocked FC from pausing the VM. The
    /// outer timeout is the assertion: the bounded-retry budget must elapse
    /// to an `Err`, not block forever.
    #[tokio::test]
    async fn read_of_missing_chunk_fails_fast_within_bound_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        // Reference a chunk hash we NEVER store: the backing blob is absent,
        // so every fetch attempt 404s — the prod "reaped chunk" shape.
        let missing = ChunkHash::of(b"a-chunk-that-was-reaped-and-never-stored");
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, missing)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        // The read must resolve to an Err well within this generous bound
        // (the bounded-retry budget), NOT hang. A timeout here = regression.
        let outer = CHUNK_FETCH_RETRY_BACKOFF * (CHUNK_FETCH_MAX_ATTEMPTS + 4)
            + CHUNK_FETCH_ATTEMPT_TIMEOUT;
        let outcome = tokio::time::timeout(outer, backend.read(0, chunk_size))
            .await
            .expect("read must fail fast on a missing chunk, not hang");
        assert!(
            outcome.is_err(),
            "a read of a missing chunk must surface an error (→ NBD EIO), got Ok",
        );
    }

    /// The 404-vs-transient delineation that drives the retry budget: a
    /// definitive `BlobError::NotFound` (GCS 404) is near-permanent and
    /// gets the short budget; a fetch timeout / origin 5xx is transient and
    /// gets the fuller budget.
    #[test]
    fn chunk_fetch_classifies_notfound_vs_transient() {
        use engram_chunk_store::error::ChunkStoreError;
        use engram_core::error::BlobError;
        assert!(
            chunk_fetch_is_not_found(&ChunkStoreError::Blob(BlobError::NotFound)),
            "a 404 must classify as not-found (short retry budget)",
        );
        assert!(
            !chunk_fetch_is_not_found(&ChunkStoreError::FetchTimeout("h after 3s".into())),
            "a per-attempt timeout is transient (full retry budget)",
        );
        assert!(
            !chunk_fetch_is_not_found(&ChunkStoreError::Origin("origin 5xx".into())),
            "an origin/5xx error is transient (full retry budget)",
        );
    }

    #[tokio::test]
    async fn mem_cache_evicts_lru_protecting_recently_touched() {
        // ADR 0021 P2: byte-budgeted LRU. Budget = 2 chunks. Touching h0
        // after inserting h1 must protect h0, so inserting h2 evicts h1.
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk = 4096usize;
        let h0 = put_chunk(&store, 0xa0, chunk).await;
        let h1 = put_chunk(&store, 0xb1, chunk).await;
        let h2 = put_chunk(&store, 0xc2, chunk).await;
        let bytes = |b: u8| Bytes::from(vec![b; chunk]);

        let mut mem = ChunkMemCache::new((2 * chunk) as u64);
        mem.put(h0, bytes(0xa0));
        mem.put(h1, bytes(0xb1));
        assert!(mem.get(&h0).is_some(), "h0 present after two inserts");
        // get(h0) above bumped h0 to MRU; h1 is now LRU.
        mem.put(h2, bytes(0xc2));
        assert!(mem.get(&h1).is_none(), "LRU h1 evicted by h2");
        assert!(mem.get(&h0).is_some(), "recently-touched h0 survives");
        assert!(mem.get(&h2).is_some(), "freshly-inserted h2 present");
        assert_eq!(mem.bytes, (2 * chunk) as u64, "byte accounting holds");

        // Re-putting an existing key is a no-op (no double-count).
        mem.put(h0, bytes(0xa0));
        assert_eq!(mem.bytes, (2 * chunk) as u64, "duplicate put does not grow");
    }

    #[tokio::test]
    async fn reading_same_chunk_twice_is_consistent() {
        // Exercises the mem-cache put-on-miss / hit-on-second-read wiring in
        // read_chunk: the second read of the same offset must match the first.
        let total = 8192u64;
        let chunk_size = 4096u64;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        let first = backend.read(0, 4096).await.unwrap();
        let second = backend.read(0, 4096).await.unwrap();
        assert_eq!(first, second, "second (mem-cached) read matches first");
        assert!(second.iter().all(|b| *b == 0xaa));
    }

    #[tokio::test]
    async fn read_across_chunk_boundary_stitches_two_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 8192u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let h1 = put_chunk(&store, 0xbb, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0), (chunk_size, h1)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        // 2 KiB straddle: last 2 KiB of chunk 0 + first 2 KiB of chunk 1.
        let bytes = backend.read(2048, 4096).await.unwrap();
        assert_eq!(bytes.len(), 4096);
        assert!(
            bytes[..2048].iter().all(|b| *b == 0xaa),
            "first half should be 0xaa"
        );
        assert!(
            bytes[2048..].iter().all(|b| *b == 0xbb),
            "second half should be 0xbb"
        );
    }

    #[tokio::test]
    async fn read_a_sparse_hole_returns_zeros() {
        // Manifest omits chunk 1 (sparse hole). Read of that
        // region serves zeros without touching the cache or
        // store.
        let total = 8192u64;
        let chunk_size = 4096u64;
        let (backend, store, _dir) =
            build_backend(&synth_manifest(total, chunk_size, vec![])).await;
        let _ = store; // hush unused
        let bytes = backend.read(4096, 4096).await.unwrap();
        assert!(bytes.iter().all(|b| *b == 0), "sparse hole reads as zeros");
    }

    #[tokio::test]
    async fn write_then_read_within_one_chunk_observes_dirty_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        backend.write(0, &[0xcc; 16]).await.unwrap();
        let bytes = backend.read(0, 32).await.unwrap();
        assert!(bytes[..16].iter().all(|b| *b == 0xcc));
        assert!(bytes[16..].iter().all(|b| *b == 0xaa));
    }

    #[tokio::test]
    async fn flush_with_no_writes_is_a_no_op() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _store, _dir) = build_backend(&manifest).await;
        let outcome = backend.flush().await.unwrap();
        assert_eq!(outcome.chunks_flushed, 0);
        assert_eq!(outcome.bytes_uploaded, 0);
        // The manifest ref doesn't tick when nothing was dirty.
        assert_eq!(outcome.manifest_ref, backend.manifest_ref().await);
    }

    #[tokio::test]
    async fn flush_publishes_new_manifest_version_with_dirty_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 8192u64;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store.clone(), u64::MAX)
                .unwrap();

        // Touch chunk 0 (mutate it) and chunk 1 (was sparse).
        backend.write(0, &[0xff; 64]).await.unwrap();
        backend.write(4096, &[0xee; 128]).await.unwrap();
        let outcome = backend.flush().await.unwrap();
        assert_eq!(outcome.chunks_flushed, 2);
        assert!(outcome.bytes_uploaded > 0);
        assert_eq!(outcome.manifest_ref.manifest_id, manifest_ref.manifest_id);
        assert_eq!(outcome.manifest_ref.version, manifest_ref.version + 1);

        // The new manifest, fetched back from the store, contains
        // the new hashes — not the original ones.
        let new_manifest = store.get_manifest(outcome.manifest_ref).await.unwrap();
        assert_eq!(new_manifest.parent, Some(manifest_ref));
        assert_eq!(new_manifest.chunks.len(), 2);
        let chunk0 = new_manifest.chunks.iter().find(|c| c.offset == 0).unwrap();
        assert_ne!(chunk0.hash, h0, "chunk 0 was rewritten; hash must differ");
    }

    #[tokio::test]
    async fn rejects_read_past_total_bytes() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _, _dir) = build_backend(&manifest).await;
        let err = backend.read(4000, 4096).await.unwrap_err();
        assert!(matches!(err, DiskBackendError::OutOfRange { .. }));
    }

    #[tokio::test]
    async fn rejects_write_past_total_bytes() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _, _dir) = build_backend(&manifest).await;
        let err = backend.write(4000, &[0u8; 4096]).await.unwrap_err();
        assert!(matches!(err, DiskBackendError::OutOfRange { .. }));
    }

    #[tokio::test]
    async fn rejects_non_disk_manifest_kind() {
        let mut mem = synth_manifest(4096, 4096, vec![]);
        mem.kind = ManifestKind::Memory;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let err = ChunkedDiskBackend::new(ManifestRef::new(), &mem, cache, store, u64::MAX)
            .err()
            .unwrap();
        assert!(matches!(err, DiskBackendError::WrongKind(_)));
    }

    #[tokio::test]
    async fn zero_length_read_returns_empty() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _, _dir) = build_backend(&manifest).await;
        let bytes = backend.read(0, 0).await.unwrap();
        assert_eq!(bytes.len(), 0);
    }

    /// Regression: after `flush()`, a read of a just-rewritten
    /// chunk MUST serve the freshly-flushed bytes — not the
    /// original base hash's bytes. An earlier revision left the
    /// backend's `base.chunks` pointing at the pre-flush hashes
    /// and zeroed the dirty buffer, so the next read returned
    /// stale data. The fix rebases `state.base.chunks` to the
    /// new hashes atomically with the version tick under the
    /// state lock.
    #[tokio::test]
    async fn post_flush_read_observes_flushed_bytes_not_stale_base() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64;
        let h_base = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h_base)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store.clone(), u64::MAX)
                .unwrap();

        // Write 16 bytes of 0xCC at offset 0, then flush. The
        // dirty buffer is drained during flush; the next read
        // must reflect the WRITTEN content.
        backend.write(0, &[0xcc; 16]).await.unwrap();
        let outcome = backend.flush().await.unwrap();
        assert_eq!(outcome.chunks_flushed, 1);

        // Post-flush read of the rewritten region. Without the
        // rebase fix this would return 0xaa (base content);
        // with the fix it returns 0xcc.
        let bytes = backend.read(0, 32).await.unwrap();
        assert!(
            bytes[..16].iter().all(|b| *b == 0xcc),
            "post-flush read served stale base bytes: {:?}",
            &bytes[..16],
        );
        assert!(
            bytes[16..].iter().all(|b| *b == 0xaa),
            "untouched region should still serve base content"
        );

        // Subsequent flush should be a no-op (dirty buffer
        // empty after the rebase + the absence of new writes).
        let second = backend.flush().await.unwrap();
        assert_eq!(second.chunks_flushed, 0);
        // The manifest_ref hasn't ticked again.
        assert_eq!(second.manifest_ref, outcome.manifest_ref);
    }

    /// ADR 0014 issue #7 regression. Multiple sandboxes restored from
    /// the same template share `manifest_ref` v1; the first flush
    /// publishes v2, the second sees `state.manifest_ref` is still v1
    /// and ALSO tries v2 → `VersionConflict { attempted: 2, latest: 2
    /// or 3 }`. The retry loop MUST advance to `latest + 1` and re-put.
    ///
    /// The original guard was `attempted - latest > 32` over u64 — when
    /// attempted < latest the subtraction wrapped to ~u64::MAX,
    /// triggered `> 32`, and bailed on the very first retry. Fix
    /// substitutes an explicit attempt counter (`MAX_FLUSH_RETRIES`).
    /// This test simulates the race by pre-populating a manifest at
    /// the version the flush will attempt; the retry must land on the
    /// next version up and finish cleanly.
    #[tokio::test]
    async fn flush_retries_past_version_conflict_with_store_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64;
        let h_base = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let base_manifest = synth_manifest(total, chunk_size, vec![(0, h_base)]);
        let manifest_ref = ManifestRef::new();
        // v1: the backend's starting point.
        store
            .put_manifest(manifest_ref, &base_manifest)
            .await
            .unwrap();

        // Simulate sandbox A's prior flush by publishing a foreign v2
        // BEFORE we attempt our own v2 below. Any well-formed disk
        // manifest at v2 will do; we just need the key to exist.
        let foreign_v2 = ManifestRef {
            manifest_id: manifest_ref.manifest_id,
            version: 2,
        };
        let foreign_manifest = synth_manifest(total, chunk_size, vec![(0, h_base)]);
        store
            .put_manifest(foreign_v2, &foreign_manifest)
            .await
            .unwrap();

        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &base_manifest, cache, store.clone(), u64::MAX)
                .unwrap();

        // Dirty some bytes so flush has something to publish.
        backend.write(0, &[0xcc; 16]).await.unwrap();

        // Pre-fix this would have returned VersionConflict on the
        // very first attempt (attempted=2, latest=2, u64 sub wraps,
        // > 32 trips). With the fix, the retry observes latest=2 and
        // re-targets v3, succeeds.
        let outcome = backend
            .flush()
            .await
            .expect("flush must retry past version conflict, not bail on u64 wrap");
        assert_eq!(outcome.manifest_ref.version, 3, "must land on v3");
        assert_eq!(outcome.chunks_flushed, 1);

        // Confirm v3 is what's in the store now.
        let stored_v3 = store.get_manifest(outcome.manifest_ref).await.unwrap();
        assert_eq!(stored_v3.chunks.len(), 1);
    }

    /// Bonus coverage: multi-level race. Two sandboxes back-to-back
    /// pre-publish v2 AND v3. Our flush() retries past both — first
    /// tries v2 (conflict, latest=3), retargets v4, succeeds.
    /// Confirms the retry doesn't just cap at "one bump past latest"
    /// — it does the right thing when latest moves between attempts
    /// (which can happen when a concurrent writer commits during our
    /// retry, just at a finer granularity than the test simulates).
    #[tokio::test]
    async fn flush_retries_past_multiple_foreign_versions() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64;
        let h_base = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let base_manifest = synth_manifest(total, chunk_size, vec![(0, h_base)]);
        let manifest_ref = ManifestRef::new();
        store
            .put_manifest(manifest_ref, &base_manifest)
            .await
            .unwrap();

        // Pre-populate v2 + v3 — every retry that targets one of
        // these conflicts, and latest jumps forward each time.
        for v in 2u64..=3 {
            let r = ManifestRef {
                manifest_id: manifest_ref.manifest_id,
                version: v,
            };
            let m = synth_manifest(total, chunk_size, vec![(0, h_base)]);
            store.put_manifest(r, &m).await.unwrap();
        }

        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &base_manifest, cache, store.clone(), u64::MAX)
                .unwrap();

        backend.write(0, &[0xee; 16]).await.unwrap();
        let outcome = backend
            .flush()
            .await
            .expect("multi-level race must still converge");
        assert_eq!(outcome.manifest_ref.version, 4, "must land on v4");
    }

    // -- ADR 0016 Phase A: COW diagnostic accessors -------------------

    #[tokio::test]
    async fn dirty_chunks_count_zero_on_fresh_backend() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _, _dir) = build_backend(&manifest).await;
        assert_eq!(backend.dirty_chunks_count().await, 0);
        assert_eq!(backend.dirty_bytes().await, 0);
    }

    #[tokio::test]
    async fn dirty_counters_track_writes_and_clear_on_flush() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 4096u64 * 3;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        backend.write(0, &[0xcc; 16]).await.unwrap();
        // Touching one chunk materialises it whole — `dirty_bytes`
        // counts the full chunk buffer, not just the patched range.
        assert_eq!(backend.dirty_chunks_count().await, 1);
        assert_eq!(backend.dirty_bytes().await, chunk_size);

        // Second chunk via a write that lands inside chunk 1.
        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();
        assert_eq!(backend.dirty_chunks_count().await, 2);
        assert_eq!(backend.dirty_bytes().await, chunk_size * 2);

        let _ = backend.flush().await.unwrap();
        assert_eq!(backend.dirty_chunks_count().await, 0);
        assert_eq!(backend.dirty_bytes().await, 0);
    }

    #[tokio::test]
    async fn last_flush_unix_ms_is_zero_until_first_flush() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _, _dir) = build_backend(&manifest).await;
        assert_eq!(
            backend.last_flush_unix_ms(),
            0,
            "sentinel for 'never flushed'"
        );
    }

    #[tokio::test]
    async fn last_flush_unix_ms_bumps_on_every_flush_including_no_dirty() {
        let manifest = synth_manifest(4096, 4096, vec![]);
        let (backend, _, _dir) = build_backend(&manifest).await;
        // No-dirty flush still records the completion timestamp —
        // the diagnostic signal is "we last verified durability at
        // T", not "we last had something to flush". Phase B's
        // flush_scheduler decides whether to publish a callback
        // based on chunks_flushed; this stamp is independent.
        let _ = backend.flush().await.unwrap();
        let first = backend.last_flush_unix_ms();
        assert!(first > 0, "first flush must stamp a non-zero unix-ms");

        // Sleep enough that the second flush lands on a distinct
        // millisecond. 2 ms is the smallest portable sleep that
        // reliably advances UNIX_EPOCH millis under tokio's mock
        // clock-less runtime.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let _ = backend.flush().await.unwrap();
        let second = backend.last_flush_unix_ms();
        assert!(
            second >= first,
            "second flush stamp ({second}) must not regress past first ({first})",
        );
    }

    // -- ADR 0016 Phase B: threshold-notify wiring -------------------

    /// Build a backend with an explicit `threshold_bytes`. Mirrors
    /// `build_backend` but exposes the threshold knob so the
    /// threshold-notify tests below can drive crossings deterministically.
    async fn build_backend_with_threshold(
        manifest: &Manifest,
        threshold_bytes: u64,
    ) -> (ChunkedDiskBackend, Arc<ChunkStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, manifest).await.unwrap();
        let backend = ChunkedDiskBackend::new(
            manifest_ref,
            manifest,
            cache,
            store.clone(),
            threshold_bytes,
        )
        .unwrap();
        (backend, store, dir)
    }

    /// A first write that fits below the threshold does not fire the
    /// notify; the second write that pushes dirty bytes past the
    /// threshold does. The `tokio::time::timeout` on `notified()`
    /// is the structural assertion — `notify_one` either parked a
    /// permit (the await returns immediately) or it didn't (the
    /// await times out).
    #[tokio::test]
    async fn writer_crosses_threshold_pokes_notify() {
        let chunk_size = 4096u64;
        let total = chunk_size * 4;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        // Threshold at 2 chunks worth: one chunk write stays under,
        // a second crosses.
        let (backend, _store, _dir) = build_backend_with_threshold(&manifest, chunk_size * 2).await;
        let notify = backend.threshold_notify();

        // First write: dirty buffer holds one full chunk (the write
        // materialises the whole 4 KiB on first touch). 4 KiB < 8
        // KiB → no crossing → no notify permit.
        backend.write(0, &[0xcc; 8]).await.unwrap();
        let below =
            tokio::time::timeout(std::time::Duration::from_millis(20), notify.notified()).await;
        assert!(
            below.is_err(),
            "notify must NOT fire before threshold is crossed",
        );

        // Second write touches chunk 1: dirty grows to 8 KiB, which
        // is >= threshold. Crossing edge observed under the dirty
        // lock; `notify_one` parks a permit.
        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();
        let crossed =
            tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified()).await;
        assert!(
            crossed.is_ok(),
            "notify must fire when dirty bytes cross threshold",
        );
    }

    /// Once the buffer is past threshold, subsequent inserts on
    /// already-dirty bytes don't re-fire — the cross is a one-shot
    /// edge, not a level. The `notify_one` permit semantics give us
    /// the "at most one wake per crossing" behaviour even if multiple
    /// writers race; this test pins the in-lock guard (`before <
    /// threshold && after >= threshold`) that prevents re-fires from
    /// writers landing while the buffer is already saturated.
    #[tokio::test]
    async fn additional_writes_past_threshold_do_not_re_notify() {
        let chunk_size = 4096u64;
        let total = chunk_size * 8;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        let (backend, _store, _dir) = build_backend_with_threshold(&manifest, chunk_size * 2).await;
        let notify = backend.threshold_notify();

        // Force a crossing: chunk 0 + chunk 1 → 8 KiB == threshold.
        backend.write(0, &[0xcc; 8]).await.unwrap();
        backend.write(chunk_size, &[0xdd; 8]).await.unwrap();
        let crossed =
            tokio::time::timeout(std::time::Duration::from_millis(100), notify.notified()).await;
        assert!(crossed.is_ok(), "crossing notify must fire");

        // Subsequent writes that materialise more chunks don't
        // re-cross — `before` is already >= threshold for them, so
        // the guard suppresses the notify_one. Permits don't
        // accumulate beyond 1; a fresh `notified()` here would only
        // ever fire if a NEW crossing edge happened.
        backend.write(chunk_size * 2, &[0xee; 8]).await.unwrap();
        backend.write(chunk_size * 3, &[0xff; 8]).await.unwrap();
        let level =
            tokio::time::timeout(std::time::Duration::from_millis(20), notify.notified()).await;
        assert!(
            level.is_err(),
            "notify must not re-fire while the buffer stays past threshold",
        );
    }

    /// Many concurrent writers each materialising a fresh chunk; the
    /// in-lock crossing detection means exactly one of them observes
    /// the monotonic edge (before<threshold && after>=threshold).
    /// The other writers either see before<threshold && after<threshold
    /// (we haven't crossed yet) or before>=threshold (someone else
    /// did). Observable invariant: no deadlock, no panic, notify
    /// fires exactly once (a permit is parked).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_observe_single_crossing() {
        let chunk_size = 4096u64;
        let chunks: u64 = 8;
        let total = chunk_size * chunks;
        let manifest = synth_manifest(total, chunk_size, vec![]);
        // Threshold sized so a single chunk insert does not cross
        // (4 KiB < 16 KiB) but a few of them collectively do
        // (4 KiB * 8 = 32 KiB > 16 KiB).
        let (backend, _store, _dir) = build_backend_with_threshold(&manifest, chunk_size * 4).await;
        let notify = backend.threshold_notify();
        let backend = Arc::new(backend);

        let mut joins = Vec::with_capacity(chunks as usize);
        for i in 0..chunks {
            let backend = backend.clone();
            joins.push(tokio::spawn(async move {
                backend.write(i * chunk_size, &[i as u8; 8]).await.unwrap();
            }));
        }
        for j in joins {
            j.await.unwrap();
        }

        // The collective writes crossed the threshold. At least one
        // writer observed the edge and poked `notify_one`; the
        // permit is parked and consumable.
        let woken =
            tokio::time::timeout(std::time::Duration::from_millis(200), notify.notified()).await;
        assert!(
            woken.is_ok(),
            "concurrent crossing must park a notify permit",
        );

        // A second `notified()` should NOT immediately return —
        // permits don't accumulate. Multiple in-lock observers of
        // the crossing all call `notify_one` but at most one permit
        // is stored.
        let second =
            tokio::time::timeout(std::time::Duration::from_millis(20), notify.notified()).await;
        assert!(
            second.is_err(),
            "notify_one stores at most one permit; a second consumer must wait",
        );
    }

    /// ADR 0018 commit 12m: `InFlightTracker::wait_idle` must return
    /// immediately when count is 0 (no requests outstanding).
    #[tokio::test]
    async fn in_flight_tracker_wait_idle_returns_immediately_when_empty() {
        let tracker = Arc::new(InFlightTracker::new());
        // 50 ms is generous; an idle wait_idle should be sub-µs.
        let res =
            tokio::time::timeout(std::time::Duration::from_millis(50), tracker.wait_idle()).await;
        assert!(
            res.is_ok(),
            "wait_idle on empty tracker must return immediately"
        );
    }

    /// `wait_idle` parks while at least one guard is live, wakes
    /// when the last guard drops. Models the snapshot pipeline's
    /// barrier semantic: pause → wait_idle → flush, where
    /// in-flight writes registered via `enter()` must complete
    /// before flush sees a quiesced dirty buffer.
    #[tokio::test]
    async fn in_flight_tracker_wait_idle_blocks_until_last_guard_drops() {
        let tracker = Arc::new(InFlightTracker::new());
        let g1 = tracker.enter();
        let g2 = tracker.enter();

        // wait_idle should NOT return while guards are held.
        let waiter_tracker = tracker.clone();
        let waiter = tokio::spawn(async move {
            waiter_tracker.wait_idle().await;
        });
        // Give the spawned task a beat to start waiting.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            !waiter.is_finished(),
            "wait_idle returned with 2 guards live"
        );

        // Drop one guard — count is still > 0, waiter still parked.
        drop(g1);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            !waiter.is_finished(),
            "wait_idle returned with 1 guard live"
        );

        // Drop the last guard — waiter must unblock.
        drop(g2);
        let res = tokio::time::timeout(std::time::Duration::from_millis(200), waiter).await;
        assert!(
            res.is_ok(),
            "wait_idle never returned after last guard dropped"
        );
    }

    /// Multiple `wait_idle` callers all wake on the 1→0 edge.
    /// `notify_waiters` is used (not `notify_one`) so any number
    /// of parkers see the drain — important if a future caller
    /// adds a second wait_idle path (admin diagnostic, e.g.).
    #[tokio::test]
    async fn in_flight_tracker_wait_idle_wakes_all_waiters() {
        let tracker = Arc::new(InFlightTracker::new());
        let g = tracker.enter();

        let w1 = tokio::spawn({
            let t = tracker.clone();
            async move { t.wait_idle().await }
        });
        let w2 = tokio::spawn({
            let t = tracker.clone();
            async move { t.wait_idle().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        drop(g);

        let r1 = tokio::time::timeout(std::time::Duration::from_millis(200), w1).await;
        let r2 = tokio::time::timeout(std::time::Duration::from_millis(200), w2).await;
        assert!(r1.is_ok() && r2.is_ok(), "both waiters must wake on drain");
    }

    /// ADR 0039 item #19: `flush_upload` fans its per-chunk puts out
    /// with bounded `buffer_unordered`. Dirty MANY more chunks than
    /// `DISK_FLUSH_UPLOAD_CONCURRENCY` so the parallel path runs across
    /// several windows, then assert the published manifest is correct:
    /// every dirty chunk's bytes are uploaded + retrievable, the
    /// manifest is offset-sorted, and a post-flush read serves the
    /// flushed bytes (proving the rebase saw every parallel put).
    /// ADR 0045 C1: a fenced backend's flush is a no-op (no drain, no
    /// publish — the scheduler skips on chunks_flushed == 0); clearing
    /// the fence flushes the preserved dirty set; the abort re-queue
    /// path round-trips drained chunks back into dirty.
    #[tokio::test]
    async fn migration_fence_noops_flush_and_requeue_restores_dirty() {
        let chunk_size = 4096u64;
        let base = synth_manifest(chunk_size * 4, chunk_size, vec![]);
        let (backend, store, _dir) = build_backend(&base).await;
        backend
            .write(0, &vec![0x42; chunk_size as usize])
            .await
            .unwrap();

        backend.set_migration_fence(true);
        let fenced = backend.flush().await.unwrap();
        assert_eq!(fenced.chunks_flushed, 0, "fenced flush must no-op");
        assert_eq!(fenced.manifest_ref, base_ref_of(&backend).await);

        // The migration path drains explicitly while fenced...
        let pending = backend.flush_local().await.unwrap();
        let (m, hashes) = backend.flush_to_local_cache(&pending).await.unwrap();
        assert_eq!(hashes.len(), 1);
        assert_eq!(m.chunks.len(), 1, "drained chunk in the local manifest");
        assert!(
            store.get_chunk(hashes[0]).await.is_err(),
            "local-cache flush must not PUT to the store"
        );

        // ...and the abort path re-queues + unfences: the next flush
        // publishes the chunk durably.
        backend.requeue_pending(pending).await;
        backend.set_migration_fence(false);
        let after = backend.flush().await.unwrap();
        assert_eq!(
            after.chunks_flushed, 1,
            "re-queued chunk flushes after abort"
        );
        assert!(store.get_chunk(hashes[0]).await.is_ok());
    }

    async fn base_ref_of(b: &ChunkedDiskBackend) -> ManifestRef {
        b.manifest_ref().await
    }

    #[tokio::test]
    async fn flush_upload_parallel_publishes_all_dirty_chunks_sorted() {
        let chunk_size = 4096u64;
        let n_chunks = DISK_FLUSH_UPLOAD_CONCURRENCY * 3 + 5; // 101 chunks
        let total = n_chunks as u64 * chunk_size;
        // Sparse base (no chunks): every write targets a fresh chunk.
        let base = synth_manifest(total, chunk_size, vec![]);
        let (backend, store, _dir) = build_backend(&base).await;

        // Write a distinct non-zero byte into every chunk.
        for c in 0..n_chunks {
            let byte = ((c % 250) + 1) as u8;
            backend
                .write(c as u64 * chunk_size, &vec![byte; chunk_size as usize])
                .await
                .unwrap();
        }

        let outcome = backend.flush().await.unwrap();
        assert_eq!(outcome.chunks_flushed, n_chunks);
        assert_eq!(outcome.bytes_uploaded, total);

        let published = store.get_manifest(outcome.manifest_ref).await.unwrap();
        assert_eq!(published.chunks.len(), n_chunks);
        let offsets: Vec<u64> = published.chunks.iter().map(|c| c.offset).collect();
        let mut sorted = offsets.clone();
        sorted.sort_unstable();
        assert_eq!(offsets, sorted, "published manifest must be offset-sorted");

        // Every published chunk's bytes are durable in the store, and a
        // spot-check read serves the flushed (not stale-base) bytes.
        for c in published.chunks.iter() {
            store.get_chunk(c.hash).await.unwrap();
        }
        let mid = (n_chunks / 2) as u64;
        let got = backend.read(mid * chunk_size, 16).await.unwrap();
        assert_eq!(got, vec![((mid as usize % 250) + 1) as u8; 16]);
    }

    /// A `BlobStorage` that fails every PUT while `fail` is set and
    /// delegates everything else to a real local backend. Used to drive
    /// the flush-atomicity test.
    struct FlakyPutBlob {
        inner: LocalBlobStorage,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl BlobStorage for FlakyPutBlob {
        async fn put_streaming(
            &self,
            key: &str,
            body: ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(engram_core::error::BlobError::Protocol(
                    "injected put failure (test)".into(),
                ));
            }
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<ByteStream, engram_core::error::BlobError> {
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), engram_core::error::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(
            &self,
            prefix: &str,
        ) -> Result<Vec<String>, engram_core::error::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    /// Issue #199 test harness: a `BlobStorage` whose FIRST chunk PUT
    /// parks until `release` fires, signalling `at_gate` once it has
    /// arrived. Lets a test freeze one flush mid-upload (after its drain,
    /// before its publish) while a second, newer flush races — proving
    /// the flush pipeline serializes drain→upload→publish→rebase so the
    /// newest-drained chunk always wins. Only `chunks/...` PUTs gate;
    /// manifest PUTs pass straight through so the un-gated flush can
    /// publish normally.
    struct GatedChunkPutBlob {
        inner: LocalBlobStorage,
        gate_armed: Arc<std::sync::atomic::AtomicBool>,
        at_gate: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl BlobStorage for GatedChunkPutBlob {
        async fn put_streaming(
            &self,
            key: &str,
            body: ByteStream,
        ) -> Result<u64, engram_core::error::BlobError> {
            // Gate exactly the first chunk PUT we see while armed.
            if key.starts_with("chunks/")
                && self
                    .gate_armed
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.at_gate.notify_one();
                self.release.notified().await;
            }
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<ByteStream, engram_core::error::BlobError> {
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), engram_core::error::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(
            &self,
            prefix: &str,
        ) -> Result<Vec<String>, engram_core::error::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    /// Issue #199 regression: two flushes that touch the SAME chunk must
    /// never rebase/publish out of order. We freeze flush A mid-upload
    /// (after it drained `w1`, before its publish), write `w2` into the
    /// same chunk, then run flush B — and prove the newest-drained bytes
    /// (`w1+w2`) win both the live read AND the highest published
    /// manifest version, with NO dropped write.
    ///
    /// Before the fix, A held no lock past its drain: B would drain `w2`
    /// concurrently, publish + rebase `base[X]=hB` first, then A's slow
    /// upload would finish LAST, overwrite the manifest entry + `base[X]`
    /// with its own older `hA`, and bump PAST B's version — silently
    /// rolling back the acked `w2` write (the #191 corruption class). The
    /// flush-pipeline mutex serializes the whole drain→upload→rebase, so
    /// the later drain's data is the one that survives.
    #[tokio::test]
    async fn concurrent_flushes_never_rebase_a_chunk_out_of_order() {
        use std::sync::atomic::AtomicBool;
        let chunk_size = 4096u64;
        let total = 4 * chunk_size;
        let base = synth_manifest(total, chunk_size, vec![]);

        let dir = tempfile::tempdir().unwrap();
        let gate_armed = Arc::new(AtomicBool::new(true));
        let at_gate = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let blob: Arc<dyn BlobStorage> = Arc::new(GatedChunkPutBlob {
            inner: LocalBlobStorage::new(dir.path().to_path_buf()),
            gate_armed: gate_armed.clone(),
            at_gate: at_gate.clone(),
            release: release.clone(),
        });
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &base).await.unwrap();
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &base, cache, store.clone(), u64::MAX).unwrap(),
        );

        // Chunk 0: w1 (first half = 0x11, rest = 0x00).
        let mut w1 = vec![0u8; chunk_size as usize];
        for b in w1.iter_mut().take(chunk_size as usize / 2) {
            *b = 0x11;
        }
        backend.write(0, &w1).await.unwrap();

        // Flush A: drains w1, then parks at the gated chunk PUT — still
        // holding the flush-pipeline guard it took in flush_local.
        let a = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.flush().await })
        };
        // Wait until A is parked at the gate (drain done, publish pending).
        at_gate.notified().await;

        // Guest writes w2 into the SAME chunk while A is mid-upload. With
        // FC-style RMW the dirty buffer now holds the full w1+w2 chunk.
        let mut w2_full = w1.clone();
        for b in w2_full
            .iter_mut()
            .skip(chunk_size as usize / 2)
            .take(chunk_size as usize / 2)
        {
            *b = 0x22;
        }
        backend
            .write(chunk_size / 2, &vec![0x22; chunk_size as usize / 2])
            .await
            .unwrap();

        // Flush B: with the fix its flush_local blocks on the pipeline
        // guard A holds, so it cannot drain/publish until A is done.
        let b = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.flush().await })
        };
        // Give B a beat to reach (and, with the fix, block on) the guard.
        tokio::task::yield_now().await;

        // Let A finish: it publishes hA, rebases base[0]=hA, releases the
        // guard; B then drains w1+w2, uploads hB, publishes, rebases.
        release.notify_one();
        let _ = a.await.unwrap().unwrap();
        let _ = b.await.unwrap().unwrap();

        // (1) Live read of chunk 0 reflects the NEWEST drained data
        //     (w1+w2) — never the stale w1-only A rebase.
        let got = backend.read(0, chunk_size).await.unwrap();
        assert_eq!(
            &got[..],
            &w2_full[..],
            "live read must reflect the newest write (w1+w2), not a stale older flush's rebase",
        );

        // (2) The HIGHEST published manifest version maps chunk 0 to the
        //     w1+w2 hash — a resume restoring `latest` is not behind the
        //     guest's acked writes.
        let (_latest_ref, latest) = store
            .get_latest_manifest(manifest_ref.manifest_id)
            .await
            .unwrap()
            .expect("a manifest must have been published");
        let entry = latest
            .chunks
            .iter()
            .find(|c| c.offset == 0)
            .expect("chunk 0 must be present in the latest published manifest");
        assert_eq!(
            entry.hash,
            ChunkHash::of(&w2_full),
            "latest published manifest must map chunk 0 to the newest (w1+w2) bytes",
        );
        // And those bytes are durable.
        assert_eq!(store.get_chunk(entry.hash).await.unwrap(), w2_full);
    }

    /// Issue #199 fence re-check: a flush already past `flush()`'s
    /// top-of-function fence check must NOT publish if the migration
    /// fence is raised mid-upload — otherwise it republishes a newer
    /// `live_disk_manifest` after the migration's coherence cut
    /// (split-brain). We park flush A at its chunk PUT, raise the fence,
    /// release A, and assert A aborts the publish (manifest unchanged,
    /// chunks_flushed == 0) and re-queues the drained chunk to dirty.
    #[tokio::test]
    async fn flush_upload_aborts_publish_when_fence_raised_mid_upload() {
        use std::sync::atomic::AtomicBool;
        let chunk_size = 4096u64;
        let total = 4 * chunk_size;
        let base = synth_manifest(total, chunk_size, vec![]);

        let dir = tempfile::tempdir().unwrap();
        let gate_armed = Arc::new(AtomicBool::new(true));
        let at_gate = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let blob: Arc<dyn BlobStorage> = Arc::new(GatedChunkPutBlob {
            inner: LocalBlobStorage::new(dir.path().to_path_buf()),
            gate_armed: gate_armed.clone(),
            at_gate: at_gate.clone(),
            release: release.clone(),
        });
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &base).await.unwrap();
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &base, cache, store.clone(), u64::MAX).unwrap(),
        );
        let ref0 = backend.manifest_ref().await;

        backend
            .write(0, &vec![0x42; chunk_size as usize])
            .await
            .unwrap();

        // Flush A: passes the top-of-flush() fence check (fence is
        // clear), drains, then parks at the gated chunk PUT.
        let a = {
            let backend = backend.clone();
            tokio::spawn(async move { backend.flush().await })
        };
        at_gate.notified().await;

        // The migration coherence cut lands while A is mid-upload.
        backend.set_migration_fence(true);

        // Release A: its upload completes, but the in-publish fence
        // re-check must abort BEFORE put_manifest.
        release.notify_one();
        let outcome = a.await.unwrap().unwrap();
        assert_eq!(
            outcome.chunks_flushed, 0,
            "a flush whose upload finishes after the fence is raised must NOT publish",
        );
        assert_eq!(
            backend.manifest_ref().await,
            ref0,
            "the manifest must not advance past the migration coherence cut",
        );
        // The drained chunk is re-queued for the post-migration retry.
        assert!(
            backend.dirty_chunks_count().await >= 1,
            "the aborted flush must re-queue its drained chunk to dirty",
        );
    }

    /// ADR 0038: a failed `flush_upload` must be an ATOMIC no-op — the
    /// manifest is NOT advanced and the drained bytes are re-queued to
    /// `dirty` so the next checkpoint retries them. Regression test for the
    /// prod chunk-drop (session `79b689e4`): the old shared-`pending_uploads`
    /// lookup silently skipped puts, publishing a manifest that referenced
    /// never-uploaded chunks → guest EIO on read.
    #[tokio::test]
    async fn flush_upload_failure_is_atomic_no_op_and_retains_bytes() {
        use std::sync::atomic::Ordering;
        let chunk_size = 4096u64;
        let total = 4 * chunk_size;
        let base = synth_manifest(total, chunk_size, vec![]);

        let dir = tempfile::tempdir().unwrap();
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let blob: Arc<dyn BlobStorage> = Arc::new(FlakyPutBlob {
            inner: LocalBlobStorage::new(dir.path().to_path_buf()),
            fail: fail.clone(),
        });
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &base).await.unwrap();
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &base, cache, store.clone(), u64::MAX).unwrap();

        let ref0 = backend.manifest_ref().await;

        // Dirty three chunks.
        for c in 0..3u64 {
            backend
                .write(c * chunk_size, &vec![(c as u8) + 1; chunk_size as usize])
                .await
                .unwrap();
        }

        // Uploads fail → the flush must error, atomically.
        fail.store(true, Ordering::SeqCst);
        assert!(
            backend.flush().await.is_err(),
            "flush must fail when an upload fails"
        );

        // (1) manifest NOT advanced — no half-written snapshot.
        assert_eq!(
            backend.manifest_ref().await,
            ref0,
            "failed flush must not tick the manifest"
        );
        // (2) the drained bytes are re-queued for retry — not lost.
        assert!(
            backend.dirty_chunks_count().await >= 3,
            "drained bytes must be re-queued to dirty on a failed upload"
        );
        // (3) reads still serve the written bytes locally — never EIO/stale.
        assert_eq!(backend.read(chunk_size, 16).await.unwrap(), vec![2u8; 16]);

        // Recover: uploads succeed → the retry flushes cleanly + durably.
        fail.store(false, Ordering::SeqCst);
        let outcome = backend.flush().await.unwrap();
        assert_ne!(
            outcome.manifest_ref, ref0,
            "a successful retry must advance the manifest"
        );
        let published = store.get_manifest(outcome.manifest_ref).await.unwrap();
        for c in published.chunks.iter() {
            store
                .get_chunk(c.hash)
                .await
                .expect("every published chunk must be durable after a successful flush");
        }
        assert_eq!(backend.read(chunk_size, 16).await.unwrap(), vec![2u8; 16]);
    }

    /// ADR 0039 (sticky-everywhere): a flush write-throughs each uploaded
    /// chunk into the local cache, so the flushing host keeps its own
    /// chunks resident on NVMe instead of re-fetching its writes from GCS.
    #[tokio::test]
    async fn flush_write_throughs_chunks_into_local_cache() {
        let chunk_size = 4096u64;
        let total = 3 * chunk_size;
        let base = synth_manifest(total, chunk_size, vec![]);

        // A near-full dev/CI disk (e.g. the dev VM at >90% used) trips the
        // cache's default 20%-free-space floor and evicts the just-flushed
        // chunk before this test can observe it, independent of `budget_bytes`
        // — see the same fix in two_host_drain_wave.rs / migration_source.rs.
        // Nextest runs each test in its own process, so this env override
        // is safe.
        std::env::set_var("ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT", "0.01");

        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &base).await.unwrap();
        // Hold a clone of the cache so we can assert the write-through.
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &base, cache.clone(), store.clone(), u64::MAX)
                .unwrap();

        backend
            .write(chunk_size, &vec![0x5a; chunk_size as usize])
            .await
            .unwrap();
        let outcome = backend.flush().await.unwrap();
        assert_eq!(outcome.chunks_flushed, 1);

        // The flushed chunk is now resident in the LOCAL cache (write-through),
        // not only in GCS — so a same-host read never round-trips to blob.
        let published = store.get_manifest(outcome.manifest_ref).await.unwrap();
        let h = published
            .chunks
            .iter()
            .find(|c| c.offset == chunk_size)
            .expect("flushed chunk in manifest")
            .hash;
        assert!(
            cache.contains_on_disk(h),
            "flush must write-through the uploaded chunk into the local cache",
        );
    }

    // === ADR 0045 C2 disk post-copy ===

    /// Per-chunk-distinct synthetic peer. `fail` makes every fetch a
    /// terminal error (the synthetic peer death).
    struct FakeFetcher {
        calls: std::sync::atomic::AtomicUsize,
        chunk_size: usize,
        fail: bool,
    }

    impl FakeFetcher {
        fn new(chunk_size: usize, fail: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                chunk_size,
                fail,
            })
        }

        fn peer_byte(chunk_idx: u64) -> u8 {
            0x70 + chunk_idx as u8
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl PostCopyDiskFetcher for FakeFetcher {
        fn fetch(&self, chunk_idx: u64) -> futures::future::BoxFuture<'_, Result<Bytes, String>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.fail {
                    return Err("synthetic peer death".into());
                }
                Ok(Bytes::from(vec![
                    Self::peer_byte(chunk_idx);
                    self.chunk_size
                ]))
            })
        }
    }

    /// Source seal: covers BOTH tiers (dirty + pending), drains dirty
    /// without hashing, leaves pending in place, and the shipped
    /// manifest is base-only (every hash durable) — `guest content ==
    /// manifest ⊕ seal`. Dirty wins over pending on the same index.
    #[tokio::test]
    async fn seal_covers_dirty_and_pending_dirty_wins_and_manifest_is_base_only() {
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let chunk_size = 4096u64;
        let total = 3 * chunk_size;
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let h1 = put_chunk(&store, 0xbb, chunk_size as usize).await;
        let h2 = put_chunk(&store, 0xcc, chunk_size as usize).await;
        let manifest = synth_manifest(
            total,
            chunk_size,
            vec![(0, h0), (chunk_size, h1), (2 * chunk_size, h2)],
        );
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        // Chunk 1: dirty -> pending (flush window). Chunk 0: dirty.
        // Chunk 1 ALSO re-dirtied after the drain — dirty must win.
        backend
            .write(chunk_size, &vec![0x11; chunk_size as usize])
            .await
            .unwrap();
        let _pending = backend.flush_local().await.unwrap();
        backend
            .write(0, &vec![0x22; chunk_size as usize])
            .await
            .unwrap();
        backend.write(chunk_size, &[0x33; 16]).await.unwrap();

        let (seal, base_manifest, base_ref) = backend.seal_for_postcopy().await;
        assert_eq!(
            seal.indices(),
            vec![0, 1],
            "dirty(0) + dirty-over-pending(1)"
        );
        assert_eq!(base_ref, manifest_ref, "the published ref, no mint");
        let sealed1 = seal.get(1).unwrap();
        assert!(
            sealed1[..16].iter().all(|b| *b == 0x33),
            "dirty wins over pending"
        );
        assert!(sealed1[16..].iter().all(|b| *b == 0x11));
        assert!(seal.get(0).unwrap().iter().all(|b| *b == 0x22));
        assert!(seal.get(2).is_none(), "clean chunk not sealed");
        // Manifest = base only: idx 1 still references the ORIGINAL
        // durable hash (the divergence rides the seal, not a publish).
        let at = |off: u64| {
            base_manifest
                .chunks
                .iter()
                .find(|c| c.offset == off)
                .unwrap()
                .hash
        };
        assert_eq!(at(0), h0);
        assert_eq!(at(chunk_size), h1);
        assert_eq!(at(2 * chunk_size), h2);
        // The seal drained `dirty` (no copies left behind).
        assert_eq!(backend.dirty_chunks_count().await, 0);

        // Abort path: requeue restores readability of the sealed bytes.
        backend.requeue_postcopy_seal(&seal).await;
        let bytes = backend.read(0, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == 0x22));
        let bytes = backend.read(chunk_size, 16).await.unwrap();
        assert!(bytes.iter().all(|b| *b == 0x33));
    }

    /// Destination overlay: install rebases to the source's manifest,
    /// a sealed read demand-fetches EXACTLY once (installed into
    /// `dirty`), and non-sealed reads bypass the fetcher entirely.
    #[tokio::test]
    async fn overlay_read_demand_fetches_once_and_nonsealed_bypasses() {
        let chunk_size = 4096u64;
        let total = 2 * chunk_size;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let h1 = put_chunk(&store, 0xbb, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0), (chunk_size, h1)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        let fetcher = FakeFetcher::new(chunk_size as usize, false);
        let _sub = backend
            .install_postcopy_overlay(&manifest, manifest_ref, &[1], fetcher.clone())
            .await
            .unwrap();

        // Non-sealed chunk: straight from base, fetcher untouched.
        let bytes = backend.read(0, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == 0xaa));
        assert_eq!(fetcher.calls(), 0);

        // Sealed chunk: peer-authoritative, fetched once, then served
        // from `dirty`.
        let bytes = backend.read(chunk_size, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == FakeFetcher::peer_byte(1)));
        assert_eq!(fetcher.calls(), 1);
        let bytes = backend.read(chunk_size, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == FakeFetcher::peer_byte(1)));
        assert_eq!(fetcher.calls(), 1, "second read must not re-fetch");
    }

    /// A partial write to a sealed chunk must RMW against the PEER
    /// content — a base RMW would resurrect pre-divergence bytes
    /// (the disk flavor of the flush-window corruption).
    #[tokio::test]
    async fn overlay_partial_write_rmws_peer_content_not_base() {
        let chunk_size = 4096u64;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(chunk_size, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        let fetcher = FakeFetcher::new(chunk_size as usize, false);
        let _sub = backend
            .install_postcopy_overlay(&manifest, manifest_ref, &[0], fetcher.clone())
            .await
            .unwrap();

        backend.write(0, &[0x55; 16]).await.unwrap();
        assert_eq!(
            fetcher.calls(),
            1,
            "the RMW must have materialized the peer content"
        );
        let bytes = backend.read(0, chunk_size).await.unwrap();
        assert!(bytes[..16].iter().all(|b| *b == 0x55));
        assert!(
            bytes[16..].iter().all(|b| *b == FakeFetcher::peer_byte(0)),
            "the unwritten remainder must be the PEER content, not base 0xaa",
        );
    }

    /// A terminal fetch failure latches `lost`: the failing read
    /// errors (bounded -> NBD EIO, never a hang) and subsequent sealed
    /// reads fail fast WITHOUT re-dialing the dead peer.
    #[tokio::test]
    async fn overlay_fetch_failure_latches_lost_and_fails_fast() {
        let chunk_size = 4096u64;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(2 * chunk_size, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend =
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap();

        let fetcher = FakeFetcher::new(chunk_size as usize, true);
        let _sub = backend
            .install_postcopy_overlay(&manifest, manifest_ref, &[0, 1], fetcher.clone())
            .await
            .unwrap();

        assert!(backend.read(0, chunk_size).await.is_err());
        assert_eq!(fetcher.calls(), 1);
        assert!(backend.read(chunk_size, chunk_size).await.is_err());
        assert_eq!(fetcher.calls(), 1, "lost is latched; no re-dial per read");
    }

    /// The background drain pulls every sealed chunk, unfences the
    /// publisher, clears the overlay, and reports the count. After it,
    /// reads serve locally with zero peer traffic.
    #[tokio::test]
    async fn postcopy_drain_installs_all_unfences_and_reports() {
        let chunk_size = 4096u64;
        let total = 3 * chunk_size;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(total, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap(),
        );
        backend.set_migration_fence(true);

        let fetcher = FakeFetcher::new(chunk_size as usize, false);
        let mut sub = backend
            .install_postcopy_overlay(&manifest, manifest_ref, &[1, 2], fetcher.clone())
            .await
            .unwrap();
        backend.clone().spawn_postcopy_drain();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(r) = sub.borrow_and_update().clone() {
                    return r;
                }
                if sub.changed().await.is_err() {
                    return sub.borrow().clone().expect("terminal value");
                }
            }
        })
        .await
        .expect("drain must terminate");
        assert_eq!(outcome, Ok(2));

        // Self-sufficient: fence released, overlay gone, content local.
        assert!(
            backend.postcopy_drain_subscribe().is_none(),
            "overlay cleared"
        );
        let calls_after_drain = fetcher.calls();
        let bytes = backend.read(chunk_size, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == FakeFetcher::peer_byte(1)));
        let bytes = backend.read(2 * chunk_size, chunk_size).await.unwrap();
        assert!(bytes.iter().all(|b| *b == FakeFetcher::peer_byte(2)));
        assert_eq!(
            fetcher.calls(),
            calls_after_drain,
            "no peer traffic after the drain"
        );
        // The fence is released: a flush now publishes the pulled
        // divergence (the durability catch-up on the normal cadence).
        let outcome = backend.flush().await.unwrap();
        assert_eq!(
            outcome.chunks_flushed, 2,
            "post-drain flush publishes the pulled chunks"
        );
    }

    /// Drain against a dead peer reports the failure (the coordinator
    /// turns it into PeerLost and rewinds); the overlay stays lost.
    #[tokio::test]
    async fn postcopy_drain_peer_death_reports_error() {
        let chunk_size = 4096u64;
        let dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(dir.path().to_path_buf()));
        let store = Arc::new(ChunkStore::new(blob));
        let h0 = put_chunk(&store, 0xaa, chunk_size as usize).await;
        let manifest = synth_manifest(2 * chunk_size, chunk_size, vec![(0, h0)]);
        let manifest_ref = ManifestRef::new();
        store.put_manifest(manifest_ref, &manifest).await.unwrap();
        let mut cfg = ChunkCacheConfig::new(dir.path().join("cache"));
        cfg.budget_bytes = 64 * 1024 * 1024;
        let cache = ChunkCache::new(cfg);
        let backend = Arc::new(
            ChunkedDiskBackend::new(manifest_ref, &manifest, cache, store, u64::MAX).unwrap(),
        );
        backend.set_migration_fence(true);

        let fetcher = FakeFetcher::new(chunk_size as usize, true);
        let mut sub = backend
            .install_postcopy_overlay(&manifest, manifest_ref, &[0, 1], fetcher.clone())
            .await
            .unwrap();
        backend.clone().spawn_postcopy_drain();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(r) = sub.borrow_and_update().clone() {
                    return r;
                }
                if sub.changed().await.is_err() {
                    return sub.borrow().clone().expect("terminal value");
                }
            }
        })
        .await
        .expect("drain must terminate");
        assert!(outcome.is_err(), "dead peer must surface as a drain error");
        assert!(
            backend.postcopy_drain_subscribe().is_some(),
            "the lost overlay stays for drain_wait to read",
        );
    }
}
