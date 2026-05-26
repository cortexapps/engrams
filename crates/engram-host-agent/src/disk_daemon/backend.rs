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

use std::collections::HashMap;
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
    /// `chunk_idx -> dirty bytes`. Locked together so a concurrent
    /// read + write of the same chunk doesn't see torn state.
    dirty: Arc<Mutex<HashMap<usize, Vec<u8>>>>,
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
    threshold_bytes: u64,
    /// ADR 0018 commit 12m: in-flight request counter for the NBD
    /// daemon's serve loop. Incremented on entering a write/read
    /// handler, decremented (with a `Notify::notify_waiters` on the
    /// 1→0 edge) on exit. `wait_idle()` parks on the notify until
    /// the count reads 0, giving the snapshot pipeline a barrier
    /// to drain in-flight virtio writes after FC has paused. Reads
    /// participate too (cheap, and lets future barriers cover them).
    in_flight: Arc<InFlightTracker>,
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
    manifest_ref: ManifestRef,
    base: PositionalDiskManifest,
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
        let base = PositionalDiskManifest::from_manifest(&manifest)?;
        let chunk_size = base.chunk_size;
        let total_bytes = base.total_bytes;
        Ok(Self {
            state: Arc::new(Mutex::new(BackendState { manifest_ref, base })),
            chunk_size,
            total_bytes,
            cache,
            store,
            dirty: Arc::new(Mutex::new(HashMap::new())),
            last_flush_unix_ms: Arc::new(AtomicI64::new(0)),
            threshold_notify: Arc::new(Notify::new()),
            threshold_bytes,
            in_flight: Arc::new(InFlightTracker::new()),
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
            state: Arc::new(Mutex::new(BackendState { manifest_ref, base })),
            chunk_size,
            total_bytes,
            cache,
            store,
            dirty: Arc::new(Mutex::new(HashMap::new())),
            last_flush_unix_ms: Arc::new(AtomicI64::new(0)),
            threshold_notify: Arc::new(Notify::new()),
            threshold_bytes,
            in_flight: Arc::new(InFlightTracker::new()),
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
        let mut out = Vec::with_capacity(length as usize);
        let chunk_size = self.chunk_size;
        let mut cursor = offset;
        let end = offset + length;
        while cursor < end {
            let chunk_idx = (cursor / chunk_size) as usize;
            let chunk_start = (chunk_idx as u64) * chunk_size;
            let chunk_end = std::cmp::min(chunk_start + chunk_size, self.total_bytes);
            let intra = (cursor - chunk_start) as usize;
            let take = std::cmp::min(end, chunk_end) - cursor;
            let chunk_bytes = self.read_chunk(chunk_idx, chunk_end - chunk_start).await?;
            out.extend_from_slice(&chunk_bytes[intra..intra + take as usize]);
            cursor += take;
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
        // Cheap pre-check: if the chunk is already in dirty, skip
        // the base fetch entirely. Holding the dirty lock briefly
        // here is fine — a HashMap::contains_key is constant-time.
        let already_present = self.dirty.lock().await.contains_key(&chunk_idx);
        let prefetched_base: Option<Vec<u8>> = if already_present {
            None
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
    pub async fn flush(&self) -> Result<DiskFlushOutcome, DiskBackendError> {
        let mut dirty_guard = self.dirty.lock().await;
        let chunk_size = self.chunk_size;
        if dirty_guard.is_empty() {
            let out = DiskFlushOutcome {
                manifest_ref: self.state.lock().await.manifest_ref,
                chunks_flushed: 0,
                bytes_uploaded: 0,
            };
            self.stamp_flush_completion();
            return Ok(out);
        }
        let mut new_chunks: Vec<(usize, ChunkHash, u64)> = Vec::new();
        // Drain into a Vec so we can release the lock while
        // uploading (uploads are async + bounded by network
        // latency; holding the lock would serialise unrelated
        // reads).
        let drained: Vec<(usize, Vec<u8>)> = dirty_guard.drain().collect();
        drop(dirty_guard);
        for (chunk_idx, bytes) in drained {
            let size = bytes.len() as u64;
            let hash = self.store.put_chunk(&bytes).await?;
            new_chunks.push((chunk_idx, hash, size));
        }

        // Now atomically: rebuild the manifest from the (locked)
        // current base + the just-uploaded hashes, publish to the
        // store, and rebase `state` so future reads serve from the
        // new hashes. Holding the state lock across the put_manifest
        // call is OK — it's a single PUT, bounded by network
        // latency, and reads against this backend during flush are
        // already in-flight or waiting on the dirty lock anyway.
        let mut state = self.state.lock().await;
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
        for (idx, hash, size) in &new_chunks {
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
            bytes_uploaded += *size;
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
        let mut attempt_ref = state.manifest_ref.next_version();
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
        drop(state);

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
        {
            let dirty = self.dirty.lock().await;
            if let Some(buf) = dirty.get(&chunk_idx) {
                return Ok(Bytes::copy_from_slice(buf));
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
            Some(hash) => Ok(self.cache.get(hash, || self.store.get_chunk(hash)).await?),
            // Zero-filled hole — the manifest had no entry here.
            None => Ok(Bytes::from(vec![0u8; chunk_len as usize])),
        }
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
    use engram_core::traits::BlobStorage;
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
}
